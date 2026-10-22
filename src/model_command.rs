//! `/model`: the agents' models while Arko runs.
//!
//! `/model [MODEL] [EFFORT]` is the main agent's, and only before the
//! conversation starts: once it has, the main agent keeps its model for the
//! conversation (its saved configuration can't change). `/model worker
//! [MODEL] [EFFORT]` is for the workers started from now on, at any time,
//! and `/model worker main` makes them follow the main agent's model again.
//! Before the start a change rebuilds the agents (their configurations,
//! the workflow, the handover size); after it, new workers start on the
//! model through the subagents' start hook (see `crew::WorkerModels`),
//! and the workers already started keep theirs.

use arko_ai::{Model, Provider};
use arko_workflows::notes::{Note, NoteSource};

use crate::agents::{self, AgentSettings, Effort, ExtraRules, MAIN, ModelSettings};
use crate::crew::{NewWorkers, WorkerSetup};
use crate::providers::{self, OpenAiAuth};
use crate::remote::{self, Level, Source};
use crate::session::Settings;
use crate::{Conversation, Harness, index, lock};

/// What `/cc-compact` says in a session with no agent in Claude Code.
const CC_NOT_IN_CLAUDE_CODE: &str = "The agents aren't in Claude Code, so they hand over at the handover size and /cc-compact changes nothing in this session.";

/// Whose model `/model` changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Main,
    Workers,
}

/// What `/model` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// The model now, and how to change it.
    Show(Target),
    /// The workers follow the main agent's model again.
    FollowMain,
    /// `target` runs `model`, at `effort` (or the nearest it takes to the
    /// effort now).
    Set {
        target: Target,
        model: String,
        effort: Option<Effort>,
    },
}

/// `/model`'s words: `[MODEL] [EFFORT]`, or `worker` first.
pub fn parse(argument: &str) -> Result<Request, String> {
    let words: Vec<&str> = argument.split_whitespace().collect();
    let (target, rest) = match words.split_first() {
        Some((first, rest)) if matches!(*first, "worker" | "workers") => (Target::Workers, rest),
        _ => (Target::Main, words.as_slice()),
    };
    match rest {
        [] => Ok(Request::Show(target)),
        ["main"] if target == Target::Workers => Ok(Request::FollowMain),
        [model] => Ok(Request::Set {
            target,
            model: model.to_string(),
            effort: None,
        }),
        [model, effort] => Ok(Request::Set {
            target,
            model: model.to_string(),
            effort: Some(providers::parse_effort(effort)?),
        }),
        _ => Err(usage(target).to_string()),
    }
}

/// How `/model` is used, for `target`.
pub fn usage(target: Target) -> &'static str {
    match target {
        Target::Main => {
            "/model MODEL [EFFORT] sets the main agent's model before the conversation starts (/model sol high). /model worker MODEL [EFFORT] sets the workers'."
        }
        Target::Workers => {
            "/model worker MODEL [EFFORT] sets the model of the workers started from now on (/model worker fable high). /model worker main makes them follow the main agent's model."
        }
    }
}

/// `model`'s settings as Arko says them: `gpt-6.1-sol (high effort)`.
fn shown(model: &ModelSettings) -> String {
    format!("{} ({} effort)", model.model, model.effort)
}

impl Harness {
    /// `/model`, typed at the prompt or sent from the page: says or changes
    /// an agent's model, in a plain line.
    pub(crate) fn model_command(&mut self, argument: &str, source: Source) {
        let before = (
            self.settings.main.clone(),
            self.settings.worker.clone(),
            self.settings.own_worker_model,
        );
        let result = parse(argument).and_then(|request| self.model_request(request));
        let changed = before
            != (
                self.settings.main.clone(),
                self.settings.worker.clone(),
                self.settings.own_worker_model,
            );
        self.setting_reply(result, source, changed);
    }

    fn reply_settings(&self) -> (bool, bool, ExtraRules) {
        (
            self.settings.text_replies,
            self.settings.cc_handover,
            self.settings.extra_rules,
        )
    }

    pub(crate) fn setting_reply(
        &mut self,
        result: Result<String, String>,
        source: Source,
        changed: bool,
    ) {
        match result {
            Ok(text) => {
                if source == Source::Web {
                    let origin = if changed {
                        "Changed from the web page."
                    } else {
                        "From the web page."
                    };
                    self.say(Level::Info, &format!("{origin} {text}"));
                } else {
                    self.mirrored(|harness| harness.output.line(&text));
                }
            }
            Err(error) => {
                self.mirrored(|harness| harness.output.line(&harness.output.error(&error)))
            }
        }
    }

    fn model_request(&mut self, request: Request) -> Result<String, String> {
        let started = self.conversation_started();
        match request {
            Request::Show(Target::Main) => Ok(if started {
                format!(
                    "The main agent keeps {} for this session. /model worker sets the model for workers started from now on.",
                    shown(&self.settings.main)
                )
            } else {
                format!(
                    "The main agent runs {}. {}",
                    shown(&self.settings.main),
                    usage(Target::Main)
                )
            }),
            Request::Show(Target::Workers) => Ok(format!(
                "{} {}",
                self.workers_line(),
                usage(Target::Workers)
            )),
            Request::Set {
                target: Target::Main,
                ..
            } if started => Ok(format!(
                "The main agent keeps {} for this session. /model worker sets the model for workers started from now on.",
                shown(&self.settings.main)
            )),
            Request::Set {
                target,
                model,
                effort,
            } => {
                let resolved = providers::resolve_model(&model)?;
                self.can_reach(&resolved)?;
                let current = match target {
                    Target::Main => &self.settings.main,
                    Target::Workers => &self.settings.worker,
                };
                let effort = match effort {
                    Some(effort) if providers::efforts(&resolved).is_empty() => effort,
                    Some(effort) if !providers::efforts(&resolved).contains(&effort) => {
                        return Err(format!(
                            "{} takes {} effort, not {effort}.",
                            providers::model_name(&resolved),
                            providers::efforts_text(&resolved)
                        ));
                    }
                    Some(effort) => effort,
                    None => providers::fit_effort(&resolved, current.effort),
                };
                match target {
                    Target::Main => self.set_main_model(&resolved, effort),
                    Target::Workers => self.set_workers_model(Some((&resolved, effort))),
                }
            }
            Request::FollowMain => self.set_workers_model(None),
        }
    }

    /// `/text-replies`, typed at the prompt, sent from the page or chosen
    /// in the settings: says how the main agent ends its turns, and with
    /// `on` or `off` changes it before the conversation starts (the agents
    /// are built again). Once it has started, the main agent keeps it for
    /// the session, as it keeps its model.
    pub(crate) fn text_replies_command(&mut self, argument: &str, source: Source) {
        let before = self.reply_settings();
        let result = match argument {
            "" => Ok(self.text_replies_line()),
            "on" => self.set_text_replies(true),
            "off" => self.set_text_replies(false),
            _ => Err("/text-replies takes on or off (/text-replies off).".to_string()),
        };
        let changed = before != self.reply_settings();
        self.setting_reply(result, source, changed);
    }

    /// How the main agent ends its turns, as `/text-replies` says it.
    fn text_replies_line(&self) -> String {
        let on = self.settings.text_replies;
        let (state, other) = if on { ("on", "off") } else { ("off", "on") };
        let mode = agents::replies_text(on);
        if self.conversation_started() {
            format!("Text replies are {state}, so {mode}. It keeps this for the session.")
        } else {
            format!(
                "Text replies are {state}, so {mode}. /text-replies {other} changes that before the conversation starts."
            )
        }
    }

    /// Text replies on or off, before the conversation starts: the agents
    /// are built again with them.
    fn set_text_replies(&mut self, on: bool) -> Result<String, String> {
        let state = if on { "on" } else { "off" };
        let mode = agents::replies_text(on);
        if self.settings.text_replies == on {
            return Ok(format!("Text replies are {state} already, so {mode}."));
        }
        if self.conversation_started() {
            let kept = if on { "off" } else { "on" };
            return Ok(format!(
                "The main agent keeps text replies {kept} for this session, so {}. A new session takes --text-replies or --no-text-replies.",
                agents::replies_text(!on)
            ));
        }
        let mut settings = self.settings.clone();
        settings.text_replies = on;
        self.rebuild_agents(settings)?;
        Ok(format!("Text replies are {state}, so {mode}."))
    }

    /// `/cc-compact`, typed at the prompt, sent from the page or chosen in
    /// the settings: says what the agents on `cc:` models do at the handover
    /// size, and with `on` (compact) or `off` (hand over) changes that
    /// before the conversation starts (the agents are built again). Once it
    /// has started, the session keeps it: it is part of the agents' saved
    /// configurations.
    pub(crate) fn cc_compact_command(&mut self, argument: &str, source: Source) {
        let before = self.reply_settings();
        let result = match argument {
            "" => Ok(self.cc_compact_line()),
            "on" => self.set_cc_compact(true),
            "off" => self.set_cc_compact(false),
            _ => Err("/cc-compact takes on or off (/cc-compact off).".to_string()),
        };
        let changed = before != self.reply_settings();
        self.setting_reply(result, source, changed);
    }

    /// Whether an agent of the session runs in Claude Code: the main agent,
    /// or the workers it starts.
    fn in_claude_code(&self) -> bool {
        self.settings.main.in_claude_code() || self.settings.worker.in_claude_code()
    }

    /// What the agents on `cc:` models do at the handover size, as
    /// `/cc-compact` says it.
    fn cc_compact_line(&self) -> String {
        if !self.in_claude_code() {
            return CC_NOT_IN_CLAUDE_CODE.to_string();
        }
        let on = !self.settings.cc_handover;
        let (what, other) = if on {
            ("compact", "off")
        } else {
            ("hand over", "on")
        };
        let text = crate::ui::sentence(agents::cc_text(on));
        if self.conversation_started() {
            format!(
                "Agents in Claude Code {what} at the handover size. {text}. The session keeps this."
            )
        } else {
            format!(
                "Agents in Claude Code {what} at the handover size. {text}. /cc-compact {other} changes that before the conversation starts."
            )
        }
    }

    /// Compacting on or off in Claude Code, before the conversation starts:
    /// the agents are built again with it.
    fn set_cc_compact(&mut self, on: bool) -> Result<String, String> {
        if !self.in_claude_code() {
            return Ok(CC_NOT_IN_CLAUDE_CODE.to_string());
        }
        let what = if on { "compact" } else { "hand over" };
        if self.settings.cc_handover != on {
            return Ok(format!(
                "Agents in Claude Code {what} at the handover size already. {}.",
                crate::ui::sentence(agents::cc_text(on))
            ));
        }
        if self.conversation_started() {
            return Ok(format!(
                "This session keeps what its agents in Claude Code do at the handover size. {}. A new session takes --cc-compact or --no-cc-compact.",
                crate::ui::sentence(agents::cc_text(!on))
            ));
        }
        let mut settings = self.settings.clone();
        settings.cc_handover = !on;
        self.rebuild_agents(settings)?;
        Ok(format!(
            "Agents in Claude Code now {what} at the handover size. {}.",
            crate::ui::sentence(agents::cc_text(on))
        ))
    }

    /// `/extra-rules`, typed at the prompt, sent from the page or chosen in
    /// the settings: says which agents get the extra rules for small models,
    /// and with `small`, `all` or `off` changes that before the conversation
    /// starts (the agents are built again). Once it has started, the
    /// session keeps it, as it keeps the main agent's model.
    pub(crate) fn extra_rules_command(&mut self, argument: &str, source: Source) {
        let before = self.reply_settings();
        let result = match argument {
            "" => Ok(self.extra_rules_line()),
            other => match ExtraRules::parse(other) {
                Some(rules) => self.set_extra_rules(rules),
                None => Err("/extra-rules takes small, all or off (/extra-rules off).".to_string()),
            },
        };
        let changed = before != self.reply_settings();
        self.setting_reply(result, source, changed);
    }

    /// Who gets the extra rules, as `/extra-rules` says it.
    fn extra_rules_line(&self) -> String {
        let rules = self.settings.extra_rules;
        let text = format!(
            "Extra rules for small models are set to {}, so {}. {}",
            rules.name(),
            rules.describe(),
            self.extra_rules_here()
        );
        if self.conversation_started() {
            format!("{text} The session keeps this.")
        } else {
            format!(
                "{text} /extra-rules small, all or off changes that before the conversation starts."
            )
        }
    }

    /// Which of this session's agents get the extra rules: the main agent
    /// by its model, and the workers by the model new ones start on.
    fn extra_rules_here(&self) -> String {
        let rules = self.settings.extra_rules;
        let main = rules.for_model(&self.settings.main);
        if self.settings.max_workers == 0 || self.crew.is_none() {
            return if main {
                "The main agent gets them.".to_string()
            } else {
                "The main agent doesn't get them.".to_string()
            };
        }
        let workers = rules.for_model(&self.settings.worker);
        match (main, workers) {
            (true, true) => "The main agent and the workers get them.",
            (true, false) => "The main agent gets them, and the workers don't.",
            (false, true) => "The workers get them, and the main agent doesn't.",
            (false, false) => "Neither the main agent nor the workers get them.",
        }
        .to_string()
    }

    /// Who gets the extra rules, before the conversation starts: the agents
    /// are built again with them.
    fn set_extra_rules(&mut self, rules: ExtraRules) -> Result<String, String> {
        if self.settings.extra_rules == rules {
            return Ok(format!(
                "Extra rules for small models are set to {} already, so {}. {}",
                rules.name(),
                rules.describe(),
                self.extra_rules_here()
            ));
        }
        if self.conversation_started() {
            return Ok(format!(
                "The session keeps its extra rules, so {}. A new session takes --extra-rules or --no-extra-rules.",
                self.settings.extra_rules.describe()
            ));
        }
        let mut settings = self.settings.clone();
        settings.extra_rules = rules;
        self.rebuild_agents(settings)?;
        Ok(format!(
            "Extra rules for small models are now set to {}, so {}. {}",
            rules.name(),
            rules.describe(),
            self.extra_rules_here()
        ))
    }

    /// Whether the main agent's conversation has started: it can't change
    /// model then. (A checkpoint, a turn that ran or runs.)
    pub(crate) fn conversation_started(&self) -> bool {
        self.session.latest_checkpoint().is_some()
            || !matches!(self.conversation, Conversation::New)
            || self.feed.activity().working
            || self
                .workflow
                .as_ref()
                .is_some_and(|workflow| !workflow.result().agent_results.is_empty())
    }

    /// The workers' model, as `/model worker` says it.
    fn workers_line(&self) -> String {
        if self.settings.max_workers == 0 || self.crew.is_none() {
            return "This session has no workers.".to_string();
        }
        if self.settings.workers_follow_main() {
            format!(
                "Workers start on the main agent's model, {}.",
                shown(&self.settings.worker)
            )
        } else {
            format!("Workers start on {}.", shown(&self.settings.worker))
        }
    }

    /// An error with what `model` needs when this session can't reach it:
    /// its client has no client for its provider (whose key wasn't there as
    /// Arko started nor added with `/keys`, or was removed), or Claude Code
    /// isn't installed and signed in.
    fn can_reach(&self, model: &Model) -> Result<(), String> {
        match self.needs(model) {
            Some(needs) => Err(providers::needs_text(&providers::model_name(model), &needs)),
            None => Ok(()),
        }
    }

    /// What `model` needs that this session doesn't have.
    pub(crate) fn needs(&self, model: &Model) -> Option<String> {
        self.keys.needs(model)
    }

    /// How the session reaches OpenAI models when its models don't say: as
    /// its client does, else as a new session would.
    fn openai_auth(&self) -> Option<OpenAiAuth> {
        self.keys
            .openai_auth()
            .or_else(|| providers::spare_openai_auth(None))
    }

    /// The main agent's model, before the conversation starts: the agents
    /// are built again on it (the workers too, when they follow it).
    fn set_main_model(&mut self, model: &Model, effort: Effort) -> Result<String, String> {
        let mut settings = self.settings.clone();
        let follow = settings.workers_follow_main();
        if model.provider == Provider::OpenAI && settings.openai_auth.is_none() {
            settings.openai_auth = self.openai_auth();
        }
        let old_main = settings.main.clone();
        settings.main = switched(&old_main, model, effort, settings.openai_auth);
        if follow {
            let worker_effort = if settings.worker.effort == old_main.effort {
                effort
            } else {
                providers::fit_effort(model, settings.worker.effort)
            };
            settings.worker = ModelSettings {
                effort: worker_effort,
                ..settings.main.clone()
            };
        }
        settings.handover_at = settings.main.handover_at.unwrap_or(settings.handover_at);
        self.rebuild_agents(settings)?;
        Ok(if follow {
            format!(
                "The main agent now runs {}, and new workers follow it.",
                shown(&self.settings.main)
            )
        } else {
            format!(
                "The main agent now runs {}. Workers keep {}.",
                shown(&self.settings.main),
                shown(&self.settings.worker)
            )
        })
    }

    /// The model of the workers started from now on: `model` at `effort`,
    /// else (`None`) the main agent's. Before the conversation starts the
    /// agents are built again on it; after, new workers start on it and the
    /// workers already started keep theirs.
    fn set_workers_model(&mut self, model: Option<(&Model, Effort)>) -> Result<String, String> {
        if self.settings.max_workers == 0 || self.crew.is_none() {
            return Err("This session has no workers (it started with --workers 0, or with the old worker tools).".to_string());
        }
        let mut settings = self.settings.clone();
        settings.worker = match model {
            Some((model, effort)) => {
                if model.provider == Provider::OpenAI && settings.openai_auth.is_none() {
                    settings.openai_auth = self.openai_auth();
                }
                switched(&settings.worker, model, effort, settings.openai_auth)
            }
            None => settings.main.clone(),
        };
        settings.own_worker_model = model.is_some();
        let follows = model.is_none();
        if !self.conversation_started() {
            self.rebuild_agents(settings)?;
            return Ok(match follows {
                true => format!(
                    "Workers follow the main agent's model, {}.",
                    shown(&self.settings.worker)
                ),
                false => format!("Workers now start on {}.", shown(&self.settings.worker)),
            });
        }
        let setup = providers::Setup {
            openai_auth: settings.openai_auth,
            claude_code: self.providers.claude_code.clone(),
        };
        let (_, worker_config) = self.agent_configs(&settings, &setup)?;
        let crew = self.crew.clone().expect("a crew");
        // The workers already started keep theirs, also those of Arko
        // before it recorded them.
        let started = crew.keep_started_models()?;
        crew.set_new_workers(new_workers(
            &settings,
            &worker_config,
            self.workers_basis(&settings, &setup),
        ));
        self.settings = settings;
        self.providers = setup;
        self.worker_config = worker_config;
        self.save_session();
        self.publish_models();
        let mut text = format!(
            "From now on, the workers you start run {}.",
            shown(&self.settings.worker)
        );
        if started > 0 {
            text.push_str(" The workers started before keep their model.");
        }
        // The main agent's instructions name the workers' model as the
        // conversation started: a note tells it.
        if let Err(error) = self
            .workflow()
            .post_note(Note::new(text.clone()).source(NoteSource::Host).to([MAIN]))
        {
            log::warn!("could not tell the main agent about the workers' model: {error}");
        }
        let before = match started {
            0 => "",
            _ => ", and the workers started before keep theirs",
        };
        Ok(match follows {
            true => format!(
                "Workers started from now on follow the main agent's model, {}. The main agent keeps it for this session{before}.",
                shown(&self.settings.worker)
            ),
            false => format!(
                "Workers started from now on run {}. The main agent keeps {} for this session{before}.",
                shown(&self.settings.worker),
                shown(&self.settings.main)
            ),
        })
    }

    /// The agents' configurations for `settings`, as the session builds
    /// them at its start (with its MCP servers, skills and indexes).
    pub(crate) fn agent_configs(
        &self,
        settings: &Settings,
        setup: &providers::Setup,
    ) -> Result<(arko_agents::AgentConfig, arko_agents::AgentConfig), String> {
        let mcp_tools = self.mcp.tool_names();
        let mcp_servers = self.mcp.started();
        let known = self
            .indexes
            .as_ref()
            .map(|indexes| indexes.known(&settings.workspace))
            .unwrap_or_default();
        let profile_names = crate::chooser::named_profiles(
            &crate::profiles::Roots::new(&self.root)
                .workspace(&settings.workspace)
                .session(&self.session.dir),
        );
        let agent_settings = AgentSettings {
            profiles: &profile_names,
            mcp_tools: &mcp_tools,
            mcp_servers: &mcp_servers,
            skill_folders: &self.skill_folders,
            index_tools: if self.indexes.is_some() {
                index::TOOL_NAMES
            } else {
                &[]
            },
            indexes: &known,
            monitors: !self.one_shot,
            ..AgentSettings::for_session(settings, setup)
        };
        Ok((
            agents::main_config(&agent_settings)?,
            agents::worker_config(&agent_settings)?,
        ))
    }

    /// Builds the agents again on `settings`, before the conversation
    /// starts: their configurations, the workflow, the main agent's handover
    /// size and new workers' model. The session and the next session's
    /// default keep them.
    pub(crate) fn rebuild_agents(&mut self, settings: Settings) -> Result<(), String> {
        let setup = providers::Setup {
            openai_auth: settings.openai_auth,
            claude_code: self.providers.claude_code.clone(),
        };
        let (main_config, worker_config) = self.agent_configs(&settings, &setup)?;
        self.settings = settings;
        self.providers = setup;
        self.main_config = main_config;
        self.worker_config = worker_config;
        self.open(false)?;
        self.start_new_workers();
        self.save_session();
        if !self.one_shot
            && let Err(error) = providers::LastModels::remember(&self.root, &self.settings)
        {
            log::warn!("could not keep the session's models for the next one: {error}");
        }
        self.publish_models();
        Ok(())
    }

    /// New workers start on the session's workers' model: on resume too,
    /// when it changed after the workers' configuration was saved.
    pub(crate) fn start_new_workers(&self) {
        if let Some(crew) = &self.crew {
            crew.set_new_workers(new_workers(
                &self.settings,
                &self.worker_config,
                self.workers_basis(&self.settings, &self.providers),
            ));
        }
    }

    /// What the start hook chooses a worker's setup from, for `settings`
    /// on `setup`: the session's worker model and the settings around it,
    /// and where the profiles are.
    pub(crate) fn workers_basis(
        &self,
        settings: &Settings,
        setup: &providers::Setup,
    ) -> std::sync::Arc<crate::chooser::Basis> {
        let known = self
            .indexes
            .as_ref()
            .map(|indexes| indexes.known(&settings.workspace))
            .unwrap_or_default();
        let agent_settings = AgentSettings {
            index_tools: if self.indexes.is_some() {
                index::TOOL_NAMES
            } else {
                &[]
            },
            indexes: &known,
            ..AgentSettings::for_session(settings, setup)
        };
        std::sync::Arc::new(crate::chooser::Basis {
            roots: crate::profiles::Roots::new(&self.root)
                .workspace(&settings.workspace)
                .session(&self.session.dir),
            worker: settings.worker.clone(),
            providers: setup.clone(),
            thinking_text: !settings.hide_thinking,
            cc_compact: !settings.cc_handover,
            handover_at: settings.handover_at,
            choice: settings.worker_models,
            extra_rules: settings.extra_rules,
            rules_text: agents::extra_rules(&agent_settings),
        })
    }

    pub(crate) fn save_session(&self) {
        if let Err(error) = self.session.save_settings(&self.settings) {
            self.say(
                Level::Error,
                &format!("Couldn't save the session's settings ({error})."),
            );
        }
    }

    /// Tells the clients (the full-screen view, the page) the agents'
    /// models, and whether the main agent's can still change.
    pub(crate) fn publish_models(&self) {
        self.gauges.touch();
        self.publish_details();
        self.gauges.text_replies.store(
            self.settings.text_replies,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.gauges.cc_compact.store(
            !self.settings.cc_handover,
            std::sync::atomic::Ordering::Relaxed,
        );
        *lock(&self.gauges.extra_rules) = self.settings.extra_rules;
        *lock(&self.gauges.worker_models) = self.settings.worker_models;
        self.gauges.worker_depth.store(
            self.settings.worker_depth,
            std::sync::atomic::Ordering::Relaxed,
        );
        *lock(&self.gauges.project) = (
            self.settings.project_instructions,
            self.settings.worker_project_instructions,
        );
        *lock(&self.gauges.models) = remote::Models {
            main: self.settings.main.model.clone(),
            main_effort: self.settings.main.effort.to_string(),
            workers: self.settings.worker.model.clone(),
            workers_effort: self.settings.worker.effort.to_string(),
            workers_follow: self.settings.workers_follow_main(),
            started: self.conversation_started(),
            main_fast: self.settings.main.fast,
            banner_model: self.settings.describe(&self.settings.main),
            banner_workers: self.workers_summary(),
            reachable: self.keys.reachable(),
            openai_auth: self
                .settings
                .openai_auth
                .or_else(|| self.openai_auth())
                .map(|auth| auth.describe().to_string()),
            has_workers: self.settings.max_workers > 0 && self.crew.is_some(),
        };
    }
}

/// `old`'s settings on `model` at `effort`: fast mode where the model has
/// it, the 1-hour cache where it takes one and it was asked for, and the
/// handover size the model's own unless one was given (held under what
/// the model leaves room for).
fn switched(
    old: &ModelSettings,
    model: &Model,
    effort: Effort,
    openai_auth: Option<OpenAiAuth>,
) -> ModelSettings {
    let old_model = providers::resolve_model(&old.model).ok();
    let default_before = old_model
        .as_ref()
        .map(|model| agents::default_handover(model, openai_auth));
    let handover_at = match old.handover_at {
        Some(at) if Some(at) != default_before => {
            Some(agents::max_handover(model, openai_auth).map_or(at, |most| at.min(most)))
        }
        _ => Some(agents::default_handover(model, openai_auth)),
    };
    let wants_1h = old.cache_1h
        || crate::credentials::switch(crate::credentials::CACHE_1H)
            .ok()
            .flatten()
            .unwrap_or(false);
    ModelSettings {
        model: providers::model_name(model),
        effort,
        fast: old.fast && providers::generation(model, effort, true, false, false).is_ok(),
        cache_1h: wants_1h && providers::takes_cache_ttl(model),
        handover_at,
    }
}

/// What new workers start on, from their configuration (`worker_config`,
/// built from `settings`).
fn new_workers(
    settings: &Settings,
    worker_config: &arko_agents::AgentConfig,
    basis: std::sync::Arc<crate::chooser::Basis>,
) -> NewWorkers {
    let chatgpt = worker_config.llm_config.model.provider == Provider::OpenAI
        && settings.openai_auth != Some(OpenAiAuth::ApiKey);
    NewWorkers {
        basis,
        model: WorkerSetup::of(worker_config),
        cache_lifetime: arko_ai::prompt_cache::prompt_cache_lifetime(
            &worker_config.llm_config.model,
            &worker_config.llm_config.generation_config,
            chatgpt,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_words_say_whose_model_and_which() {
        assert_eq!(parse(""), Ok(Request::Show(Target::Main)));
        assert_eq!(parse(" worker "), Ok(Request::Show(Target::Workers)));
        assert_eq!(parse("worker main"), Ok(Request::FollowMain));
        assert_eq!(
            parse("sol high"),
            Ok(Request::Set {
                target: Target::Main,
                model: "sol".into(),
                effort: Some(Effort::High)
            })
        );
        assert_eq!(
            parse("workers cc:fable"),
            Ok(Request::Set {
                target: Target::Workers,
                model: "cc:fable".into(),
                effort: None
            })
        );
        let error = parse("sol extreme").unwrap_err();
        assert!(error.contains("`extreme` isn't an effort"), "{error}");
        assert!(
            parse("sol high now")
                .unwrap_err()
                .contains("/model MODEL [EFFORT]")
        );
        // `main` is a model's place only after `worker`.
        assert_eq!(
            parse("main"),
            Ok(Request::Set {
                target: Target::Main,
                model: "main".into(),
                effort: None
            })
        );
    }

    #[test]
    fn a_switch_keeps_what_the_new_model_takes() {
        let opus = ModelSettings {
            model: "claude-opus-5-5".into(),
            effort: Effort::Xhigh,
            fast: true,
            cache_1h: true,
            handover_at: Some(950_000),
        };
        let gemini = providers::resolve_model("gemini").unwrap();
        let on_gemini = switched(&opus, &gemini, Effort::High, None);
        assert_eq!(on_gemini.model, "gemini-3.1-pro-preview");
        assert!(!on_gemini.fast, "Gemini has no fast mode");
        assert!(!on_gemini.cache_1h, "nor Anthropic's cache");
        assert_eq!(on_gemini.handover_at, Some(200_000), "the model's own size");
        // And back, to the size of a Claude model.
        let back = switched(
            &on_gemini,
            &providers::resolve_model("opus").unwrap(),
            Effort::Xhigh,
            None,
        );
        assert_eq!(back.handover_at, Some(950_000));
        let cc = providers::resolve_model("cc:opus").unwrap();
        let on_cc = switched(&opus, &cc, Effort::Xhigh, None);
        assert!(on_cc.fast);
        assert!(!on_cc.cache_1h, "Claude Code chooses its own");
        assert_eq!(on_cc.handover_at, Some(950_000));
        // A size given (--handover-at) stays, up to 95% of what the new
        // model accepts.
        let given = ModelSettings {
            handover_at: Some(123_000),
            ..opus.clone()
        };
        assert_eq!(
            switched(&given, &gemini, Effort::High, None).handover_at,
            Some(123_000)
        );
        let large = ModelSettings {
            handover_at: Some(900_000),
            ..opus
        };
        let luna = providers::resolve_model("luna").unwrap();
        let most = agents::max_handover(&luna, None).unwrap();
        assert!(most < 900_000);
        assert_eq!(
            switched(&large, &luna, Effort::High, None).handover_at,
            Some(most)
        );
    }
}

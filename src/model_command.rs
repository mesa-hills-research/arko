//! `/model`: the agents' models while fin runs.
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

use crate::agents::{self, AgentSettings, Effort, MAIN, ModelSettings};
use crate::crew::{NewWorkers, WorkerModel};
use crate::providers::{self, OpenAiAuth};
use crate::remote::{self, Level, Source};
use crate::session::Settings;
use crate::shell::{Shell, WindowsShell};
use crate::{Conversation, Harness, index, lock};

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

/// `model`'s settings as fin says them: `gpt-6.1-sol (high effort)`.
fn shown(model: &ModelSettings) -> String {
    format!("{} ({} effort)", model.model, model.effort)
}

impl Harness {
    /// `/model`, typed at the prompt or sent from the page: says or changes
    /// an agent's model, in a plain line.
    pub(crate) fn model_command(&mut self, argument: &str, source: Source) {
        let result = parse(argument).and_then(|request| self.model_request(request));
        match result {
            Ok(text) => {
                if source == Source::Web && !argument.trim().is_empty() {
                    self.say(Level::Info, &format!("From the web page: {text}"));
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
                    "The main agent keeps {} for this conversation. /model worker sets the model for workers started from now on.",
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
                "The main agent keeps {} for this conversation. /model worker sets the model for workers started from now on.",
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
    /// its client (which has every provider whose credential was there as
    /// fin started) has no client for its provider, or Claude Code isn't
    /// installed and signed in.
    fn can_reach(&self, model: &Model) -> Result<(), String> {
        match self.needs(model) {
            Some(needs) => Err(providers::needs_text(&providers::model_name(model), &needs)),
            None => Ok(()),
        }
    }

    /// What `model` needs that this session doesn't have.
    pub(crate) fn needs(&self, model: &Model) -> Option<String> {
        match &model.provider {
            Provider::ClaudeCode => {
                providers::missing_credential(&providers::model_name(model), None)
            }
            provider if self.client.has_provider(provider) => None,
            provider => Some(providers::credential_name(provider).to_string()),
        }
    }

    /// The main agent's model, before the conversation starts: the agents
    /// are built again on it (the workers too, when they follow it).
    fn set_main_model(&mut self, model: &Model, effort: Effort) -> Result<String, String> {
        let mut settings = self.settings.clone();
        let follow = settings.workers_follow_main();
        if model.provider == Provider::OpenAI && settings.openai_auth.is_none() {
            settings.openai_auth = providers::spare_openai_auth(None);
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
                    settings.openai_auth = providers::spare_openai_auth(None);
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
        // The workers already started keep theirs, also those of fin
        // before it recorded them.
        let started = crew.keep_started_models()?;
        crew.set_new_workers(new_workers(&settings, &worker_config));
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
                "Workers started from now on follow the main agent's model, {}. The main agent keeps it for this conversation{before}.",
                shown(&self.settings.worker)
            ),
            false => format!(
                "Workers started from now on run {}. The main agent keeps {} for this conversation{before}.",
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
        let shell = Shell::detect(settings.windows_shell.unwrap_or(WindowsShell::Powershell));
        let mcp_tools = self.mcp.tool_names();
        let mcp_servers = self.mcp.started();
        let known = self
            .indexes
            .as_ref()
            .map(|indexes| indexes.known(&settings.workspace))
            .unwrap_or_default();
        let agent_settings = AgentSettings {
            main: &settings.main,
            worker: &settings.worker,
            providers: setup,
            max_workers: settings.max_workers,
            handover_at: settings.handover_at,
            show_thinking: settings.show_thinking,
            shell_name: shell.name,
            mcp_tools: &mcp_tools,
            mcp_servers: &mcp_servers,
            skill_folders: &self.skill_folders,
            index_tools: if self.indexes.is_some() {
                index::TOOL_NAMES
            } else {
                &[]
            },
            indexes: &known,
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
    fn rebuild_agents(&mut self, settings: Settings) -> Result<(), String> {
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
        if self.settings.main_handover_at.is_none() {
            let at = self
                .settings
                .main
                .handover_at
                .unwrap_or(self.settings.handover_at);
            self.handover.set_at(at);
        }
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
            crew.set_new_workers(new_workers(&self.settings, &self.worker_config));
        }
    }

    fn save_session(&self) {
        if let Err(error) = self.session.save_settings(&self.settings) {
            self.say(
                Level::Error,
                &format!("fin: could not save the session's settings: {error}"),
            );
        }
    }

    /// Tells the clients (the full-screen view, the page) the agents'
    /// models, and whether the main agent's can still change.
    pub(crate) fn publish_models(&self) {
        *lock(&self.gauges.models) = remote::Models {
            main: self.settings.main.model.clone(),
            main_effort: self.settings.main.effort.to_string(),
            workers: self.settings.worker.model.clone(),
            workers_effort: self.settings.worker.effort.to_string(),
            workers_follow: self.settings.workers_follow_main(),
            started: self.conversation_started(),
            banner_model: self.settings.describe(&self.settings.main),
            banner_workers: self.workers_summary(),
            reachable: [
                Provider::Anthropic,
                Provider::OpenAI,
                Provider::Google,
                Provider::OpenRouter,
            ]
            .iter()
            .filter(|provider| self.client.has_provider(provider))
            .map(|provider| providers::provider_key(provider).to_string())
            .collect(),
            openai_auth: self
                .settings
                .openai_auth
                .or_else(|| providers::spare_openai_auth(None))
                .map(|auth| auth.describe().to_string()),
            has_workers: self.settings.max_workers > 0 && self.crew.is_some(),
        };
    }
}

/// `old`'s settings on `model` at `effort`: fast mode where the model has
/// it, the 1-hour cache where it takes one and it was asked for, and the
/// handover size the model's own unless one was given.
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
        Some(at) if Some(at) != default_before => Some(at),
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
fn new_workers(settings: &Settings, worker_config: &arko_agents::AgentConfig) -> NewWorkers {
    let chatgpt = worker_config.llm_config.model.provider == Provider::OpenAI
        && settings.openai_auth != Some(OpenAiAuth::ApiKey);
    NewWorkers {
        model: WorkerModel::of(worker_config),
        cache_1h: settings.worker.cache_1h,
        cache_lifetime: arko_ai::prompt_cache::prompt_cache_lifetime(
            &worker_config.llm_config.model,
            &worker_config.llm_config.generation_config,
            chatgpt,
        ),
        chatgpt,
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
            handover_at: Some(500_000),
        };
        let gemini = providers::resolve_model("gemini").unwrap();
        let on_gemini = switched(&opus, &gemini, Effort::High, None);
        assert_eq!(on_gemini.model, "gemini-3.1-pro-preview");
        assert!(!on_gemini.fast, "Gemini has no fast mode");
        assert!(!on_gemini.cache_1h, "nor Anthropic's cache");
        assert_eq!(on_gemini.handover_at, Some(200_000), "the model's own size");
        let cc = providers::resolve_model("cc:opus").unwrap();
        let on_cc = switched(&opus, &cc, Effort::Xhigh, None);
        assert!(on_cc.fast);
        assert!(!on_cc.cache_1h, "Claude Code chooses its own");
        // A size given (--handover-at) stays.
        let given = ModelSettings {
            handover_at: Some(123_000),
            ..opus
        };
        assert_eq!(
            switched(&given, &gemini, Effort::High, None).handover_at,
            Some(123_000)
        );
    }
}

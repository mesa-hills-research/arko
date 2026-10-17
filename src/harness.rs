//! The session as it runs: the main agent's conversation, its turns, the
//! commands typed while it runs, and the session's terminal (`Managers`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arko_agents::Tool;
use arko_agents::builtins::{APPLY_DIFF_TOOL_NAME, ASK_USER_TOOL_NAME, EDIT_FILE_TOOL_NAME};
use arko_agents::terminal::{TerminalConfig, TerminalManager, TerminalTool};
use arko_ai::LLMClient;
use arko_workflows::notes::Note;
use arko_workflows::persistence::{EventLog, PersistenceConfig};
use arko_workflows::prelude::*;
use arko_workflows::services::{RUN_WORKFLOW_AGENT_TOOL_NAME, WorkflowServicesConfig};
use tokio::sync::{mpsc, oneshot};

use crate::agents::{MAIN, NO_LIMIT, WORKER};
use crate::crew::Crew;
use crate::inputs::{Asker, Event, Inputs, unread_notes, waits_with};
use crate::remote::{Action, Delivery, Level, REFRESH_MESSAGE, Source};
use crate::session::{Mode, Session, Settings};
use crate::shell::{Shell, ShellTool};
use crate::status::Gauges;
use crate::ui::{Control, Input, Output, Stats};
use crate::{
    agents, commands, conversations, crew, details, glyphs, handover, index, keys, mcp, message,
    outside, project, providers, remote, shell, skills, ui,
};
use crate::{and_list, lock};

/// The session's terminal: its manager runs the terminal tools' background
/// jobs and the `shell` tool's commands (held, with the shell's own
/// arguments, until one moves to the background) and keeps their logs in
/// the session's folder.
pub(crate) struct Managers {
    pub(crate) jobs: Arc<TerminalManager>,
    /// When the shell's commands move to the background, and those running.
    pub(crate) background: Arc<shell::Background>,
}

impl Managers {
    /// Opens it with outside access off ([`outside::Outside`] sets it) and
    /// the shell's commands kept in the foreground: set
    /// [`background`](Self::background) before making the tools.
    pub(crate) fn open(session: &Session, shell: &Shell) -> Result<Self, String> {
        let mut config = TerminalConfig::new(session.jobs_dir());
        config.shell = shell.program.clone();
        config.shell_args = shell.job_args.clone();
        let jobs = TerminalManager::new(config).map_err(|error| {
            format!("could not open the session's terminal ({error}). Is it open in another Arko?")
        })?;
        Ok(Self {
            background: shell::Background::new(0, &jobs),
            jobs,
        })
    }

    pub(crate) fn list(&self) -> Vec<Arc<TerminalManager>> {
        vec![self.jobs.clone()]
    }

    /// The shell tool, which keeps the command channels of the runs it is
    /// bound to in `runs`, and the terminal tools.
    pub(crate) fn tools(&self, shell: &Shell, runs: Arc<handover::Runs>) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(
            ShellTool::new(self.jobs.clone(), shell.clone(), self.background.clone())
                .keeping_runs(runs),
        )];
        for tool in TerminalTool::all(self.jobs.clone()) {
            tools.push(Arc::new(tool));
        }
        tools
    }

    /// Stops the jobs still running.
    pub(crate) async fn shutdown(&self) {
        self.jobs.shutdown().await;
    }
}

/// The workflow's name in new sessions.
pub(crate) const WORKFLOW_NAME: &str = "arko";

/// What a session's checkpoint was saved with: the workflow's name and the
/// agents' configurations. A resume needs both unchanged, so a session
/// keeps them when Arko changes: its agents' instructions, or its name
/// (sessions saved before the rename to Arko have `fin`).
pub(crate) struct SavedSetup {
    pub(crate) name: String,
    /// The configuration of each agent's latest run.
    pub(crate) configs: std::collections::HashMap<String, arko_agents::AgentConfig>,
}

pub(crate) fn saved_setup(checkpoint: &Path, run_dir: &Path) -> Result<SavedSetup, String> {
    let unreadable = |error: String| {
        format!(
            "could not read the session's checkpoint {}: {error}",
            checkpoint.display()
        )
    };
    let text =
        std::fs::read_to_string(checkpoint).map_err(|error| unreadable(error.to_string()))?;
    let state: arko_workflows::persistence::state::WorkflowState =
        serde_json::from_str(&text).map_err(|error| unreadable(error.to_string()))?;
    let mut configs = std::collections::HashMap::new();
    for result in state
        .agent_results_in(run_dir)
        .map_err(|error| unreadable(error.to_string()))?
    {
        configs.insert(result.agent_name, result.task.agent_config);
    }
    Ok(SavedSetup {
        name: state.workflow_name,
        configs,
    })
}

/// What a resumed session's agents aren't offered, as they started without
/// it: the tools Arko has now that their saved tool lists leave out, and
/// skills. A resumed conversation keeps the configuration it was saved
/// with.
pub(crate) fn unoffered(
    saved: &SavedSetup,
    fresh: &[(&str, &arko_agents::AgentConfig)],
) -> Option<String> {
    let mut missing: Vec<&str> = Vec::new();
    let mut skills = false;
    for (agent, fresh) in fresh {
        let Some(saved) = saved.configs.get(*agent) else {
            continue;
        };
        let offered = &saved.tool_config.available_tool_names;
        for name in &fresh.tool_config.available_tool_names {
            // An agent that ends its turns with a reply in text answers in
            // text beside its calls instead, and one saved before edit_file
            // edits files with apply_diff.
            let answers_in_text =
                name == message::MESSAGE_USER && saved.tool_config.ends_task_with_text();
            let edits_with_apply_diff = name == EDIT_FILE_TOOL_NAME
                && offered.iter().any(|name| name == APPLY_DIFF_TOOL_NAME);
            if !offered.contains(name)
                && !missing.contains(&name.as_str())
                && !answers_in_text
                && !edits_with_apply_diff
            {
                missing.push(name);
            }
        }
        skills |= fresh.skills_config.is_some() && saved.skills_config.is_none();
    }
    let mut parts = Vec::new();
    if missing.contains(&crew::START_AGENT) {
        parts.push("workers that run in parallel".to_string());
    }
    if missing
        .iter()
        .any(|name| agents::FILE_TOOL_NAMES.contains(name))
    {
        parts.push("the file tools".to_string());
    }
    if missing.contains(&ASK_USER_TOOL_NAME) {
        parts.push("questions to you (ask_user)".to_string());
    }
    if missing.contains(&message::MESSAGE_USER) {
        parts.push("answers to you while it works (message_user)".to_string());
    }
    if missing.iter().any(|name| index::TOOL_NAMES.contains(name)) {
        parts.push("the search index tools".to_string());
    }
    let mut servers: Vec<&str> = Vec::new();
    for (server, _) in missing.iter().filter_map(|name| mcp::split_tool_name(name)) {
        if !servers.contains(&server) {
            servers.push(server);
        }
    }
    if !servers.is_empty() {
        parts.push(format!(
            "the tools of the MCP server{} {}",
            if servers.len() == 1 { "" } else { "s" },
            and_list(&servers)
        ));
    }
    if skills {
        parts.push("skills".to_string());
    }
    let named = |name: &str| {
        index::TOOL_NAMES.contains(&name)
            || mcp::split_tool_name(name).is_some()
            || crew::LEAD_TOOL_NAMES.contains(&name)
            || crew::WORKER_TOOL_NAMES.contains(&name)
            || agents::FILE_TOOL_NAMES.contains(&name)
            || name == ASK_USER_TOOL_NAME
            || name == message::MESSAGE_USER
    };
    let others = missing.iter().filter(|name| !named(name)).count();
    if others > 0 {
        parts.push(format!(
            "{others} other tool{}",
            if others == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "This session started without {}, so its agents don't get them. A new session does.",
        and_list(&parts)
    ))
}

/// Where the conversation with the main agent stands.
pub(crate) enum Conversation {
    /// The next message starts a conversation.
    New,
    /// The last turn ended with a reply: the next message continues it.
    Answered(AgentResult),
    /// The last turn was stopped: the next message reopens it.
    Stopped(StoppedRun),
}

pub(crate) enum TurnEnd {
    Answered,
    Stopped,
    Failed,
    /// The user closed the input during the turn.
    Exit,
}

/// What starts a turn.
pub(crate) enum Prompt {
    /// The user's message.
    User(String),
    /// The workers' news, without a message from the user.
    News,
    /// A background job of the agent's finished, without a message from
    /// the user: its terminal update follows.
    Jobs,
    /// What the user typed during the last turn, which the agent finished
    /// before it read, without a new message.
    Unread,
    /// A turn Arko stopped in the middle of goes on, without a message.
    Recover,
}

/// What became of the workers' news.
pub(crate) enum News {
    /// There is none (left).
    Done,
    /// The conversation isn't waiting for a message: it goes with the
    /// user's next one.
    Waiting,
    /// The user stopped the turn it started.
    Stopped,
    /// The turn it started failed: it waits for the next message, or news.
    Failed,
    /// The user quit during the turn it started.
    Exit,
}

pub(crate) struct Harness {
    pub(crate) settings: Settings,
    /// What the agents' configurations take from the providers (how the
    /// session reaches OpenAI models, how Claude Code runs): `/model`
    /// builds them again with it.
    pub(crate) providers: providers::Setup,
    pub(crate) session: Session,
    pub(crate) client: Arc<LLMClient>,
    /// The API keys: what the client and the search indexes use, and which
    /// providers the session reaches (`/keys`).
    pub(crate) keys: Arc<keys::Keys>,
    pub(crate) main_config: arko_agents::AgentConfig,
    pub(crate) worker_config: arko_agents::AgentConfig,
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    /// Arko's folder.
    pub(crate) root: PathBuf,
    /// The MCP servers started, and what was left out and why.
    pub(crate) mcp: mcp::Running,
    /// The skill folders that exist.
    pub(crate) skill_folders: Vec<PathBuf>,
    /// The search indexes; `None` when they couldn't be opened.
    pub(crate) indexes: Option<Arc<index::Indexes>>,
    /// In a resumed session: the tools (and skills) Arko has that the
    /// session's agents aren't offered, as they started without them.
    pub(crate) unoffered: Option<String>,
    pub(crate) output: Arc<Output>,
    pub(crate) stats: Arc<Mutex<Stats>>,
    pub(crate) workflow: Option<Arc<Workflow>>,
    pub(crate) printer: Option<(mpsc::UnboundedSender<Control>, tokio::task::JoinHandle<()>)>,
    pub(crate) conversation: Conversation,
    /// The session's conversations, and which one the next message goes
    /// to (`/new`, `/resume`).
    pub(crate) conversations: conversations::Conversations,
    /// The project's instruction files, as Arko found them (see `project`).
    pub(crate) project: project::Found,
    /// What the user typed during a turn that the agent didn't read before
    /// the turn ended: it goes with the next message.
    pub(crate) unread: Vec<String>,
    /// The conversation changed (`/new`, `/resume`): the next message names
    /// the workers still running, whose reports now come to it.
    pub(crate) switched: bool,
    /// Without a terminal, lines read during a turn wait for their own
    /// turns instead of reaching the agent as notes.
    pub(crate) queue_lines: bool,
    /// The user quit while a turn ran before the prompt opened.
    pub(crate) exiting: bool,
    /// The session switched between Chat and Agent: it closes, for the
    /// front end to open it again in its new mode.
    pub(crate) reopen: bool,
    /// An edited message (or one sent again) waits to start a turn, cutting
    /// the conversation back to its index (`Request::Edit`).
    pub(crate) edited: Option<(usize, String)>,
    /// The next turn cuts the conversation back to this message first.
    pub(crate) cut_back: Option<usize>,
    /// The prompt cache is kept warm with a message (`REFRESH_MESSAGE`):
    /// the provider takes no refresh out of band.
    pub(crate) refresh_turn: bool,
    /// Arko only waits for the workers to finish (no terminal): requests
    /// that would start something are refused.
    pub(crate) only_workers: bool,
    /// The images the transcript shows.
    pub(crate) images: crate::images::Images,
    /// The images the user's next message (or note) sends, and as the
    /// transcript shows them.
    pub(crate) pending_images: Vec<arko_ai::Image>,
    pub(crate) pending_refs: Vec<crate::images::ImageRef>,
    /// Native workers. None when disabled or when resuming a legacy session.
    pub(crate) crew: Option<Arc<Crew>>,
    /// Puts the main agent's questions to the user.
    pub(crate) asker: Arc<Asker>,
    /// Arko sends one message and exits (-p).
    pub(crate) one_shot: bool,
    /// `/exit` was typed with workers or background jobs running: the next
    /// one quits.
    pub(crate) exit_warned: bool,
    /// The background jobs the line prompt's last `/exit` warned of: the
    /// next `/exit` stops those, and asks again about any other.
    pub(crate) jobs_shown: Vec<uuid::Uuid>,
    /// Where the full-screen view, while it is up, is told to ask about
    /// the background jobs running (its quit popup).
    pub(crate) quit_popup: Option<mpsc::UnboundedSender<ui::QuitAsk>>,
    /// The front end asks before quitting with workers running too (an
    /// app), not only with background jobs.
    pub(crate) ask_about_workers: bool,
    /// Where the user's lines come from: the terminal, or an app.
    pub(crate) input_source: Source,
    /// The user was told that the workers' news waits for their message.
    pub(crate) news_hint_shown: bool,
    /// What the remote page shows: the workflows' events, and what Arko
    /// says of the turns.
    pub(crate) feed: remote::Feed,
    /// The remote page's server, while it runs.
    pub(crate) remote: Option<remote::Server>,
    /// Where the page's actions go; [`Inputs`] reads them.
    pub(crate) remote_actions: mpsc::UnboundedSender<Action>,
    /// Where the page was last asked for, and the port it was served on:
    /// `/remote` serves it there again.
    pub(crate) remote_bind: Option<(remote::Bind, u16)>,
    /// The figures of the page's status.
    pub(crate) gauges: Arc<Gauges>,
    /// The session's details as data, for the commands and for front ends.
    pub(crate) details: Arc<details::Details>,
    /// Outside access, which the user turns on and off (`/outside`).
    pub(crate) outside: Arc<outside::Outside>,
    /// The main agent's handover size, and the watch that asks for its
    /// handovers (`/handover`, `/handover-at`).
    pub(crate) handover: Arc<handover::Handover>,
    /// When the shell's commands move to the background, and those running
    /// (`/background-after`, Ctrl+B).
    pub(crate) background: Arc<shell::Background>,
}

impl Harness {
    pub(crate) fn workflow(&self) -> &Arc<Workflow> {
        self.workflow.as_ref().expect("the workflow is open")
    }

    /// Builds the workflow, from the session's latest checkpoint when
    /// `resume` is set, and starts printing its events.
    pub(crate) fn open(&mut self, resume: bool) -> Result<(), String> {
        let checkpoint = if resume {
            self.session.latest_checkpoint()
        } else {
            None
        };
        let saved = match &checkpoint {
            Some(checkpoint) => Some(saved_setup(checkpoint, &self.session.run_dir())?),
            None => None,
        };
        // A resume needs the configurations the checkpoint was saved with.
        let config = |agent: &str, fresh: &arko_agents::AgentConfig| {
            saved
                .as_ref()
                .and_then(|saved| saved.configs.get(agent))
                .unwrap_or(fresh)
                .clone()
        };
        // A chat, and a session that switched between Chat and Agent, give
        // the main agent's configuration now to its new conversations, while
        // each saved one resumes with its own (until its next message, which
        // goes on in the session's mode: `reconfigure`).
        let keep = self.keeps_run_configs();
        if let Some(saved) = &saved {
            // (A conversation of the other mode takes the session's on its
            // next message: nothing to say of it.)
            let main_resumes = !keep
                || saved.configs.get(MAIN).is_some_and(|config| {
                    config.tool_config.chat == (self.settings.mode == Mode::Chat)
                });
            let mut fresh = vec![(WORKER, &self.worker_config)];
            if main_resumes && self.settings.mode == Mode::Agent {
                fresh.insert(0, (MAIN, &self.main_config));
            }
            self.unoffered = unoffered(saved, &fresh);
        }
        let name = saved.as_ref().map_or(WORKFLOW_NAME, |saved| &saved.name);
        let main_config = if keep {
            self.main_config.clone()
        } else {
            config(MAIN, &self.main_config)
        };
        // A main agent that ends its turns with a reply in text answers in
        // text beside its calls: it has no `message_user`. One that calls
        // `complete_task` (as every session saved before text replies
        // does) keeps it, and so may a saved conversation that keeps its
        // configuration.
        let answers_with_tool = keep || !main_config.tool_config.ends_task_with_text();
        let mut builder = Workflow::builder()
            .name(name)
            .workspace(&self.settings.workspace)
            .llm_client(self.client.clone())
            .agent(MAIN, main_config.clone())
            // The size the user set, which isn't saved with the workflow.
            .agent_handover_threshold(MAIN, self.handover.threshold())
            .persistence(
                PersistenceConfig::new(self.session.run_dir())
                    .event_log(EventLog::LifecycleOnly)
                    .keep_last_checkpoints(3),
            )
            .disable_messaging()
            .cancel_grace_period(Duration::from_secs(10));
        if keep {
            builder = builder.keep_saved_configs([MAIN]);
        }
        for tool in &self.tools {
            builder = builder.agent_tool(MAIN, tool.clone());
        }
        if answers_with_tool {
            builder = builder.agent_tool(MAIN, Arc::new(message::MessageUser));
        }
        builder = builder.user_input_handler(self.asker.clone());
        let legacy = self.settings.max_workers > 0
            && saved
                .as_ref()
                .and_then(|saved| saved.configs.get(MAIN))
                .is_some_and(|config| {
                    !config
                        .tool_config
                        .available_tool_names
                        .iter()
                        .any(|name| name == crew::START_AGENT)
                });
        if legacy {
            self.crew = None;
            self.unoffered = Some(match self.unoffered.take() {
                Some(note) => format!("{note}\n{}", crew::LEGACY_NOTICE),
                None => crew::LEGACY_NOTICE.into(),
            });
            for name in crew::LEGACY_TOOL_NAMES
                .iter()
                .copied()
                .chain([RUN_WORKFLOW_AGENT_TOOL_NAME])
            {
                builder = builder.agent_tool(MAIN, Arc::new(crew::LegacyTool(name)));
            }
        }
        if self.settings.max_workers > 0 {
            builder = builder
                .agent(WORKER, config(WORKER, &self.worker_config))
                // The workers' size the user set, for every worker run.
                .agent_handover_threshold(WORKER, self.handover.workers_threshold());
            for tool in &self.tools {
                builder = builder.agent_tool(WORKER, tool.clone());
            }
            if legacy {
                for name in crew::LEGACY_TOOL_NAMES {
                    builder = builder.agent_tool(WORKER, Arc::new(crew::LegacyTool(name)));
                }
            }
        }
        if let Some(crew) = &self.crew {
            let weak = Arc::downgrade(crew);
            builder = builder
                .workflow_services(
                    WorkflowServicesConfig::default()
                        .allow_caller(MAIN)
                        .allow_target(WORKER),
                )
                .subagents(crew.config())
                .hook(
                    HookPoint::PreToolExecution,
                    Arc::new(move |hook| {
                        if hook.agent_name.as_deref() == Some(MAIN)
                            && let Some(run) = hook.agent_run_id
                            && let Some(crew) = weak.upgrade()
                        {
                            crew.lead_run(run);
                        }
                        HookResult::Continue
                    }),
                );
        }
        // After the crew's hook: a call it refuses doesn't act.
        builder = builder.hook(HookPoint::PreToolExecution, self.outside.hook());
        // The images the tools return, for the transcript.
        builder = builder.tool_image_sink(Arc::new(crate::images::ToolImages {
            feed: self.feed.clone(),
            images: self.images.clone(),
        }));
        if let Some(checkpoint) = checkpoint {
            builder = builder.resume_from(checkpoint);
            // The saved workers' runs are checked as the workflow is built.
            if let Some(crew) = &self.crew {
                crew.detach();
            }
        }
        let workflow = Arc::new(builder.build().map_err(|error| error.to_string())?);
        let mut configs = vec![main_config];
        if self.settings.max_workers > 0 {
            configs.push(config(WORKER, &self.worker_config));
        }
        self.outside.attach(&workflow, &configs);
        self.handover
            .watch(&workflow, &configs[0], self.settings.openai_auth);
        let (control, controls) = mpsc::unbounded_channel();
        let task = tokio::spawn(ui::print_events(
            workflow.subscribe_events_lossless(),
            controls,
            self.output.clone(),
            self.stats.clone(),
            self.settings.show_thinking,
            configs[0].tool_config.ends_task_with_text(),
            Some(Arc::downgrade(&workflow)),
            Some(self.feed.clone()),
        ));
        *lock(&self.gauges.workflow) = Some(workflow.clone());
        *lock(&self.gauges.crew) = self.crew.clone();
        if let Some((_, old)) = self.printer.replace((control, task)) {
            old.abort();
        }
        // The conversation the session was in goes on.
        self.open_conversations(&workflow, resume);
        let lead = self.conversations.current_run();
        self.conversation = conversation_of(&workflow, lead);
        {
            // What the main agent's model accepts, and (resumed) how much
            // of it the conversation takes, before its next request says.
            let model = &configs[0].llm_config.model;
            let chatgpt = model.provider == arko_ai::Provider::OpenAI
                && self.settings.openai_auth == Some(providers::OpenAiAuth::ChatGpt);
            let mut stats = lock(&self.stats);
            stats.context_window = arko_ai::input_limit::effective_max_input_tokens(model, chatgpt);
            // How long its prompt cache lasts, as the provider says; Claude
            // Code's shows in its cache writes.
            let lifetime = arko_ai::prompt_cache::prompt_cache_lifetime(
                model,
                &configs[0].llm_config.generation_config,
                chatgpt,
            );
            stats.learns_cache_lifetime = model.provider == arko_ai::Provider::ClaudeCode;
            if !stats.learns_cache_lifetime || stats.cache_lifetime.is_none() {
                stats.cache_lifetime = match lifetime {
                    arko_ai::PromptCacheLifetime::Known(lifetime) => Some(lifetime),
                    _ => None,
                };
            }
            let task = match &self.conversation {
                Conversation::Answered(result) => Some(&result.task),
                Conversation::Stopped(run) => Some(&run.result.task),
                Conversation::New => None,
            };
            if let Some(size) = task.and_then(|task| task.current_conversation_context_size()) {
                stats.context_tokens = Some(size);
            }
        }
        if let Some(crew) = &self.crew {
            crew.attach(&workflow, lead);
            if resume {
                workflow.resume_subagents();
            }
        }
        self.workflow = Some(workflow);
        self.publish_conversations();
        self.details.set_unoffered(self.unoffered.clone());
        self.publish_details();
        Ok(())
    }

    /// What the full-screen view's banner shows: the session's top-level
    /// facts, and its notes (`notes` first: what Arko said before the
    /// session started).
    pub(crate) fn header(&self, model_note: Option<String>) -> ui::Banner {
        let mut tools = Vec::new();
        tools.extend(self.mcp.summary());
        let mut notes: Vec<(String, Option<Level>)> = Vec::new();
        match skills::list(&self.skill_folders, &self.settings.workspace) {
            Ok(skills) => {
                let count = skills.on_demand_skills.len() + skills.auto_loaded_skills.len();
                if count > 0 {
                    tools.push(format!(
                        "{count} skill{}",
                        if count == 1 { "" } else { "s" }
                    ));
                }
            }
            Err(error) => notes.push((
                format!("The skills couldn't be read: {error}"),
                Some(Level::Warn),
            )),
        }
        tools.extend(self.indexes.as_ref().and_then(|indexes| indexes.summary()));
        if let Some(crew) = &self.crew
            && !crew.is_empty()
        {
            notes.push((
                "The session's workers are kept. /workers lists them.".to_string(),
                None,
            ));
        }
        if let Some(note) = &self.unoffered {
            notes.push((note.clone(), Some(Level::Warn)));
        }
        if self.outside.is_on() {
            notes.push((
                "Outside access is on: the agents may change files outside the working directory. /outside off turns it off."
                    .to_string(),
                Some(Level::Warn),
            ));
        }
        ui::Banner {
            model: self.settings.describe(&self.settings.main),
            workers: self.workers_summary(),
            workspace: self.settings.workspace.display().to_string(),
            session: self.session.id.clone(),
            project: Some(self.project_names()).filter(|names| !names.is_empty()),
            tools,
            model_note,
            notes,
        }
    }

    /// The session's top-level facts at the line prompt (`boxed`: the
    /// full-screen view draws them as its banner instead), then the start
    /// of a new conversation or a resumed one.
    pub(crate) fn banner(&self, new: bool, boxed: bool) {
        if !boxed {
            self.loose_banner();
        }
        self.banner_end(new);
    }

    /// The facts as lines, as the line prompt shows them.
    pub(crate) fn loose_banner(&self) {
        let out = &self.output;
        out.line(&format!(
            "{} {} · {}",
            out.bold("Arko"),
            self.settings.describe(&self.settings.main),
            self.workers_summary()
        ));
        out.line(&out.dim(&format!(
            "{} · session {}{}",
            self.settings.workspace.display(),
            self.session.id,
            Some(self.project_names())
                .filter(|names| !names.is_empty())
                .map(|names| format!(" · {names} loaded"))
                .unwrap_or_default()
        )));
        let mut extras = Vec::new();
        extras.extend(self.mcp.summary());
        match skills::list(&self.skill_folders, &self.settings.workspace) {
            Ok(skills) => {
                let count = skills.on_demand_skills.len() + skills.auto_loaded_skills.len();
                if count > 0 {
                    extras.push(format!(
                        "{count} skill{}",
                        if count == 1 { "" } else { "s" }
                    ));
                }
            }
            Err(error) => out.line(&out.warn(&format!("The skills couldn't be read: {error}"))),
        }
        extras.extend(self.indexes.as_ref().and_then(|indexes| indexes.summary()));
        if !extras.is_empty() {
            out.line(&out.dim(&extras.join(" · ")));
        }
        if let Some(crew) = &self.crew
            && !crew.is_empty()
        {
            out.line(&out.dim(
                "The session's workers are kept: /workers lists them, and the agent can continue them.",
            ));
        }
        if let Some(note) = &self.unoffered {
            out.line(&out.warn(note));
        }
        // The risky setting is the one worth a line.
        if self.outside.is_on() {
            out.line(&out.warn(&self.outside.describe()));
        }
    }

    /// A new conversation's hint, or a resumed one's history and notice.
    pub(crate) fn banner_end(&self, new: bool) {
        let out = &self.output;
        if new {
            out.echo(&out.dim(
                "Type a message (end a line with \\ to continue it). While the agent works, what you type adds to its task, and Ctrl-C stops the turn. /help lists the commands.",
            ));
            return;
        }
        // The conversation so far, for the clients (the web page, the
        // full-screen view): they start with what was said.
        if let Some(run) = self
            .conversations
            .current_run()
            .and_then(|run| self.workflow().agent_result(run))
        {
            self.feed.restore(remote::history(
                &run.task.conversation.messages,
                run.task.agent_config.tool_config.ends_task_with_text(),
                &self.images,
            ));
        }
        if let Conversation::Answered(result) = &self.conversation {
            out.echo(&out.dim("Resumed. The last reply:"));
            out.echo(&result.summary);
            self.feed.notice(Level::Info, "Resumed.");
        } else if self.conversations.current.is_none() && self.conversations.has_earlier() {
            self.say(
                Level::Info,
                "Resumed. The next message starts a new conversation. /resume goes back to the last one.",
            );
        } else {
            self.say(Level::Info, "Resumed.");
        }
    }

    /// When the main agent and new workers hand over (or compact).
    pub(crate) fn handover_facts(&self) -> details::HandoverFacts {
        details::HandoverFacts {
            main_at: self.handover.at(),
            main_compacts: self.handover.compacts(),
            workers_at: self.new_workers_handover_at().unwrap_or(
                self.settings
                    .worker
                    .handover_at
                    .unwrap_or(self.settings.handover_at),
            ),
            workers_compact: agents::compacts(&self.worker_config),
        }
    }

    /// The session's facts, as `/session` says them.
    pub(crate) fn session_facts(&self) -> details::SessionFacts {
        let settings = &self.settings;
        let model = |model: &agents::ModelSettings| details::ModelFacts {
            model: model.model.clone(),
            shown: settings.describe(model),
            effort: model.effort,
            fast: model.fast,
            cache_1h: model.cache_1h,
        };
        details::SessionFacts {
            id: self.session.id.clone(),
            dir: self.session.dir.clone(),
            workspace: settings.workspace.clone(),
            title: settings.title.clone(),
            created: settings.created,
            main: model(&settings.main),
            workers: (settings.max_workers > 0).then(|| details::WorkersFacts {
                model: model(&settings.worker),
                max_running: (settings.max_workers != NO_LIMIT).then_some(settings.max_workers),
            }),
            handover: self.handover_facts(),
            text_replies: settings.text_replies,
            extra_rules: settings.extra_rules,
            cc_compact: !settings.cc_handover,
            project_files: settings.project_instructions,
            project_workers: self
                .has_workers()
                .then_some(settings.worker_project_instructions),
            outside: self.outside.is_on(),
            background_after: self.background.after().map_or(0, |after| after.as_secs()),
            show_thinking: settings.show_thinking,
            hide_thinking: settings.hide_thinking,
        }
    }

    /// Tells the session's details what changed: what front ends read and
    /// the commands say.
    pub(crate) fn publish_details(&self) {
        self.details
            .publish_session(self.session_facts(), self.settings.clone());
    }

    /// The workers, as the banner and `/session` say it.
    pub(crate) fn workers_summary(&self) -> String {
        let mut text = match self.settings.max_workers {
            0 => return "no workers".to_string(),
            NO_LIMIT => "parallel workers".to_string(),
            n if self.crew.is_some() => format!("up to {n} parallel workers"),
            n => format!("up to {n} workers"),
        };
        if self.settings.worker != self.settings.main {
            text.push_str(&format!(
                " on {}",
                self.settings.describe(&self.settings.worker)
            ));
        }
        text
    }

    /// After a crash, the turn that was running goes on where it was saved.
    /// Continues a turn Arko stopped in the middle of. The user may quit
    /// during it (`exiting`).
    pub(crate) async fn restart_unfinished(&mut self, input: &mut Inputs) {
        if let Conversation::Stopped(run) = &self.conversation
            && matches!(run.restart, Restart::Recover)
        {
            self.say(
                Level::Info,
                "The last turn didn't finish (Arko stopped while it ran), so it continues now.",
            );
            if let TurnEnd::Exit = self.turn(Prompt::Recover, input).await {
                self.exiting = true;
            }
        }
    }

    pub(crate) async fn repl(&mut self, input: &mut Inputs) {
        let mut last_interrupt: Option<Instant> = None;
        loop {
            let (line, source) = match input.next_event(true).await {
                Event::Woken => {
                    if let News::Exit = self.deliver_news(input).await {
                        break;
                    }
                    continue;
                }
                Event::Finished(finished) => {
                    if let News::Exit = self.deliver_finished(finished, input).await {
                        break;
                    }
                    continue;
                }
                Event::Input(Some(Input::Background(agent))) => {
                    self.move_to_background(&agent);
                    continue;
                }
                Event::Question(question) => {
                    // No turn runs: no one waits for the answer.
                    drop(question);
                    continue;
                }
                Event::Request(request) => {
                    Box::pin(self.request(request)).await;
                    // It switched between Chat and Agent: it opens again.
                    if self.reopen {
                        break;
                    }
                    // The cache is kept warm with a message.
                    if std::mem::take(&mut self.refresh_turn) {
                        self.feed
                            .user(REFRESH_MESSAGE, Source::App, Delivery::Refresh);
                        let refresh = Prompt::User(REFRESH_MESSAGE.into());
                        if let TurnEnd::Exit = self.turn(refresh, input).await {
                            break;
                        }
                        continue;
                    }
                    // An edited message, or one sent again.
                    let Some((index, text)) = self.edited.take() else {
                        continue;
                    };
                    self.cut_back = Some(index);
                    let shown = std::mem::take(&mut self.pending_refs);
                    self.feed
                        .user_with(&text, Source::App, Delivery::Turn, shown);
                    if let TurnEnd::Exit = self.turn(Prompt::User(text), input).await {
                        break;
                    }
                    if let News::Exit = self.deliver_news(input).await {
                        break;
                    }
                    continue;
                }
                Event::Input(None | Some(Input::Eof)) => {
                    // Without a terminal, the workers finish first.
                    if self.queue_lines {
                        self.finish_workers(input).await;
                    }
                    break;
                }
                Event::Input(Some(Input::Quit(agreed))) => {
                    if self.mirrored(|harness| harness.may_exit(agreed)) {
                        break;
                    }
                    continue;
                }
                Event::Input(Some(Input::Interrupt)) => {
                    if last_interrupt.is_some_and(|at| at.elapsed() < Duration::from_secs(3)) {
                        let shown = self.jobs_shown.clone();
                        if self.mirrored(|harness| harness.may_exit(Some(shown))) {
                            break;
                        }
                        continue;
                    }
                    last_interrupt = Some(Instant::now());
                    self.output
                        .line(&self.output.dim("(Ctrl-C again, Ctrl-D or /exit to quit)"));
                    continue;
                }
                Event::Input(Some(Input::Line(line))) => {
                    (input.coalesce(line).await, self.input_source)
                }
                Event::Input(Some(Input::Message(message))) => match self.message_text(message) {
                    Some(text) => (text, self.input_source),
                    None => continue,
                },
                Event::Input(Some(Input::Answer { .. })) => {
                    self.feed
                        .notice(Level::Warn, "That question is no longer asked.");
                    continue;
                }
                Event::Remote(Action::Message(text)) => {
                    self.echo_web(&text);
                    (text, Source::Web)
                }
                // (The page's stop never quits Arko.)
                Event::Remote(Action::Stop) => {
                    self.feed.notice(Level::Info, "No turn is running.");
                    continue;
                }
                Event::Remote(Action::Answer { .. }) => {
                    self.feed
                        .notice(Level::Warn, "That question is no longer asked.");
                    continue;
                }
            };
            let text = line.trim();
            if text.is_empty() && self.pending_images.is_empty() {
                continue;
            }
            if let Some(command) = command(text) {
                // (Images go with messages only.)
                self.pending_images.clear();
                self.pending_refs.clear();
                if self.refused(command, source) {
                    continue;
                }
                if is_remote_command(command) {
                    self.remote_command(command).await;
                    continue;
                }
                if self.live_command(command, source) {
                    continue;
                }
                self.feed.user(command, source, Delivery::Command);
                if let Some(argument) = conversations::resume_argument(command) {
                    self.resume_command(argument).await;
                    continue;
                }
                let quit = self.mirrored(|harness| match command {
                    "/exit" | "/quit" => harness.may_exit(Some(harness.jobs_shown.clone())),
                    "/new" => {
                        harness.new_conversation();
                        false
                    }
                    other => {
                        harness.command(other);
                        false
                    }
                });
                if quit {
                    break;
                }
                continue;
            }
            let shown = std::mem::take(&mut self.pending_refs);
            self.feed.user_with(text, source, Delivery::Turn, shown);
            if let TurnEnd::Exit = self.turn(Prompt::User(text.to_string()), input).await {
                break;
            }
            if let News::Exit = self.deliver_news(input).await {
                break;
            }
        }
    }

    /// Prints `text` (dim, or as a warning or an error) and shows it on the
    /// remote page.
    pub(crate) fn say(&self, level: Level, text: &str) {
        let out = &self.output;
        out.echo(&match level {
            Level::Info => out.dim(text),
            Level::Warn => out.warn(text),
            Level::Error => out.error(text),
        });
        self.feed.notice(level, text);
    }

    /// Shows in the terminal what was typed on the remote page.
    pub(crate) fn echo_web(&self, text: &str) {
        let out = &self.output;
        out.echo(&format!(
            "{} {}",
            out.accent(&format!("web {}", ui::PROMPT.trim_end())),
            text.trim().replace('\n', "\n  ")
        ));
    }

    /// Whether the command can't be run from where it was typed: the page
    /// can't quit Arko or turn itself off.
    pub(crate) fn refused(&self, command: &str, source: Source) -> bool {
        if source == Source::Web
            && (matches!(command, "/exit" | "/quit") || is_remote_command(command))
        {
            self.say(
                Level::Warn,
                &format!("  {command} works at the terminal only, not from the web page."),
            );
            return true;
        }
        false
    }

    /// Runs `run` with what the harness prints shown on the remote page
    /// too, as a command's output.
    pub(crate) fn mirrored<R>(&mut self, run: impl FnOnce(&mut Self) -> R) -> R {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let tee = Tee {
            to: self.output.clone(),
            lines: lines.clone(),
        };
        // In colour, so that each line's level (a warning, an error) is
        // known; Tee prints it plain where the terminal has no colour.
        let mirror = Arc::new(Output::new(Some(Box::new(tee)), false).colored());
        let output = std::mem::replace(&mut self.output, mirror);
        let result = run(self);
        self.output = output;
        // The lines of one level go together, as one block of output.
        let lines = std::mem::take(&mut *lock(&lines));
        let mut block: Vec<String> = Vec::new();
        let mut level = None;
        for (line_level, line) in lines {
            if line_level != level && !block.is_empty() {
                self.feed
                    .output_at(&remote::plain(&block.join("\n")), level);
                block.clear();
            }
            level = line_level;
            block.push(line);
        }
        if !block.is_empty() {
            self.feed
                .output_at(&remote::plain(&block.join("\n")), level);
        }
        result
    }

    /// `/remote`: starts the page (on the address given, else where it was
    /// last, else 127.0.0.1), or shows its links again; `/remote off` stops
    /// it, and `/remote new-key` makes a new key, which ends the page's
    /// sessions.
    pub(crate) async fn remote_command(&mut self, command: &str) {
        let argument = command.strip_prefix("/remote").unwrap_or_default().trim();
        match argument {
            "" if self.remote.is_some() => {
                if let Some(addr) = self.remote.as_ref().map(remote::Server::addr) {
                    match self.remote_key(false) {
                        Ok(key) => self.announce_remote(addr, &key),
                        Err(error) => self.say(Level::Error, &format!("arko: {error}")),
                    }
                }
            }
            "" | "on" => {
                let bind = self.remote_bind.map(|(bind, _)| bind).unwrap_or_default();
                self.start_remote(bind).await;
            }
            qr if qr == "qr" || qr.starts_with("qr ") => self.remote_qr(qr[2..].trim()),
            "off" => {
                if self.remote.is_some() {
                    self.feed
                        .notice(Level::Warn, "The web page was turned off at the terminal.");
                    self.stop_remote().await;
                    self.output.line(&self.output.dim(
                        "The web page is off: its links and sessions no longer work. /remote starts it again.",
                    ));
                } else {
                    self.output.line("The web page isn't on.");
                }
            }
            "new-key" => {
                if let Err(error) = self.remote_key(true) {
                    self.say(Level::Error, &format!("arko: {error}"));
                    return;
                }
                match self.remote_bind.filter(|_| self.remote.is_some()) {
                    // On the same port: the old links are dead.
                    Some((bind, _)) => {
                        self.feed.notice(
                            Level::Warn,
                            "The session's key was changed at the terminal.",
                        );
                        self.start_remote(bind).await;
                    }
                    None => self
                        .output
                        .line("The session has a new key. /remote starts the web page with it."),
                }
            }
            other => match remote::parse_bind(other) {
                Ok(bind) => self.start_remote(bind).await,
                Err(error) => self.output.line(&self.output.error(&format!(
                    "{error}. Or: /remote qr, /remote off, /remote new-key"
                ))),
            },
        }
    }

    /// The session's key (a new one when `new`).
    pub(crate) fn remote_key(&self, new: bool) -> Result<String, String> {
        let path = self.session.dir.join(remote::KEY_FILE);
        if new {
            remote::new_key(&path)
        } else {
            remote::session_key(&path)
        }
    }

    /// Serves the web page where `bind` says, in place of the one running,
    /// and prints its links.
    pub(crate) async fn start_remote(&mut self, bind: remote::Bind) {
        self.stop_remote().await;
        let key = match self.remote_key(false) {
            Ok(key) => key,
            Err(error) => {
                self.output.line(
                    &self
                        .output
                        .error(&format!("arko: the web page is off: {error}")),
                );
                return;
            }
        };
        let info = remote::Info {
            model: self.settings.describe(&self.settings.main),
            workspace: self.settings.workspace.display().to_string(),
            session: self.session.id.clone(),
        };
        // A page served before is served on its port again, if it is free.
        let earlier = self.remote_bind.map(|(_, port)| port);
        match remote::start(
            bind,
            earlier,
            key.clone(),
            info,
            self.feed.clone(),
            self.remote_actions.clone(),
            self.gauges.clone(),
        ) {
            Ok(server) => {
                self.remote_bind = Some((bind, server.addr().port()));
                let (links, note) = remote::links(server.addr(), &key);
                // In the status before the links are said: a view that waits
                // for the page finds it as they come.
                *lock(&self.gauges.page) = Some(remote::Page {
                    addr: server.addr(),
                    links,
                    note,
                });
                self.gauges.touch();
                self.announce_remote(server.addr(), &key);
                self.remote = Some(server);
            }
            Err(error) => self.output.line(
                &self
                    .output
                    .error(&format!("arko: the web page is off: {error}")),
            ),
        }
    }

    /// Stops the web page: its streams end, and its sessions with them.
    pub(crate) async fn stop_remote(&mut self) {
        if let Some(server) = self.remote.take() {
            lock(&self.gauges.page).take();
            self.gauges.touch();
            server.stop().await;
        }
    }

    /// Prints the links to the page (with the key: never logged), best
    /// first, and a QR code of the first that other devices can open.
    pub(crate) fn announce_remote(&self, addr: std::net::SocketAddr, key: &str) {
        let out = &self.output;
        let (links, note) = remote::links(addr, key);
        let mut lines = Vec::new();
        for (n, link) in links.iter().enumerate() {
            let interface = link
                .interface
                .as_ref()
                .map(|name| out.dim(&format!("  ({name}, {})", link.kind.label())))
                .unwrap_or_default();
            lines.push(format!(
                "{} {}{interface}",
                if n == 0 {
                    out.bold("Web page:")
                } else {
                    " ".repeat("Web page:".len())
                },
                link.url
            ));
        }
        out.line(&lines.join("\n"));
        if let Some(note) = note {
            out.line(&out.warn(&note));
        }
        if links.iter().all(|link| link.loopback) {
            out.line(&out.dim(
                "Only this machine can open it: for a phone, /remote 0.0.0.0 (the network's devices, Tailscale's too), or an SSH tunnel.",
            ));
        }
        out.line(&out.warn(
            "The link holds the session's key: whoever has it can follow this session and act in it. No TLS: on a network you don't trust, use an SSH tunnel or Tailscale. /remote off stops the page.",
        ));
        let others = links.iter().filter(|link| !link.loopback).count();
        if glyphs::symbols() != glyphs::Symbols::Ascii
            && let Some(link) = links.iter().find(|link| !link.loopback)
            && let Some(code) = remote::qr_lines(&link.url)
        {
            if others > 1 {
                out.echo(&out.dim(&format!(
                    "The QR code opens the first link{}. /remote qr INTERFACE shows another.",
                    place(link)
                )));
            }
            // (The full-screen view shows it in an overlay that stays.)
            out.echo(&code.join("\n"));
        }
    }

    /// `/remote qr [INTERFACE]`: the QR code of the page's first link that
    /// other devices can open, or of the one on `interface` (a name or an
    /// address).
    pub(crate) fn remote_qr(&self, interface: &str) {
        let out = &self.output;
        let Some(addr) = self.remote.as_ref().map(remote::Server::addr) else {
            out.line("The web page isn't on: /remote starts it.");
            return;
        };
        let key = match self.remote_key(false) {
            Ok(key) => key,
            Err(error) => {
                out.line(&out.error(&format!("arko: {error}")));
                return;
            }
        };
        let (links, note) = remote::links(addr, &key);
        if interface.is_empty()
            && let Some(note) = note
        {
            out.line(&out.warn(&note));
        }
        let link = match qr_link(&links, interface, addr) {
            Ok(link) => link,
            Err(error) => {
                out.line(&out.error(&error));
                return;
            }
        };
        if glyphs::symbols() == glyphs::Symbols::Ascii {
            out.line(&format!(
                "With ARKO_SYMBOLS=ascii the terminal shows no QR code. The link{}: {}",
                place(link),
                link.url
            ));
            return;
        }
        match remote::qr_lines(&link.url) {
            Some(code) => {
                out.line(&format!("{}{}", link.url, out.dim(&place(link))));
                out.line(&code.join("\n"));
            }
            None => out.line(&out.error("The link is too long for a QR code.")),
        }
    }

    /// Runs `command` when it changes a setting while the session runs
    /// (`/outside`); returns whether it was one. A change from the web
    /// page is announced in the terminal and on the page, so that whoever
    /// is at the terminal knows.
    pub(crate) fn live_command(&mut self, command: &str, source: Source) -> bool {
        let Some((name, argument)) = split_live_command(command) else {
            return false;
        };
        // (It shows as the user's message to the worker, not as a command.)
        if name == "/message" {
            self.message_command(argument, source);
            return true;
        }
        self.feed.user(command, source, Delivery::Command);
        match name {
            "/outside" => self.outside_command(argument, source),
            "/model" => self.model_command(argument, source),
            "/keys" => self.keys_command(argument, source),
            "/handover" => self.handover_command(argument, source),
            "/text-replies" => self.text_replies_command(argument, source),
            "/cc-compact" => self.cc_compact_command(argument, source),
            "/extra-rules" => self.extra_rules_command(argument, source),
            "/project-instructions" => self.project_command(argument, source),
            "/background-after" => self.background_after_command(argument, source),
            _ => self.handover_at_command(argument, source),
        }
        // (A setting it changed shows in the status and the details.)
        self.gauges.touch();
        self.publish_details();
        true
    }

    /// Does what the front end asks, and answers it.
    pub(crate) async fn request(&mut self, request: crate::engine::Request) {
        use crate::engine::Request;
        // While a turn runs, the agent's configuration stays as it is; while
        // Arko only waits for the workers (no terminal), nothing starts.
        let busy = if self.feed.activity().working {
            Some("Wait for the turn to end.")
        } else if self.only_workers {
            Some("Arko is only waiting for the workers now.")
        } else {
            None
        };
        let working = busy.is_some();
        let wait = busy.unwrap_or_default();
        match request {
            Request::Rename { title, reply } => {
                let renamed = self.session.rename(&title);
                if renamed.is_ok() {
                    self.publish_title();
                }
                let _ = reply.send(renamed);
            }
            Request::ChatInstructions { reply, .. }
            | Request::SwitchMode { reply, .. }
            | Request::Edit { reply, .. }
            | Request::Regenerate { reply }
                if working =>
            {
                let _ = reply.send(Err(wait.into()));
            }
            Request::Branch { reply, .. } if working => {
                let _ = reply.send(Err(wait.into()));
            }
            Request::RefreshCache { reply } if working => {
                let _ = reply.send(Err(wait.into()));
            }
            Request::RefreshCache { reply } => {
                let _ = reply.send(self.refresh_cache().await);
            }
            Request::Branch { before, reply } => {
                let _ = reply.send(self.branch(before));
            }
            Request::Edit {
                item,
                message,
                reply,
            } => {
                let edited = self
                    .attach(message.images)
                    .and_then(|()| self.edit(item, message.text));
                if edited.is_err() {
                    self.pending_images.clear();
                    self.pending_refs.clear();
                }
                let _ = reply.send(edited);
            }
            Request::Regenerate { reply } => {
                let _ = reply.send(self.regenerate());
            }
            Request::ChatInstructions { text, reply } => {
                let _ = reply.send(self.set_chat_instructions(text));
            }
            Request::SwitchMode {
                to,
                workspace,
                reply,
            } => {
                let _ = reply.send(self.switch_mode(to, workspace));
            }
        }
    }

    /// How the conversation goes on: in the session's mode, when it was in
    /// the other (the session switched between Chat and Agent), with the
    /// agent's configuration now, in the session's folder.
    fn continue_options(&self, conversation: &Conversation) -> ContinueOptions {
        let options = ContinueOptions::new();
        let result = match conversation {
            Conversation::New => return options,
            Conversation::Answered(result) => result,
            // A run recovered takes no message, and one reopened keeps its
            // configuration: the message after it changes it.
            Conversation::Stopped(run) if run.restart != Restart::Continue => return options,
            Conversation::Stopped(run) => &run.result,
        };
        let chat = result.task.agent_config.tool_config.chat;
        if !self.keeps_run_configs() || chat == (self.settings.mode == Mode::Chat) {
            return options;
        }
        let options = options.reconfigure(self.main_config.clone());
        if self.settings.mode == Mode::Agent
            && result.task.current_working_directory != self.settings.workspace
        {
            options.workspace(&self.settings.workspace)
        } else {
            options
        }
    }

    /// The user's message `item` started the turn just made, at message
    /// `at` of the conversation or after it (or, when the conversation was
    /// handed over during the turn, as the fresh one's latest with its
    /// words): it can be edited at the index the task gives it.
    fn mark_message(&self, item: u64, at: usize) {
        let (Some(task), Some(item)) = (self.conversation_task(), self.feed.main_item(item)) else {
            return;
        };
        let words = item.text.trim();
        let turns: Vec<arko_agents::UserTurn> = task
            .user_turns()
            .into_iter()
            .filter(|turn| {
                turn.kind != arko_agents::UserTurnKind::ContextUpdate && turn.text.contains(words)
            })
            .collect();
        let turn = turns
            .iter()
            .find(|turn| turn.index >= at)
            .or_else(|| turns.last());
        if let Some(turn) = turn {
            self.feed.mark_message(item.id, turn.index);
        }
    }

    /// The user's message `item` edited to `text`: the conversation is cut
    /// back to just before it, and `text` starts a turn in its place (in
    /// `repl`). Refused, saying why, while a worker runs, and for a message
    /// the conversation no longer has (one before a handover, or of
    /// another conversation).
    fn edit(&mut self, item: u64, text: String) -> Result<(), String> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err("The message is empty.".into());
        }
        if command(&text).is_some() {
            return Err("A command can't take a message's place.".into());
        }
        if let Some(crew) = &self.crew
            && let Some(worker) = crew.running().first()
        {
            return Err(format!(
                "{worker} is running: wait for the workers or stop them first."
            ));
        }
        let index = self.message_at(item, "edited")?;
        self.feed.truncate(item);
        // What the user typed during the turns cut goes with them.
        self.unread.clear();
        self.edited = Some((index, text));
        Ok(())
    }

    /// Where the user's message `item` is in the agent's conversation now
    /// (else why it isn't, for a message to be `done`: "edited").
    fn message_at(&self, item: u64, done: &str) -> Result<usize, String> {
        let gone = "The agent's conversation no longer has this message (it was handed over or compacted since, or it is another conversation's): choose a later message, or start a new conversation.";
        let Some(item) = self
            .feed
            .main_item(item)
            .filter(|item| item.kind == remote::Kind::User)
        else {
            return Err(format!("This message can't be {done}."));
        };
        let (index, words) = match item.message_index {
            Some(index) => (index, item.text),
            // A message that started a turn of an earlier conversation.
            None if item.delivery == Some(Delivery::Turn) => return Err(gone.into()),
            None => return Err(format!("This message can't be {done}.")),
        };
        let there = self.conversation_task().is_some_and(|task| {
            task.user_turns().into_iter().any(|turn| {
                turn.index == index
                    && turn.kind != arko_agents::UserTurnKind::ContextUpdate
                    && turn.text.contains(words.trim())
            })
        });
        if there { Ok(index) } else { Err(gone.into()) }
    }

    /// The task of the conversation the session is in, once it started.
    fn conversation_task(&self) -> Option<&arko_agents::Task> {
        match &self.conversation {
            Conversation::New => None,
            Conversation::Answered(result) => Some(&result.task),
            Conversation::Stopped(run) => Some(&run.result.task),
        }
    }

    /// A new session holding the conversation up to just before the user's
    /// message `before`, or all of it: its ID. It takes this session's
    /// settings, and its conversation from its seed when it opens (see
    /// `import_seed`).
    fn branch(&self, before: Option<u64>) -> Result<String, String> {
        // (Its workers stay here: the branch's conversation would speak of
        // workers it doesn't have.)
        if let Some(crew) = &self.crew
            && let Some(worker) = crew.running().first()
        {
            return Err(format!(
                "{worker} is running: wait for the workers or stop them first."
            ));
        }
        let task = self
            .conversation_task()
            .ok_or("A branch starts from a conversation: send a message first.")?;
        let index = match before {
            Some(item) => self.message_at(item, "branched from")?,
            None => task.conversation.messages.len(),
        };
        if index == 0 {
            return Err("A branch keeps the conversation up to a message, and this is its first: start a new session instead.".into());
        }
        let branch = task.branch(index).map_err(|error| error.to_string())?;
        let seed = serde_json::to_string(&branch)
            .map_err(|error| format!("could not write the branch's conversation: {error}"))?;
        let session = Session::create(&self.root)
            .map_err(|error| format!("could not create a session: {error}"))?;
        let mut settings = self.settings.clone();
        settings.created = chrono::Utc::now();
        settings.branched_from = Some(self.session.id.clone());
        let title = lock(&self.gauges.title).clone();
        settings.title = Some(format!("Branch of {title}"));
        // A chat works in its own folder.
        if settings.workspace == self.session.dir {
            settings.workspace = session.dir.clone();
        }
        let written = session
            .save_settings(&settings)
            .and_then(|()| std::fs::write(session.dir.join(crate::session::SEED_FILE), seed));
        if let Err(error) = written {
            let _ = std::fs::remove_dir_all(&session.dir);
            return Err(format!("could not save the branch: {error}"));
        }
        Ok(session.id)
    }

    /// A branch's first opening: its conversation, from its seed, becomes
    /// its main agent's run, saved in a checkpoint (then the seed goes).
    pub(crate) async fn import_seed(&mut self) -> Result<(), String> {
        let seed = self.session.dir.join(crate::session::SEED_FILE);
        if !seed.exists() {
            return Ok(());
        }
        if self.session.latest_checkpoint().is_some() {
            // Saved already: the checkpoint has it.
            let _ = std::fs::remove_file(&seed);
            return Ok(());
        }
        let unreadable =
            |error: String| format!("could not read the branch's conversation: {error}");
        let text = std::fs::read_to_string(&seed).map_err(|error| unreadable(error.to_string()))?;
        let task: arko_agents::Task =
            serde_json::from_str(&text).map_err(|error| unreadable(error.to_string()))?;
        let workflow = self.workflow().clone();
        let result = workflow
            .import_agent_run(MAIN, task)
            .map_err(|error| error.to_string())?;
        workflow
            .checkpoint()
            .await
            .map_err(|error| format!("could not save the branch: {error}"))?;
        let _ = std::fs::remove_file(&seed);
        self.conversation = Conversation::Answered(result);
        self.track_conversations();
        Ok(())
    }

    /// Keeps the main agent's prompt cache warm: out of band where the
    /// provider allows, else with a short message and its reply (in
    /// `repl`).
    async fn refresh_cache(&mut self) -> Result<crate::engine::Refreshed, String> {
        let result = match &self.conversation {
            Conversation::New => {
                return Err("There is no conversation to keep warm yet.".into());
            }
            Conversation::Answered(result) => result.clone(),
            Conversation::Stopped(run) => run.result.clone(),
        };
        let workflow = self.workflow().clone();
        // The cache lasts from the request's start, as a turn's does.
        let started = chrono::Utc::now().timestamp_millis();
        // (Boxed: the session's loop would hold it on its stack.)
        match Box::pin(workflow.refresh_prompt_cache(&result)).await {
            Ok(refresh) => {
                let usage = refresh.usage;
                let (read, written) = (
                    usage.cached_tokens.unwrap_or(0),
                    usage.cache_write_tokens.unwrap_or(0),
                );
                {
                    let mut stats = lock(&self.stats);
                    stats.cache_refreshed_at = Some(started);
                    stats.cache_use = Some((read, written));
                }
                self.gauges.touch();
                Ok(crate::engine::Refreshed::OutOfBand {
                    read,
                    written,
                    cost: usage.billed_cost.unwrap_or(usage.total_cost),
                })
            }
            Err(WorkflowError::Agent(arko_agents::ArkoAgentsError::LLM(
                arko_ai::Error::UnsupportedFeature(_),
            ))) => {
                self.refresh_turn = true;
                Ok(crate::engine::Refreshed::Message)
            }
            Err(error) => Err(format!("The cache wasn't refreshed: {error}")),
        }
    }

    /// The user's latest message sent again: its turn's reply and what
    /// followed are cut, and a new reply comes.
    fn regenerate(&mut self) -> Result<(), String> {
        let Some(item) = self.feed.last_turn_message() else {
            return Err("There is no message to send again.".into());
        };
        let index = self.message_at(item.id, "sent again")?;
        // Its images go again, as the conversation has them.
        let images: Vec<arko_ai::Image> = self
            .conversation_task()
            .and_then(|task| task.conversation.messages.get(index))
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(arko_ai::Content::as_image)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        self.edit(item.id, item.text)?;
        self.pending_images = images;
        self.pending_refs = item.images;
        Ok(())
    }

    /// Whether the main agent's saved conversations each resume with their
    /// own configuration, and new ones take the session's now: a chat's
    /// (whose instructions change between conversations), and a session's
    /// that switched between Chat and Agent.
    pub(crate) fn keeps_run_configs(&self) -> bool {
        self.settings.mode == Mode::Chat
            || self.settings.switched_mode
            || self.settings.branched_from.is_some()
    }

    /// The chat's instructions from the user: from its first message, or
    /// else from its next new conversation (the one it is in keeps its
    /// own).
    fn set_chat_instructions(&mut self, text: Option<String>) -> Result<(), String> {
        if self.settings.mode != Mode::Chat {
            return Err("This session isn't a chat.".into());
        }
        let text = text.filter(|text| !text.trim().is_empty());
        if text == self.settings.chat_instructions {
            return Ok(());
        }
        let mut settings = self.settings.clone();
        settings.chat_instructions = text;
        if !self.conversation_started() {
            return self.rebuild_agents(settings);
        }
        let setup = self.providers.clone();
        let (main_config, worker_config) = self.agent_configs(&settings, &setup)?;
        self.settings = settings;
        self.main_config = main_config;
        self.worker_config = worker_config;
        self.save_session();
        self.reopen_workflow()?;
        self.publish_details();
        self.say(
            Level::Info,
            "The chat's instructions changed: its next new conversation takes them (/new).",
        );
        Ok(())
    }

    /// The workflow again, from its latest checkpoint, with the agents'
    /// configurations now (between turns): the conversations go on where
    /// they are.
    fn reopen_workflow(&mut self) -> Result<(), String> {
        // As it is now: `/new` may have left the conversation it was in.
        let new = matches!(self.conversation, Conversation::New);
        self.save_conversations();
        self.open(self.session.latest_checkpoint().is_some())?;
        if new {
            self.conversations.current = None;
            self.conversation = Conversation::New;
        }
        self.start_new_workers();
        self.publish_conversations();
        Ok(())
    }

    /// Makes the session a chat or an agent's (`to`), working in
    /// `workspace`: refused, saying why, while workers run or their news
    /// waits, and (to Chat) while jobs run. The session then closes, to be
    /// opened again in its new mode; its next message reconfigures the
    /// conversation (see `turn`).
    fn switch_mode(&mut self, to: Mode, workspace: Option<PathBuf>) -> Result<(), String> {
        if to == self.settings.mode {
            return Ok(());
        }
        // (They go with the next message, which a session opened again
        // wouldn't know of.)
        if !self.unread.is_empty() {
            return Err(
                "What you typed during the last turn hasn't reached the agent yet: send a message first."
                    .into(),
            );
        }
        if let Some(crew) = &self.crew {
            let running = crew.running();
            match running.as_slice() {
                [] => {}
                [one] => {
                    return Err(format!("{one} is running: wait for it or stop it first."));
                }
                many => {
                    return Err(format!(
                        "{} workers are running: wait for them or stop them first.",
                        many.len()
                    ));
                }
            }
            if crew.has_mail() {
                return Err(
                    "The workers' news hasn't reached the agent yet: send a message first.".into(),
                );
            }
        }
        let jobs = self.background.jobs();
        if to == Mode::Chat && !jobs.is_empty() {
            return Err(format!(
                "{} running in the background: wait for {} or stop {} first.",
                if jobs.len() == 1 {
                    "A job is".to_string()
                } else {
                    format!("{} jobs are", jobs.len())
                },
                if jobs.len() == 1 { "it" } else { "them" },
                if jobs.len() == 1 { "it" } else { "them" },
            ));
        }
        let mut settings = self.settings.clone();
        if to == Mode::Agent {
            // A chat that never was an agent's works in its session's folder.
            let folder = match workspace {
                Some(folder) => folder,
                None if settings.workspace != self.session.dir => settings.workspace.clone(),
                None => return Err("An agent needs a folder to work in.".into()),
            };
            let folder = std::path::absolute(&folder).map_err(|error| error.to_string())?;
            let folder = arko_agents::utils::normalize_path(&folder);
            if !folder.is_dir() {
                return Err(format!("{} is not a folder.", folder.display()));
            }
            // (A Claude Code conversation moves too: Claude Code resumes it
            // in the folder it started in, with this one added.)
            settings.workspace = folder;
            if settings.max_workers == 0 {
                settings.max_workers = crate::agents::NO_LIMIT;
                settings.worker_project_instructions = true;
                settings.project_instructions = Default::default();
            }
        }
        settings.mode = to;
        settings.switched_mode = true;
        // The agents can be built in the new mode.
        let setup = self.providers.clone();
        self.agent_configs(&settings, &setup)?;
        self.session
            .save_settings(&settings)
            .map_err(|error| format!("could not save the session: {error}"))?;
        self.settings = settings;
        self.say(
            Level::Info,
            &match to {
                Mode::Chat => {
                    "Switched to Chat: from the next message, the agent has no tools.".to_string()
                }
                Mode::Agent => format!(
                    "Switched to Agent in {}: from the next message, the agent has its tools.",
                    self.settings.workspace.display()
                ),
            },
        );
        self.reopen = true;
        Ok(())
    }

    /// The session's name, as the status gives it: the one the user gave
    /// it, else its current conversation's, else its first message's start.
    pub(crate) fn publish_title(&self) {
        let title = self
            .session
            .renamed()
            .or_else(|| {
                self.conversations
                    .current
                    .and_then(|index| self.conversations.list.get(index))
                    .map(|record| record.title.clone())
                    .filter(|title| !title.is_empty())
            })
            .or_else(|| self.settings.title.clone())
            .unwrap_or_else(|| match self.settings.mode {
                Mode::Agent => "New session".to_string(),
                Mode::Chat => "New chat".to_string(),
            });
        *lock(&self.gauges.title) = title;
        self.gauges.touch();
    }

    /// The answer `text`, from `source`, to the question `question`: given
    /// to the agent when it is the one it waits for (`answering`), else the
    /// user is told the question is no longer asked.
    pub(crate) fn answer(
        &mut self,
        answering: &mut Option<(u64, oneshot::Sender<String>)>,
        question: u64,
        text: &str,
        source: Source,
    ) {
        match answering.take() {
            Some((id, answer)) if id == question => {
                if source == Source::Web {
                    self.echo_web(text);
                }
                let given = text.trim().to_string();
                if answer.send(given.clone()).is_ok() {
                    self.output.echo(&self.output.dim("  ✎ answered"));
                    self.feed.answered(id, &given, source);
                } else {
                    self.feed.question_closed(id);
                    self.feed
                        .notice(Level::Warn, "That question is no longer asked.");
                }
            }
            other => {
                *answering = other;
                self.feed
                    .notice(Level::Warn, "That question is no longer asked.");
            }
        }
    }

    /// The text of an app's message, with its images kept to go with it,
    /// or `None` when it can't be sent (the user is told why).
    pub(crate) fn message_text(&mut self, message: ui::Message) -> Option<String> {
        match self.attach(message.images) {
            Ok(()) => Some(message.text),
            Err(why) => {
                self.say(Level::Error, &why);
                None
            }
        }
    }

    /// The images a message sends: checked for the main agent's model,
    /// kept for the transcript, and pending for the message's turn or note.
    fn attach(&mut self, attachments: Vec<ui::Attachment>) -> Result<(), String> {
        self.pending_images.clear();
        self.pending_refs.clear();
        let model = &self.main_config.llm_config.model;
        let mut images = Vec::new();
        for attachment in &attachments {
            let image = arko_ai::Image::from_bytes(&attachment.bytes, &attachment.mime);
            if let Some(why) = model.image_refusal(&image) {
                return Err(format!(
                    "The message wasn't sent: {why}. Send it without images, or switch to a model that takes them."
                ));
            }
            images.push(image);
        }
        for attachment in attachments {
            let label = attachment.name.as_deref().unwrap_or("image");
            let shown = self.images.add(attachment.bytes, &attachment.mime, label);
            self.pending_refs.push(shown);
        }
        self.pending_images = images;
        Ok(())
    }

    /// `/message WORKER TEXT`: the user's message goes to the worker as its
    /// lead's would, and the lead gets a copy with its next news.
    pub(crate) fn message_command(&mut self, argument: &str, source: Source) {
        let (to, text) = argument
            .split_once(char::is_whitespace)
            .map_or((argument, ""), |(to, text)| (to, text.trim()));
        if text.is_empty() {
            self.say(
                Level::Warn,
                "/message takes a worker and the message, such as /message r1 Check the tests too.",
            );
            return;
        }
        let sent = match &self.crew {
            Some(crew) => crew.message(to, text),
            None => Err("this session has no workers".to_string()),
        };
        match sent {
            Ok(id) => {
                self.feed.to_worker(&id, text, source);
                self.output
                    .echo(&self.output.dim(&format!("  ✉ sent to {id}")));
            }
            // (The model's advice after the reason isn't the user's.)
            Err(why) => self.say(
                Level::Warn,
                &format!(
                    "The message didn't reach {to}: {}. It said: {text}",
                    why.split("; ").next().unwrap_or_default()
                ),
            ),
        }
    }

    /// `/handover`: the main agent hands over at its next step, or when
    /// its next turn starts. In Claude Code, Claude Code compacts its
    /// conversation then instead.
    pub(crate) fn handover_command(&mut self, argument: &str, source: Source) {
        if !argument.is_empty() {
            self.mirrored(|harness| {
                harness.output.line(&harness.output.error(
                    "/handover takes nothing after it. /handover-at sets the size the agent hands over at.",
                ))
            });
            return;
        }
        let compacts = self.handover.compacts();
        let Some(asked) = self.ask_handover() else {
            let text = if compacts {
                "There is no conversation to compact: your next message starts a new one."
            } else {
                "There is no conversation to hand over: your next message starts a new one."
            };
            self.mirrored(|harness| harness.output.line(text));
            return;
        };
        if source == Source::Web && asked != handover::Asked::Already {
            let text = if compacts {
                "A compaction was asked for from the web page."
            } else {
                "A handover was asked for from the web page."
            };
            self.say(Level::Info, text);
        }
        self.mirrored(|harness| harness.output.line(handover_text(asked, compacts)));
    }

    /// `/handover-at`: the size the main agent and the workers hand over
    /// at, and with a size, sets both. `/handover-at main` and
    /// `/handover-at workers` say or set one of them.
    pub(crate) fn handover_at_command(&mut self, argument: &str, source: Source) {
        let (whose, size) = handover::whose(argument);
        let workers = self.has_workers();
        if whose == handover::Whose::Workers && !workers {
            self.mirrored(|harness| {
                harness.output.line(&harness.output.error(
                    "This session has no workers (it started with --workers 0, or with the old worker tools).",
                ))
            });
            return;
        }
        if size.is_empty() {
            let text = self.handover_at_text(whose);
            self.mirrored(|harness| harness.output.line(&text));
            return;
        }
        let tokens = match handover::parse_tokens(size) {
            Ok(tokens) => tokens,
            Err(error) => {
                self.mirrored(|harness| harness.output.line(&harness.output.error(&error)));
                return;
            }
        };
        let mut lines = Vec::new();
        let mut set = tokens;
        if whose != handover::Whose::Workers {
            set = self.set_handover_at(tokens);
            lines.extend(self.handover_set_lines(tokens, set));
        }
        if whose != handover::Whose::Main && workers {
            self.set_workers_handover_at(tokens);
            lines.push(self.workers_handover_text(Some(tokens)));
        }
        if source == Source::Web {
            let whose = match whose {
                handover::Whose::Both => "The handover size",
                handover::Whose::Main => "The main agent's handover size",
                handover::Whose::Workers => {
                    // As new workers go by it, on their model.
                    set = self.new_workers_handover_at().unwrap_or(tokens);
                    "The workers' handover size"
                }
            };
            self.say(
                Level::Info,
                &format!(
                    "{whose} was set to {} from the web page.",
                    handover::shown(set)
                ),
            );
        }
        self.mirrored(|harness| {
            for line in &lines {
                harness.output.line(line);
            }
        });
    }

    /// What `/handover-at` says of `whose` sizes, with how to change them.
    pub(crate) fn handover_at_text(&self, whose: handover::Whose) -> String {
        let compacts = self.handover.compacts();
        let mut parts = Vec::new();
        if whose != handover::Whose::Workers {
            let mut text = if compacts {
                format!(
                    "Claude Code compacts the main agent's conversation at {} tokens of input",
                    handover::shown(self.handover.at())
                )
            } else {
                format!(
                    "The main agent hands over at {} tokens of input",
                    handover::shown(self.handover.at())
                )
            };
            if let Some(input) = self.handover.input() {
                text.push_str(&format!(
                    " (its latest request: {})",
                    handover::shown(input)
                ));
            }
            parts.push(format!("{text}."));
        }
        let workers = self.has_workers();
        if whose != handover::Whose::Main && workers {
            parts.push(self.workers_handover_text(None));
        }
        let sizes = if workers {
            "/handover-at [main|workers] 200k sets another size"
        } else {
            "/handover-at 200k sets another size"
        };
        let now = if compacts {
            "/handover compacts now"
        } else {
            "/handover hands over now"
        };
        parts.push(format!("{sizes}, and {now}."));
        parts.join(" ")
    }

    /// The size new workers hand over at, on their model: the workers'
    /// size the user set, else their model's own, up to what the model
    /// leaves room for.
    pub(crate) fn new_workers_handover_at(&self) -> Option<usize> {
        self.handover.worker_at(
            &self.worker_config.llm_config.model,
            self.worker_config
                .handover
                .as_ref()
                .and_then(|handover| handover.at_input_tokens),
        )
    }

    /// What the user is told of the workers' size: `set` just now, or (with
    /// `None`) as it is. The models of the workers that go by less (theirs
    /// leave less room) are named.
    pub(crate) fn workers_handover_text(&self, set: Option<usize>) -> String {
        let compacts = agents::compacts(&self.worker_config);
        let verb = |size: usize| {
            let now = if set.is_some() { "now " } else { "" };
            if compacts {
                format!(
                    "Claude Code {now}compacts the workers' conversations at {} tokens of input",
                    handover::shown(size)
                )
            } else {
                format!(
                    "Workers {now}hand over at {} tokens of input",
                    handover::shown(size)
                )
            }
        };
        let Some(asked) = set.or(self.handover.workers_at()) else {
            // As each worker's model says, by default.
            let size = self
                .new_workers_handover_at()
                .unwrap_or(self.settings.handover_at);
            return format!("{}, their model's own size.", verb(size));
        };
        // New workers' model first, then those of the workers started.
        let mut models = vec![self.worker_config.llm_config.model.clone()];
        if let Some(crew) = &self.crew {
            for model in crew.run_models() {
                if !models
                    .iter()
                    .any(|known| known.id == model.id && known.provider == model.provider)
                {
                    models.push(model);
                }
            }
        }
        let capped: Vec<(String, usize)> = models
            .iter()
            .filter_map(|model| {
                let most = self.handover.worker_most(model)?;
                (most < asked).then(|| (providers::model_name(model), most))
            })
            .collect();
        let running = set.is_some()
            && self
                .crew
                .as_ref()
                .is_some_and(|crew| !crew.running().is_empty());
        let since = if running {
            ", those running from their next step"
        } else {
            ""
        };
        if capped.len() == models.len() && capped.iter().all(|(_, most)| *most == capped[0].1) {
            return format!(
                "{}{since}, the largest size Arko allows for their model.",
                verb(capped[0].1)
            );
        }
        let mut text = format!("{}{since}.", verb(asked));
        for (name, most) in capped {
            text.push_str(&format!(
                " Those on {name} go by {}, the largest size Arko allows for that model.",
                handover::shown(most)
            ));
        }
        text
    }

    /// Asks the main agent for a handover: at its next step while it
    /// works, else as its next turn starts. `None` when there is no
    /// conversation to hand over.
    pub(crate) fn ask_handover(&self) -> Option<handover::Asked> {
        if self.feed.activity().working {
            return Some(self.handover.ask_now());
        }
        let run = match &self.conversation {
            Conversation::New => return None,
            Conversation::Answered(result) => result.agent_run_id,
            Conversation::Stopped(run) => run.result.agent_run_id,
        };
        Some(self.handover.ask_next(run))
    }

    /// `--handover-at` when resuming: the size the main agent and the
    /// workers hand over at from now on.
    pub(crate) fn handover_at_start(&mut self, tokens: usize) {
        let set = self.set_handover_at(tokens);
        let mut lines = self.handover_set_lines(tokens, set);
        if self.has_workers() {
            self.set_workers_handover_at(tokens);
            lines.push(self.workers_handover_text(Some(tokens)));
        }
        for line in lines {
            self.say(Level::Info, &line);
        }
    }

    /// The workers' handover size when resuming (`Options::workers_handover_at`).
    pub(crate) fn workers_handover_at_start(&mut self, tokens: usize) {
        if self.has_workers() {
            self.set_workers_handover_at(tokens);
            let line = self.workers_handover_text(Some(tokens));
            self.say(Level::Info, &line);
        }
    }

    /// Sets the size the workers hand over at, larger or smaller, each up
    /// to what its own model leaves room for, and keeps it in the session.
    pub(crate) fn set_workers_handover_at(&mut self, tokens: usize) {
        self.handover.set_workers_at(tokens);
        self.settings.workers_handover_at = Some(tokens);
        if let Err(error) = self.session.save_settings(&self.settings) {
            self.say(
                Level::Error,
                &format!("arko: could not save the session's settings: {error}"),
            );
        }
    }

    /// Sets the size the main agent hands over at, larger or smaller, up to
    /// the largest its model leaves room for, and keeps it in the session.
    /// Returns the size set.
    pub(crate) fn set_handover_at(&mut self, tokens: usize) -> usize {
        let set = self.handover.set_at(tokens);
        self.settings.main_handover_at = Some(set);
        if let Err(error) = self.session.save_settings(&self.settings) {
            self.say(
                Level::Error,
                &format!("arko: could not save the session's settings: {error}"),
            );
        }
        set
    }

    /// What the user is told when `tokens` was asked for and `set` set;
    /// asks for the handover when the main agent's context is over it.
    pub(crate) fn handover_set_lines(&self, tokens: usize, set: usize) -> Vec<String> {
        let compacts = self.handover.compacts();
        let shown = handover::shown(set);
        let mut lines = vec![match (set < tokens, compacts) {
            (true, false) => format!(
                "The main agent now hands over at {shown} tokens of input, the largest size Arko allows for its model."
            ),
            (true, true) => format!(
                "Claude Code now compacts the main agent's conversation at {shown} tokens of input, the largest size Arko allows for its model."
            ),
            (false, false) => {
                format!("The main agent now hands over at {shown} tokens of input.")
            }
            (false, true) => format!(
                "Claude Code now compacts the main agent's conversation at {shown} tokens of input."
            ),
        }];
        if let Some(input) = self.handover.input().filter(|input| *input >= set)
            && let Some(asked) = self.ask_handover()
        {
            lines.push(format!(
                "Its context ({}) is over that already. {}",
                handover::shown(input),
                handover_text(asked, compacts)
            ));
        }
        lines
    }

    /// `/outside`: says whether outside access is on; `/outside on|off`
    /// turns it on or off.
    pub(crate) fn outside_command(&mut self, argument: &str, source: Source) {
        let on = match outside::parse(argument) {
            Ok(Some(on)) => on,
            Ok(None) => {
                self.mirrored(|harness| harness.output.line(&harness.outside.describe()));
                return;
            }
            Err(error) => {
                self.mirrored(|harness| harness.output.line(&harness.output.error(&error)));
                return;
            }
        };
        let state = if on { "on" } else { "off" };
        if self.outside.is_on() == on {
            self.mirrored(|harness| {
                harness
                    .output
                    .line(&format!("Outside access is already {state}."))
            });
            return;
        }
        if source == Source::Web {
            self.say(
                if on { Level::Warn } else { Level::Info },
                &format!("Outside access turned {state} from the web page."),
            );
        }
        self.set_outside(on);
        self.mirrored(|harness| {
            let text = harness.outside.describe();
            harness
                .output
                .line(&if on { harness.output.warn(&text) } else { text })
        });
    }

    /// Outside access as the session starts. The agents' instructions
    /// leave it to a note: a new session started with it on says so, and
    /// `--allow-outside-workspace` (`flag`) turns it on in a resumed one.
    /// (The banner tells the user.)
    pub(crate) fn outside_at_start(&mut self, new: bool, flag: bool) {
        if new && self.outside.is_on() {
            self.tell_agents(&self.outside.note());
        } else if !new && flag {
            self.set_outside(true);
        }
    }

    /// Turns outside access on or off: the command guard, the file tools
    /// and new workers follow at once, the session keeps it, and the agents
    /// get a note. Returns whether it changed.
    pub(crate) fn set_outside(&mut self, on: bool) -> bool {
        if !self.outside.set(on) {
            return false;
        }
        self.settings.allow_outside_workspace = on;
        if let Err(error) = self.session.save_settings(&self.settings) {
            self.say(
                Level::Error,
                &format!("arko: could not save the session's settings: {error}"),
            );
        }
        self.tell_agents(&self.outside.note());
        true
    }

    /// A note to the agents from Arko: the running ones read it at their
    /// next step, and later runs (workers, conversations, handovers) in
    /// their first message.
    pub(crate) fn tell_agents(&self, text: &str) {
        if let Err(error) = self
            .workflow()
            .post_note(Note::new(text).source(NoteSource::Host))
        {
            self.say(
                Level::Error,
                &format!("arko: could not tell the agents: {error}"),
            );
        }
    }

    /// `/background-after`: when shell commands move to the background, and
    /// with a time (or `off`), sets it.
    pub(crate) fn background_after_command(&mut self, argument: &str, source: Source) {
        let now = self.background.after().map_or(0, |after| after.as_secs());
        if argument.is_empty() {
            let mut text = background_after_text(now);
            text.push_str(if now == 0 {
                " /background-after 10m moves them after 10 minutes."
            } else {
                " /background-after off keeps them in the foreground."
            });
            self.mirrored(|harness| harness.output.line(&text));
            return;
        }
        let after = match shell::parse_background_after(argument) {
            Ok(after) => after,
            Err(error) => {
                self.mirrored(|harness| harness.output.line(&harness.output.error(&error)));
                return;
            }
        };
        if after == now {
            let text = format!("No change: {}", background_after_text(after));
            self.mirrored(|harness| harness.output.line(&text));
            return;
        }
        if source == Source::Web {
            self.say(
                Level::Info,
                &format!(
                    "Background after was set to {} from the web page.",
                    shell::shown_background_after(after)
                ),
            );
        }
        self.set_background_after(after);
        let text = background_after_text(after);
        self.mirrored(|harness| harness.output.line(&text));
    }

    /// Sets when shell commands move to the background: the commands
    /// started from now on follow, the session keeps it, and the agents get
    /// a note (their shell's description says what it was as Arko started).
    pub(crate) fn set_background_after(&mut self, after: u64) {
        self.background.set_after(after);
        self.settings.background_after = after;
        if let Err(error) = self.session.save_settings(&self.settings) {
            self.say(
                Level::Error,
                &format!("arko: could not save the session's settings: {error}"),
            );
        }
        let note = match after {
            0 => format!(
                "From now on, shell commands no longer move to the background on their own: each runs until it finishes or timeout_seconds passes, which now defaults to {}.",
                shell::default_timeout(0)
            ),
            after => format!(
                "From now on, a shell command still running after {} moves to the background, where it goes on as a job: the result gives its job ID and output so far, and a terminal update tells you when it finishes. timeout_seconds now defaults to {}.",
                shell::background_after_words(after),
                shell::default_timeout(after)
            ),
        };
        self.tell_agents(&note);
    }

    /// Whether `/exit` quits now (Ctrl+D and Ctrl+C twice are `/exit`
    /// too). The first one only warns of the workers running. Background
    /// jobs stop for good when Arko exits, so Arko quits only once every
    /// one running is among `agreed`, those the user agreed to stop: the
    /// full-screen view's popup's, or at the line prompt those its last
    /// warning named. Else it asks (the popup, or a warning) and stays. The
    /// check also readies the jobs for the exit, in one step, so none moves
    /// to the background after it. `None`: the user turned asking off.
    pub(crate) fn may_exit(&mut self, agreed: Option<Vec<uuid::Uuid>>) -> bool {
        let workers = self.crew.as_ref().map_or(0, |crew| crew.running().len());
        let warn_workers = workers > 0 && !self.exit_warned;
        let jobs = match agreed {
            None if warn_workers => Vec::new(),
            None => return true,
            // Arko stays, for now: the jobs only to say so.
            Some(_) if warn_workers => self.background.jobs(),
            Some(agreed) => match self.background.close(&agreed) {
                Ok(()) => return true,
                Err(jobs) => jobs,
            },
        };
        self.exit_warned = true;
        self.jobs_shown = jobs.iter().map(|job| job.job_id).collect();
        if let Some(popup) = self
            .quit_popup
            .as_ref()
            .filter(|_| !jobs.is_empty() || (self.ask_about_workers && workers > 0))
        {
            let _ = popup.send(ui::QuitAsk {
                jobs,
                workers: self
                    .crew
                    .as_ref()
                    .map(|crew| crew.running())
                    .unwrap_or_default(),
            });
            return false;
        }
        let jobs = jobs.len();
        let them = |count: usize| if count == 1 { "it" } else { "them" };
        let s = |count: usize| if count == 1 { "" } else { "s" };
        let text = match (workers, jobs) {
            (_, 0) => format!(
                "{workers} worker{} running: /exit again stops {} and quits (resuming the session lets the agent continue {}).",
                if workers == 1 { " is" } else { "s are" },
                them(workers),
                them(workers),
            ),
            (0, _) => format!(
                "{jobs} background job{} running: /exit again stops {} and quits.",
                if jobs == 1 { " is" } else { "s are" },
                them(jobs),
            ),
            _ => format!(
                "{workers} worker{} and {jobs} background job{} are running: /exit again stops them and quits (resuming the session lets the agent continue the worker{}).",
                s(workers),
                s(jobs),
                s(workers),
            ),
        };
        self.output.line(&self.output.warn(&text));
        false
    }

    /// Commands that work at any time.
    pub(crate) fn command(&self, command: &str) {
        let out = &self.output;
        match command {
            "/cost" => {
                self.publish_details();
                let report = self.details.cost();
                let plans = report
                    .plans
                    .iter()
                    .map(|plan| (plan.plan.clone(), plan.text.clone()))
                    .collect();
                for line in providers::cost_lines(&report.main, &report.workers, &plans) {
                    match line.detail {
                        true => out.line(&out.dim(&format!("  {}", line.text))),
                        false => out.line(&line.text),
                    }
                }
                if let Some((tokens, dollars)) = report.embeddings {
                    out.line(&format!(
                        "${dollars:.3} for embeddings (Voyage) while Arko ran · {}k tokens",
                        tokens.div_ceil(1000)
                    ));
                }
            }
            "/workers" => match &self.crew {
                Some(crew) if !crew.is_empty() => {
                    for line in crew.describe().lines() {
                        out.line(line);
                    }
                }
                Some(_) => out.line("No workers yet: the agent starts them as it needs them."),
                None if self.settings.max_workers == 0 => {
                    out.line("Workers are off in this session (--workers 0).");
                }
                None => out.line(
                    "Old workers are archived. Start a new Arko session for native workers. /new keeps this session's old tools.",
                ),
            },
            stop if stop.split_whitespace().next() == Some("/stop-workers")
                && stop.split_whitespace().nth(1).is_some() =>
            {
                let worker = stop.split_whitespace().nth(1).unwrap_or_default();
                let stopped = match &self.crew {
                    Some(crew) => crew.stop(worker, "the user stopped the worker"),
                    None => Err("This session has no workers.".to_string()),
                };
                match stopped {
                    Ok(()) => out.line(&out.warn(&format!(
                        "Stopping {worker}: a message from the agent resumes it."
                    ))),
                    Err(why) => out.line(&why),
                }
            }
            "/stop-workers" => {
                let stopped = self
                    .crew
                    .as_ref()
                    .map_or(0, |crew| crew.stop_all("the user stopped the workers"));
                if stopped == 0 {
                    out.line("No worker is running.");
                } else {
                    out.line(&out.warn(&format!(
                        "Stopping {stopped} worker{}: a message from the agent resumes {}.",
                        if stopped == 1 { "" } else { "s" },
                        if stopped == 1 { "it" } else { "them" }
                    )));
                }
            }
            "/mcp" => self.list_mcp(),
            "/skills" => self.list_skills(),
            "/session" => {
                self.publish_details();
                let facts = self.details.session();
                out.line(&format!("session {}", facts.id));
                out.line(&out.dim(&format!("{}", facts.dir.display())));
                out.line(&out.dim(&format!(
                    "main: {} · {}",
                    facts.main.shown,
                    match &facts.workers {
                        None => "no workers".to_string(),
                        Some(workers) => format!(
                            "workers: {} ({})",
                            workers.model.shown,
                            match workers.max_running {
                                None => "no limit".to_string(),
                                Some(n) => format!("up to {n} at once"),
                            }
                        ),
                    }
                )));
                out.line(&out.dim(&details::handover_text(
                    &facts.handover,
                    facts.workers.is_some(),
                )));
                out.line(&out.dim(&format!(
                    "text replies {}: {}",
                    if facts.text_replies { "on" } else { "off" },
                    agents::replies_text(facts.text_replies)
                )));
                out.line(&out.dim(&format!(
                    "extra rules {}: {}",
                    facts.extra_rules.name(),
                    facts.extra_rules.describe()
                )));
                out.line(&out.dim(&details::project_text(
                    facts.project_files,
                    facts.project_workers,
                )));
            }
            "/new" => out.line(&out.dim("Wait for the turn to end (or press Ctrl-C) first.")),
            other if conversations::resume_argument(other).is_some() => {
                out.line(&out.dim("Wait for the turn to end (or press Ctrl-C) first."))
            }
            other
                if commands::find(other)
                    .is_some_and(|command| command.runs == commands::Runs::Screen) =>
            {
                out.line(&format!(
                    "{} works in the full-screen view (arko without --repl).",
                    other.split_whitespace().next().unwrap_or(other)
                ));
            }
            _ => {
                for line in commands::help_lines() {
                    out.line(&line);
                }
                out.line(&out.dim(
                    "While the agent works, what you type adds to its task, and Ctrl-C stops the turn (the workers go on). When it asks you something, type the answer.",
                ));
            }
        }
    }

    /// `/mcp`: each server with its tools, or why it didn't start.
    pub(crate) fn list_mcp(&self) {
        let out = &self.output;
        let facts = self.details.mcp();
        if facts.servers.is_empty() {
            out.line(&format!(
                "No MCP servers. Arko starts those set up in {}, in the project's {} (once you approve them) and in --mcp-config files.",
                facts.user_file.display(),
                mcp::PROJECT_FILE
            ));
        }
        for server in &facts.servers {
            out.line(&format!(
                "{}  {}",
                out.bold(&server.name),
                out.dim(&format!(
                    "{} · {}",
                    ui::cut(&server.shown, 100),
                    server.file.display()
                ))
            ));
            match &server.tools {
                Ok(tools) if tools.is_empty() => out.line(&out.dim("  no tools")),
                Ok(tools) => out.line(&format!("  {}", tools.join(", "))),
                Err(error) => out.line(&out.error(&format!("  didn't start: {error}"))),
            }
        }
        for note in &facts.notes {
            out.line(&out.dim(note));
        }
        if let Some(note) = &facts.unoffered {
            out.line(&out.warn(note));
        }
    }

    pub(crate) fn list_skills(&self) {
        let out = &self.output;
        self.publish_details();
        let skills = match self.details.skills() {
            Ok(skills) => skills,
            Err(error) => {
                out.line(&out.error(&format!("The skills couldn't be read: {error}")));
                return;
            }
        };
        if skills.skills.is_empty() {
            out.line(&format!(
                "No skills. Arko reads them (folders with a SKILL.md) from the project's .agents/skills, .claude/skills and .skills, from {}, and from ~/.agents/skills and ~/.claude/skills.",
                skills.own_folder.display()
            ));
            return;
        }
        for skill in skills.skills.iter().filter(|skill| !skill.always) {
            out.line(&format!(
                "{}  {}",
                out.bold(&skill.name),
                ui::cut(&skill.description, 160)
            ));
        }
        for skill in skills.skills.iter().filter(|skill| skill.always) {
            out.line(&format!(
                "{}  {}",
                out.bold(&skill.name),
                out.dim("(always in the agents' instructions)")
            ));
        }
        out.line(&out.dim(&format!(
            "From {}",
            skills
                .folders
                .iter()
                .map(|folder| folder.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    /// One turn: the message goes to the main agent, and its reply is
    /// printed.
    pub(crate) async fn turn(&mut self, prompt: Prompt, input: &mut Inputs) -> TurnEnd {
        // A turn Arko stopped in the middle of (a crash) goes on where it
        // was saved, without a message: the message follows it.
        if !matches!(prompt, Prompt::Recover)
            && self.cut_back.is_none()
            && let Conversation::Stopped(run) = &self.conversation
            && matches!(run.restart, Restart::Recover)
            && let TurnEnd::Exit = Box::pin(self.turn(Prompt::Recover, input)).await
        {
            return TurnEnd::Exit;
        }
        self.exit_warned = false;
        self.jobs_shown.clear();
        self.news_hint_shown = false;
        // A session takes its first message's start as its name, as -p does.
        if let Prompt::User(text) = &prompt
            && self.settings.title.is_none()
            && !text.trim().is_empty()
        {
            self.settings.title = Some(ui::cut(text.trim(), 80));
            if let Err(error) = self.session.save_settings(&self.settings) {
                log::warn!("could not save the session's title: {error}");
            }
            self.publish_title();
        }
        // The user's message that starts it, which can be edited once the
        // turn says where it is in the conversation.
        let user_item = match &prompt {
            Prompt::User(text) => self
                .feed
                .last_turn_message()
                .filter(|item| item.text == text.trim())
                .map(|item| item.id),
            _ => None,
        };
        self.feed.turn_started();
        // The main agent's model is the conversation's from now on.
        self.publish_models();
        let workflow = self.workflow().clone();
        // A message after a stop goes to the stopped task, which resumes.
        let resumes = !matches!(prompt, Prompt::Recover)
            && matches!(&self.conversation, Conversation::Stopped(run) if run.restart != Restart::Recover);
        let news_turn = matches!(prompt, Prompt::News);
        let jobs_turn = matches!(prompt, Prompt::Jobs);
        let message = self.compose(prompt);
        let cut = self.cut_back.take();
        if news_turn {
            self.say(Level::Info, "  ⇠ the workers' news goes to the agent");
        }
        if jobs_turn {
            self.say(
                Level::Info,
                "  ⇠ a background job finished: its update goes to the agent",
            );
        }
        if resumes {
            self.say(
                Level::Info,
                "  ✎ added to the stopped task: the agent resumes it where it stopped",
            );
        }
        let spent_before = self.spent();
        let started = Instant::now();
        let conversation = std::mem::replace(&mut self.conversation, Conversation::New);
        // Where this turn's message goes in the conversation.
        let at = cut.unwrap_or(match &conversation {
            Conversation::New => 0,
            Conversation::Answered(result) => result.task.conversation.messages.len(),
            Conversation::Stopped(run) => run.result.task.conversation.messages.len(),
        });
        let mut options = self.continue_options(&conversation);
        if let Some(index) = cut {
            options = options.cut_back_to(index);
        }
        // The images the user's message sends.
        let images = std::mem::take(&mut self.pending_images);
        if !images.is_empty() {
            options = options.images(images.clone());
        }
        let call = {
            let workflow = workflow.clone();
            tokio::spawn(async move {
                match conversation {
                    Conversation::New => {
                        let options = RunOptions::new().images(images);
                        workflow.run_agent_with(MAIN, &message, options).await
                    }
                    Conversation::Answered(previous) => {
                        workflow
                            .continue_agent_with(&previous, &message, options)
                            .await
                    }
                    // A stopped run is cut back whatever its state.
                    Conversation::Stopped(run) if cut.is_some() => {
                        workflow
                            .continue_agent_with(&run.result, &message, options)
                            .await
                    }
                    Conversation::Stopped(run) => {
                        workflow.restart_with(&run, &message, options).await
                    }
                }
            })
        };
        tokio::pin!(call);
        let mut notes: Vec<(u64, String)> = Vec::new();
        let mut queued: Vec<String> = Vec::new();
        let mut stopping = false;
        let mut exit = false;
        // The agent's question the next line answers, with the ID the
        // remote page knows it by.
        let mut answering: Option<(u64, oneshot::Sender<String>)> = None;
        let outcome = loop {
            tokio::select! {
                outcome = &mut call => break outcome,
                event = input.next_event(false) => {
                    let (line, source) = match event {
                        Event::Request(request) => {
                            Box::pin(self.request(request)).await;
                            continue;
                        }
                        Event::Question(question) => {
                            if question.answer.is_closed() {
                                continue;
                            }
                            self.flush_printer().await;
                            self.output.echo(&self.output.bold("  ? The agent asks:"));
                            for line in question.text.trim().lines() {
                                self.output.echo(&format!("    {line}"));
                            }
                            self.output.echo(&self.output.dim("  (Type your answer and press Enter.)"));
                            let id = self.feed.question(&question.text);
                            if let Some((old, _)) = answering.replace((id, question.answer)) {
                                self.feed.question_closed(old);
                            }
                            continue;
                        }
                        Event::Woken | Event::Finished(_) => continue,
                        Event::Input(Some(Input::Background(agent))) => {
                            self.move_to_background(&agent);
                            continue;
                        }
                        Event::Input(Some(Input::Line(line))) => {
                            (input.coalesce(line).await, self.input_source)
                        }
                        Event::Input(Some(Input::Message(message))) => {
                            match self.message_text(message) {
                                Some(text) => (text, self.input_source),
                                None => continue,
                            }
                        }
                        Event::Remote(Action::Message(text)) => {
                            self.echo_web(&text);
                            (text, Source::Web)
                        }
                        Event::Remote(Action::Answer { question, text }) => {
                            self.answer(&mut answering, question, &text, Source::Web);
                            continue;
                        }
                        Event::Input(Some(Input::Answer { question, text })) => {
                            let source = self.input_source;
                            self.answer(&mut answering, question, &text, source);
                            continue;
                        }
                        Event::Input(Some(Input::Interrupt)) | Event::Remote(Action::Stop) => {
                            if stopping {
                                self.output.line(&self.output.dim("  (stopping: the agents get up to 10 s to end)"));
                            } else {
                                stopping = self.stop(&workflow);
                            }
                            continue;
                        }
                        Event::Input(Some(Input::Eof) | None) => {
                            exit = true;
                            stopping |= self.stop(&workflow);
                            continue;
                        }
                        Event::Input(Some(Input::Quit(agreed))) => {
                            if self.mirrored(|harness| harness.may_exit(agreed)) {
                                exit = true;
                                stopping |= self.stop(&workflow);
                            }
                            continue;
                        }
                    };
                    // The images an app's message sends go with its note.
                    let images = std::mem::take(&mut self.pending_images);
                    let shown = std::mem::take(&mut self.pending_refs);
                    let text = line.trim().to_string();
                    if text.is_empty() && images.is_empty() {
                        continue;
                    }
                    if let Some(command) = command(&text) {
                        if self.refused(command, source) {
                            continue;
                        }
                        if is_remote_command(command) {
                            self.remote_command(command).await;
                            continue;
                        }
                        if self.live_command(command, source) {
                            continue;
                        }
                        self.feed.user(command, source, Delivery::Command);
                        let quit = self.mirrored(|harness| {
                            if matches!(command, "/exit" | "/quit") {
                                harness.may_exit(Some(harness.jobs_shown.clone()))
                            } else {
                                harness.command(command);
                                false
                            }
                        });
                        if quit {
                            exit = true;
                            stopping |= self.stop(&workflow);
                        }
                        continue;
                    }
                    let text = match answering.take() {
                        Some((id, answer)) => match answer.send(text) {
                            Ok(()) => {
                                self.output.echo(&self.output.dim("  ✎ answered"));
                                self.feed.answered(id, line.trim(), source);
                                continue;
                            }
                            // The question was dropped (a stop).
                            Err(text) => {
                                self.feed.question_closed(id);
                                text
                            }
                        },
                        None => text,
                    };
                    if self.queue_lines {
                        self.feed.user(&text, source, Delivery::Queued);
                        queued.push(text);
                    } else {
                        let note = Note::new(text.clone()).to([MAIN]).once().images(images);
                        match workflow.post_note(note) {
                            Ok(seq) => {
                                self.output.echo(&self.output.dim(
                                    "  ✎ added to the task: the agent reads it at its next step",
                                ));
                                self.feed.user_with(&text, source, Delivery::Note, shown);
                                notes.push((seq.seq, text));
                            }
                            Err(error) => self.say(
                                Level::Error,
                                &format!("  could not pass that on: {error}"),
                            ),
                        }
                    }
                }
            }
        };
        self.flush_printer().await;
        // The jobs that finished meanwhile told the agent as it worked,
        // unless an update came during its last request (below).
        let finished = input.drain_finished();
        let outcome = outcome.unwrap_or_else(|error| {
            Err(WorkflowError::Channel(format!(
                "the turn's task failed: {error}"
            )))
        });
        if let Some((id, _)) = answering.take() {
            self.feed.question_closed(id);
        }
        let end = match outcome {
            Ok(result) => {
                self.output.echo("");
                let unread = unread_notes(&notes, &workflow);
                // With -p, a reply the workers' news or a turn for what the
                // user typed will follow isn't the answer.
                if self.one_shot && (self.news_coming() || !unread.is_empty()) {
                    self.output.line(result.summary.trim());
                } else {
                    self.output.answer(result.summary.trim());
                }
                self.feed.reply(&result.summary);
                self.keep_unread(unread, true);
                // Its task keeps that update for the next turn, which the
                // job starts as if it finished now.
                input.late = finished
                    .into_iter()
                    .find(|job| waits_with(&result.task, job));
                self.conversation = Conversation::Answered(result);
                self.track_conversations();
                TurnEnd::Answered
            }
            Err(WorkflowError::AgentStopped { .. } | WorkflowError::Cancelled) if stopping => {
                self.say(Level::Warn, "Stopped.");
                self.keep_unread(unread_notes(&notes, &workflow), false);
                self.track_conversations();
                self.conversation = conversation_of(&workflow, self.conversations.current_run());
                TurnEnd::Stopped
            }
            Err(error) => {
                self.say(Level::Error, &format!("The turn failed: {error}"));
                self.keep_unread(unread_notes(&notes, &workflow), false);
                self.recover_after_error(&workflow);
                TurnEnd::Failed
            }
        };
        self.footer(spent_before, started);
        if let Some(item) = user_item {
            self.mark_message(item, at);
        }
        if exit {
            return TurnEnd::Exit;
        }
        // Lines read without a terminal, each its own turn.
        for line in queued {
            if let TurnEnd::Exit = Box::pin(self.turn(Prompt::User(line), input)).await {
                return TurnEnd::Exit;
            }
        }
        // What the user typed that the agent replied before reading starts
        // a turn of its own, as the workers' news does. (After a stop or a
        // failure it waits for the next message.)
        if matches!(end, TurnEnd::Answered) && !self.unread.is_empty() {
            return Box::pin(self.turn(Prompt::Unread, input)).await;
        }
        end
    }

    /// Whether the workers have news for the main agent, or will have.
    pub(crate) fn news_coming(&self) -> bool {
        self.crew
            .as_ref()
            .is_some_and(|crew| !crew.running().is_empty() || crew.has_mail())
    }

    /// While the main agent's conversation waits for a message, the
    /// workers' news (their reports and messages) starts a turn of it.
    pub(crate) async fn deliver_news(&mut self, input: &mut Inputs) -> News {
        input.drain_woken();
        loop {
            let Some(crew) = self.crew.clone() else {
                return News::Done;
            };
            if !crew.has_mail() {
                return News::Done;
            }
            if !matches!(self.conversation, Conversation::Answered(_)) {
                if !self.news_hint_shown && !self.one_shot {
                    self.news_hint_shown = true;
                    self.say(
                        Level::Info,
                        "  (the workers have news for the agent: it goes with your next message)",
                    );
                }
                return News::Waiting;
            }
            match self.turn(Prompt::News, input).await {
                TurnEnd::Answered => input.drain_woken(),
                TurnEnd::Exit => return News::Exit,
                TurnEnd::Stopped => return News::Stopped,
                TurnEnd::Failed => return News::Failed,
            }
        }
    }

    /// While the main agent's conversation waits for a message, a background
    /// job of its that finished starts a turn of it: the job's terminal
    /// update reaches the agent as the turn starts. (One that finishes while
    /// the agent works reaches it as a note.)
    pub(crate) async fn deliver_finished(
        &mut self,
        finished: arko_agents::terminal::JobFinished,
        input: &mut Inputs,
    ) -> News {
        let run = match &self.conversation {
            Conversation::New => return News::Done,
            Conversation::Answered(result) => result.agent_run_id,
            Conversation::Stopped(run) => run.result.agent_run_id,
        };
        if finished.owner != run {
            return News::Done;
        }
        if !matches!(self.conversation, Conversation::Answered(_)) {
            self.say(
                Level::Info,
                "  (a background job finished: its update goes with your next message)",
            );
            return News::Waiting;
        }
        // Others that finished meanwhile go with it.
        input.drain_finished();
        match self.turn(Prompt::Jobs, input).await {
            TurnEnd::Answered => News::Done,
            TurnEnd::Exit => News::Exit,
            TurnEnd::Stopped => News::Stopped,
            TurnEnd::Failed => News::Failed,
        }
    }

    /// Ctrl+B: moves the shell command `agent` runs (the main agent, or a
    /// worker by its ID) to the background at once.
    pub(crate) fn move_to_background(&self, agent: &str) {
        let workers = self.workflow().subagents();
        // The runs that aren't workers' are the main agent's.
        let runs: Vec<uuid::Uuid> = self
            .background
            .running()
            .into_iter()
            .filter(
                |run| match workers.iter().find(|worker| worker.run_id == *run) {
                    Some(worker) => worker.handle == agent,
                    None => agent == remote::MAIN_AGENT,
                },
            )
            .collect();
        let moved: usize = runs
            .into_iter()
            .map(|run| self.background.move_run(run))
            .sum();
        if moved == 0 {
            self.say(
                Level::Info,
                "  No command is running to move to the background.",
            );
        }
    }

    /// Without a terminal (with -p, or input from a pipe), Arko waits for
    /// the workers before it ends: their news goes to the agent as it
    /// comes. Returns how the last turn ended.
    pub(crate) async fn finish_workers(&mut self, input: &mut Inputs) -> TurnEnd {
        let mut said = false;
        loop {
            let news = self.deliver_news(input).await;
            let Some(crew) = self.crew.clone() else {
                return TurnEnd::Answered;
            };
            // (A worker's report is in the mailbox as it stops running.)
            let running = crew.running();
            match news {
                News::Exit => return TurnEnd::Exit,
                News::Stopped => {
                    crew.stop_all("the user stopped Arko");
                    return TurnEnd::Stopped;
                }
                // News that came as the last turn ended.
                News::Done if crew.has_mail() => continue,
                News::Done if running.is_empty() => return TurnEnd::Answered,
                // The conversation can't take the news (a turn failed).
                News::Waiting | News::Failed if running.is_empty() => return TurnEnd::Failed,
                _ => {}
            }
            if !said {
                said = true;
                self.output.line(&self.output.dim(&format!(
                    "Waiting for {} (Ctrl-C stops {})…",
                    and_list(&running),
                    if running.len() == 1 { "it" } else { "them" }
                )));
            }
            match input.next_event(true).await {
                Event::Woken => {}
                Event::Finished(finished) => {
                    if let News::Exit = self.deliver_finished(finished, input).await {
                        return TurnEnd::Exit;
                    }
                }
                Event::Question(question) => drop(question),
                Event::Request(request) => {
                    self.only_workers = true;
                    Box::pin(self.request(request)).await;
                    self.only_workers = false;
                }
                Event::Input(Some(Input::Line(_))) => {}
                Event::Input(Some(Input::Background(agent))) => self.move_to_background(&agent),
                Event::Remote(Action::Message(_) | Action::Answer { .. })
                | Event::Input(Some(Input::Message(_) | Input::Answer { .. })) => {
                    self.feed.notice(
                        Level::Warn,
                        "Arko is only waiting for the workers now, so messages aren't taken.",
                    );
                }
                Event::Input(Some(Input::Interrupt | Input::Eof | Input::Quit(_)) | None)
                | Event::Remote(Action::Stop) => {
                    let stopped = crew.stop_all("the user stopped Arko");
                    self.output.line(&self.output.warn(&format!(
                        "Stopping {stopped} worker{}.",
                        if stopped == 1 { "" } else { "s" }
                    )));
                    return TurnEnd::Stopped;
                }
            }
        }
    }

    /// The user's message with project instructions and unread operator notes.
    /// arko injects worker news from its mailbox when the run continues.
    /// Recovery keeps the existing prompt.
    pub(crate) fn compose(&mut self, prompt: Prompt) -> String {
        let jobs = matches!(prompt, Prompt::Jobs);
        let message = match prompt {
            Prompt::Recover => return String::new(),
            Prompt::User(message) => Some(message),
            Prompt::News | Prompt::Jobs | Prompt::Unread => None,
        };
        let mut parts = Vec::new();
        // (A conversation cut back to its first message starts over.)
        let starts = matches!(self.conversation, Conversation::New) || self.cut_back == Some(0);
        if starts && let Some(project) = self.project.for_main(self.settings.project_instructions) {
            parts.push(project);
        }
        if !self.unread.is_empty() {
            let unread = std::mem::take(&mut self.unread);
            // They were posted as one-time notes too: the run this message
            // continues would get them a second time.
            let continued = match &self.conversation {
                Conversation::New => None,
                Conversation::Answered(result) => Some(result.agent_run_id),
                Conversation::Stopped(run) => Some(run.result.agent_run_id),
            };
            if let Some(run) = continued {
                self.workflow().skip_one_time_notes(run);
            }
            parts.push(format!(
                "# What I typed during your last turn, which you didn't get to read\n\n{}",
                unread
                    .iter()
                    .map(|text| format!("- {}", text.replace('\n', "\n  ")))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if std::mem::take(&mut self.switched) {
            let running = self
                .crew
                .as_ref()
                .map(|crew| crew.running())
                .unwrap_or_default();
            let named = match running.as_slice() {
                [] => None,
                [one] => Some(format!(
                    "Worker {one} is still running from earlier in this session. Its reports and messages"
                )),
                [rest @ .., last] => Some(format!(
                    "Workers {} and {last} are still running from earlier in this session. Their reports and messages",
                    rest.join(", ")
                )),
            };
            if let Some(named) = named {
                parts.push(format!(
                    "# Workers already running\n\n{named} come to you in this conversation."
                ));
            }
        }
        if let Some(notice) = &self.unoffered {
            parts.push(format!("# Session capabilities\n\n{notice}"));
        }
        if jobs {
            parts.push("A background job of yours finished after your last turn ended: its terminal update follows. (Arko started this turn for it. The user hasn't written.)".to_string());
        }
        match message {
            Some(message) if parts.is_empty() => message,
            Some(message) => {
                parts.push(format!("# My message\n\n{message}"));
                parts.join("\n\n")
            }
            None if parts.is_empty() => {
                "Your workers sent you news as your last turn ended: it follows. (Arko started this turn for it; the user hasn't written.)".to_string()
            }
            None => parts.join("\n\n"),
        }
    }

    /// Keeps the notes the run never read for the agent: a new turn takes
    /// them to it now (`now`, after a reply), else they go with the next
    /// message.
    pub(crate) fn keep_unread(&mut self, unread: Vec<String>, now: bool) {
        if unread.is_empty() {
            return;
        }
        self.say(
            Level::Info,
            if now {
                "  (the agent finished before it read what you typed, so a new turn takes it to the agent)"
            } else {
                "  (the agent finished before it read what you typed, so it goes with your next message)"
            },
        );
        self.unread.extend(unread);
    }

    pub(crate) fn stop(&self, workflow: &Workflow) -> bool {
        let running = self.crew.as_ref().map_or(0, |crew| crew.running().len());
        self.say(
            Level::Warn,
            &if running == 0 {
                "  Stopping the turn…".to_string()
            } else {
                format!(
                    "  Stopping the turn… (the workers go on: /stop-workers stops {})",
                    if running == 1 { "it" } else { "them" }
                )
            },
        );
        workflow
            .stop_agent(
                MAIN,
                StopRequest::operator("the user stopped the turn"),
                RunStopIntent::Relaunch,
            )
            .is_ok_and(|runs| !runs.is_empty())
    }

    /// Keep the same workflow after a failed turn so its workers keep running.
    pub(crate) fn recover_after_error(&mut self, workflow: &Workflow) {
        self.track_conversations();
        self.conversation = conversation_of(workflow, self.conversations.current_run());
        if matches!(self.conversation, Conversation::New) {
            self.say(
                Level::Info,
                "This conversation can't go on, so your next message starts it over, without what was said before.",
            );
        }
    }

    pub(crate) async fn flush_printer(&self) {
        if let Some((control, _)) = &self.printer {
            let (done, flushed) = oneshot::channel();
            if control.send(Control::Flush(done)).is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(5), flushed).await;
            }
        }
    }

    /// What the session has cost: the main agent's runs and the workers'.
    pub(crate) fn spent(&self) -> f64 {
        self.workflow().context().budget.total_spent()
    }

    pub(crate) fn footer(&self, spent_before: f64, started: Instant) {
        let spent = self.spent();
        let stats = self.stats.lock().unwrap().clone();
        let mut parts = vec![format!("{:.0} s", started.elapsed().as_secs_f64())];
        if spent > 0.0 {
            parts.push(format!(
                "${:.3} (session ${:.2})",
                (spent - spent_before).max(0.0),
                spent
            ));
        }
        if let Some(tokens) = stats.context_tokens {
            parts.push(format!("context {}k", tokens / 1000));
        }
        let running = self.crew.as_ref().map_or(0, |crew| crew.running().len());
        if running > 0 {
            parts.push(format!(
                "{running} worker{} running",
                if running == 1 { "" } else { "s" }
            ));
        }
        let parts = parts.join(" · ");
        let line = match &stats.plan_usage {
            Some(plan) => format!("{parts} · {plan}"),
            None => parts.clone(),
        };
        self.output.echo(&self.output.dim(&format!("── {line} ──")));
        self.feed.turn_ended(&parts, stats.plan_usage.as_deref());
    }
}

/// Where the conversation whose latest run is `run` stands: stopped (or
/// interrupted), answered, or neither (no run yet, or one that can't go
/// on: the next message starts one).
pub(crate) fn conversation_of(workflow: &Workflow, run: Option<uuid::Uuid>) -> Conversation {
    let Some(latest) = run.and_then(|run| workflow.agent_result(run)) else {
        return Conversation::New;
    };
    if let Some(run) = workflow
        .stopped_runs()
        .into_iter()
        .find(|run| run.result.agent_run_id == latest.agent_run_id)
    {
        return Conversation::Stopped(run);
    }
    if latest.status == AgentRunStatus::Completed {
        Conversation::Answered(latest)
    } else {
        Conversation::New
    }
}

/// The command `text` is, if it is one (see `commands`): a command's name,
/// alone or with what it takes.
pub(crate) fn command(text: &str) -> Option<&str> {
    (commands::find(text).is_some() || split_live_command(text).is_some()).then_some(text)
}

/// `/remote`, alone or with what follows it.
pub(crate) fn is_remote_command(text: &str) -> bool {
    text == "/remote" || text.starts_with("/remote ")
}

/// The link `/remote qr` shows: the first other devices can open, or the
/// one on `interface` (a name or an address). `Err` with what to tell the
/// user when there is none, of a server on `addr`.
pub(crate) fn qr_link<'a>(
    links: &'a [remote::Link],
    interface: &str,
    addr: std::net::SocketAddr,
) -> Result<&'a remote::Link, String> {
    let mut others = links.iter().filter(|link| !link.loopback);
    let link = if interface.is_empty() {
        others.next()
    } else {
        others.find(|link| link.is(interface))
    };
    link.ok_or_else(|| {
        if links.iter().all(|link| link.loopback) {
            format!(
                "Only this machine can open the page (it listens on {addr}), so there is no QR code: /remote 0.0.0.0 serves it to the network's devices."
            )
        } else {
            format!(
                "The page has no link on `{interface}`: its links are on {}.",
                remote::link_names(links)
            )
        }
    })
}

/// Where a link leads, as the terminal says it: ` (ens33, LAN)`.
pub(crate) fn place(link: &remote::Link) -> String {
    link.interface
        .as_ref()
        .map(|name| format!(" ({name}, {})", link.kind.label()))
        .unwrap_or_default()
}

/// The commands that change a setting while the session runs, which take
/// an argument: `/outside`, `/model`, `/handover`, `/handover-at`, `/keys`.
/// And `/message`, which works while the agent works too.
pub(crate) const LIVE_COMMANDS: [&str; 11] = [
    "/outside",
    "/model",
    "/handover",
    "/handover-at",
    "/keys",
    "/text-replies",
    "/cc-compact",
    "/extra-rules",
    "/project-instructions",
    "/background-after",
    "/message",
];

/// What the user is told of when shell commands move to the background,
/// after `after` seconds (0: never).
pub(crate) fn background_after_text(after: u64) -> String {
    match after {
        0 => format!(
            "Shell commands stay in the foreground until they finish, and timeout_seconds defaults to {} s.",
            shell::default_timeout(0)
        ),
        after => format!(
            "A shell command still running after {} moves to the background, where it goes on as a job, and timeout_seconds defaults to {} s.",
            shell::background_after_words(after),
            shell::default_timeout(after)
        ),
    }
}

/// What the user is told of a handover asked for, or of a compaction when
/// the main agent `compacts` in Claude Code.
pub(crate) fn handover_text(asked: handover::Asked, compacts: bool) -> &'static str {
    match (asked, compacts) {
        (handover::Asked::Now, false) => {
            "The main agent hands over at its next step: it writes a report of its work, then goes on from the report in a fresh conversation."
        }
        (handover::Asked::Next, false) => {
            "The main agent hands over when your next message starts its turn: it writes a report of its work first, then goes on from the report in a fresh conversation."
        }
        (handover::Asked::Already, false) => "A handover is under way already.",
        (handover::Asked::Now, true) => {
            "Claude Code compacts the main agent's conversation at its next step, and the agent goes on from Claude Code's summary."
        }
        (handover::Asked::Next, true) => {
            "Claude Code compacts the main agent's conversation when your next message starts its turn, and the agent goes on from Claude Code's summary with your message."
        }
        (handover::Asked::Already, true) => "A compaction is under way already.",
    }
}

/// A live command, alone or with its argument: the command and the
/// argument.
pub(crate) fn split_live_command(text: &str) -> Option<(&str, &str)> {
    let (name, argument) = text.split_once(' ').unwrap_or((text, ""));
    LIVE_COMMANDS
        .contains(&name)
        .then_some((name, argument.trim()))
}

/// Prints through `to`, and keeps a copy of the lines: a command's output,
/// for the remote page.
pub(crate) struct Tee {
    pub(crate) to: Arc<Output>,
    /// Each line, with its level.
    pub(crate) lines: Arc<Mutex<Vec<(Option<Level>, String)>>>,
}

impl ui::Printer for Tee {
    fn print(&mut self, text: String) -> Result<(), ()> {
        let text = text.strip_suffix('\n').unwrap_or(&text);
        // (The feed gets the whole output when the command ends.)
        if self.to.in_color() {
            self.to.echo(text);
        } else {
            self.to.echo(&remote::plain(text));
        }
        lock(&self.lines).push((Output::level_of(text), text.to_string()));
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agents::{AgentSettings, Effort, ExtraRules, ModelSettings, resolve_model};
    use crate::cli::*;
    use crate::credentials::Need;
    use crate::inputs::*;
    use crate::options::{Options, SessionChoice};
    use crate::prompter::{McpApproval, NoPrompter, Prompter};
    use crate::providers::Failure;
    use crate::shell::WindowsShell;
    use crate::start::*;
    use clap::Parser;

    /// The live settings' end-to-end tests, in their own file. Most run
    /// only on Unix, so their helpers go unused elsewhere.
    #[cfg_attr(not(unix), allow(dead_code))]
    mod live {
        use super::*;
        include!("live_tests.rs");
    }

    /// The end-to-end tests of commands moving to the background, in their
    /// own file. Most run only on Unix.
    #[cfg_attr(not(unix), allow(dead_code))]
    mod background {
        use super::*;
        include!("background_tests.rs");
    }

    /// `/model`'s end-to-end tests, in their own file.
    mod model {
        use super::*;
        include!("model_tests.rs");
    }

    /// `/keys`'s end-to-end tests, in their own file.
    mod api_keys {
        use super::*;
        include!("keys_tests.rs");
    }

    /// `/resume`'s end-to-end tests, in their own file.
    mod resume {
        use super::*;
        include!("resume_tests.rs");
    }

    /// Text replies' end-to-end tests, in their own file.
    mod text_replies {
        use super::*;
        include!("text_reply_tests.rs");
    }

    /// The extra rules' end-to-end tests, in their own file.
    mod extra_rules {
        use super::*;
        include!("extra_rules_tests.rs");
    }

    /// What agents in Claude Code do at the handover size, end to end, in
    /// its own file.
    mod cc_compact {
        use super::*;
        include!("cc_compact_tests.rs");
    }

    /// The project instructions' end-to-end tests, in their own file.
    mod project_instructions {
        use super::*;
        include!("project_tests.rs");
    }

    /// File editing's end-to-end tests, in their own file.
    mod edit_file {
        use super::*;
        include!("edit_file_tests.rs");
    }

    /// What the model is sent, measured (an ignored test), in its own file.
    mod request_sizes {
        use super::*;
        include!("request_size_tests.rs");
    }

    /// `--handover-at` takes what `/handover-at` takes (950k, 950K, 950000,
    /// 950,000, 950_000) and refuses what it refuses, and a new session's
    /// agents, the main one and the workers, start with it.
    #[test]
    fn the_handover_flag_takes_what_the_command_takes() {
        let parse = |size: &str| Args::try_parse_from(["arko", "--handover-at", size]);
        for size in ["950k", "950K", "950000", "950,000", "950_000"] {
            assert_eq!(parse(size).unwrap().handover_at, Some(950_000), "{size}");
        }
        assert!(parse("5k").unwrap_err().to_string().contains("too small"));
        for size in ["950 k", "0.95m", "lots"] {
            let error = parse(size).unwrap_err().to_string();
            assert!(
                error.contains("isn't a number of tokens"),
                "{size}: {error}"
            );
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap();
        let args =
            Args::try_parse_from(["arko", "-m", "cc:opus", "-C", dir, "--handover-at", "300k"])
                .unwrap();
        let (settings, _) = new_settings(&args.options().unwrap(), &NoPrompter).unwrap();
        assert_eq!(settings.main.handover_at, Some(300_000));
        assert_eq!(settings.worker.handover_at, Some(300_000));
        // Each agent's size is held at 95% of what its model accepts.
        let args = Args::try_parse_from([
            "arko",
            "-m",
            "cc:opus",
            "--worker-model",
            "luna",
            "--openai-auth",
            "api-key",
            "-C",
            dir,
            "--handover-at",
            "2000k",
        ])
        .unwrap();
        let (settings, _) = new_settings(&args.options().unwrap(), &NoPrompter).unwrap();
        assert_eq!(settings.main.handover_at, Some(950_000));
        let luna = agents::max_handover(&resolve_model("luna").unwrap(), settings.openai_auth);
        assert_eq!(settings.worker.handover_at, luna);
        assert!(luna < Some(950_000));
    }

    /// The flags become the session's options: which session, each pair of
    /// flags as yes, no or neither, -p's message as a new session's title,
    /// and whether someone takes part at the terminal.
    #[test]
    fn the_flags_become_the_sessions_options() {
        let options = |words: &[&str], terminal: bool| {
            Args::try_parse_from(std::iter::once("arko").chain(words.iter().copied()))
                .unwrap()
                .options_at(terminal)
        };
        let new = options(&[], true).unwrap();
        assert_eq!(new.session, SessionChoice::New);
        assert!(new.interactive && !new.one_shot);
        assert_eq!(
            (new.cache_1h, new.cc_compact, new.text_replies),
            (None, None, None)
        );
        assert_eq!(
            (new.extra_rules, new.worker_project_instructions),
            (None, None)
        );
        assert_eq!(
            options(&["-c"], true).unwrap().session,
            SessionChoice::ContinueLatest
        );
        assert_eq!(
            options(&["--resume", "abc"], true).unwrap().session,
            SessionChoice::Resume("abc".into())
        );
        let flags = options(
            &[
                "--no-cache-1h",
                "--cc-compact",
                "--no-extra-rules",
                "--no-worker-project-instructions",
                "--text-replies",
            ],
            false,
        )
        .unwrap();
        assert_eq!(flags.cache_1h, Some(false));
        assert_eq!(flags.cc_compact, Some(true));
        assert_eq!(flags.extra_rules, Some(ExtraRules::Off));
        assert_eq!(flags.worker_project_instructions, Some(false));
        assert_eq!(flags.text_replies, Some(true));
        assert!(!flags.interactive, "no terminal and no web page");
        assert!(options(&["--hide-thinking"], true).unwrap().hide_thinking);
        let one_shot = options(&["-p", "Count the files."], true).unwrap();
        assert!(one_shot.one_shot && !one_shot.interactive);
        assert_eq!(one_shot.title.as_deref(), Some("Count the files."));
        // The web page: text replies, but no one at the terminal to ask.
        let remote = options(&["--remote", "0.0.0.0:9000"], false).unwrap();
        assert!(!remote.interactive && text_replies_for(&remote));
        assert_eq!(remote.remote.and_then(|bind| bind.port), Some(9000));
        assert!(options(&["--remote", "nowhere:x"], false).is_err());
    }

    /// The names the flags take for an effort, a shell and the OpenAI login
    /// are the ones Arko reads elsewhere (`/model`, ARKO_SHELL,
    /// ARKO_OPENAI_AUTH), which builds without the command line read too.
    #[test]
    fn the_settings_read_the_names_the_flags_take() {
        use clap::ValueEnum;
        fn same<T: ValueEnum + std::str::FromStr + PartialEq + std::fmt::Debug>() {
            for value in T::value_variants() {
                let name = value.to_possible_value().unwrap().get_name().to_string();
                assert_eq!(name.parse::<T>().ok().as_ref(), Some(value), "{name}");
                let shouted = name.to_uppercase();
                assert_eq!(shouted.parse::<T>().ok().as_ref(), Some(value), "{shouted}");
            }
            assert!("nonsense".parse::<T>().is_err());
        }
        same::<Effort>();
        same::<WindowsShell>();
        same::<providers::OpenAiAuth>();
    }

    /// A missing credential is asked for through the prompter, and the
    /// client is made again after each answer; the prompter's refusal, or
    /// no one to ask, keeps the session from opening and says why.
    #[tokio::test]
    async fn a_missing_key_asks_the_prompter_and_retries() {
        struct Scripted {
            asked: Mutex<Vec<Need>>,
            answers: Mutex<Vec<Result<(), String>>>,
        }
        #[async_trait::async_trait]
        impl Prompter for Scripted {
            async fn need_credentials(&self, need: &Need) -> Result<(), String> {
                lock(&self.asked).push(need.clone());
                lock(&self.answers).remove(0)
            }
            async fn approve_project_mcp(&self, _: &McpApproval) -> Result<Option<bool>, String> {
                Ok(None)
            }
            fn note(&self, _: Level, _: &str) {}
        }
        let scripted = |answers: Vec<Result<(), String>>| Scripted {
            asked: Mutex::default(),
            answers: Mutex::new(answers),
        };
        let need = Need::AnthropicKey {
            model: "claude-opus-4-1".into(),
        };
        // Answered twice, and the key is there on the third try.
        let prompter = scripted(vec![Ok(()), Ok(())]);
        let mut tries = 0;
        let client = client_asking(&prompter, || {
            tries += 1;
            if tries < 3 {
                Err(Failure::Need(need.clone()))
            } else {
                Ok(LLMClient::new())
            }
        })
        .await;
        assert!(client.is_ok());
        assert_eq!(tries, 3);
        assert_eq!(*lock(&prompter.asked), [need.clone(), need.clone()]);
        // Cancelled.
        let prompter = scripted(vec![Err("cancelled".into())]);
        let error = client_asking(&prompter, || Err(Failure::Need(need.clone())))
            .await
            .err()
            .unwrap();
        assert_eq!(error, "cancelled");
        // No one to ask: what the session needs.
        let error = client_asking(&NoPrompter, || Err(Failure::Need(need.clone())))
            .await
            .err()
            .unwrap();
        assert_eq!(error, need.message());
        // Another failure isn't the prompter's.
        let prompter = scripted(Vec::new());
        let error = client_asking(&prompter, || Err(Failure::Other("broken".into())))
            .await
            .err()
            .unwrap();
        assert_eq!(error, "broken");
        assert!(lock(&prompter.asked).is_empty());
    }

    /// `--handover-at` when resuming sets both sizes, as `/handover-at`
    /// does, and the session keeps them.
    #[tokio::test]
    async fn the_handover_flag_sets_both_sizes_of_a_resumed_session() {
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
        harness.handover_at_start(150_000);
        assert_eq!(harness.handover.at(), 150_000);
        assert_eq!(harness.handover.workers_at(), Some(150_000));
        let saved = harness.session.load_settings().unwrap();
        assert_eq!(saved.main_handover_at, Some(150_000));
        assert_eq!(saved.workers_handover_at, Some(150_000));
    }

    #[test]
    fn commands_move_to_the_background_unless_with_p() {
        let args = |line: &[&str]| {
            Args::try_parse_from(std::iter::once("arko").chain(line.iter().copied()))
                .unwrap()
                .options()
                .unwrap()
        };
        assert_eq!(background_after_for(&args(&[]), 600), 600);
        assert_eq!(background_after_for(&args(&["-c"]), 90), 90);
        assert_eq!(background_after_for(&args(&["-p", "hi"]), 600), 0);
        assert_eq!(
            background_after_for(&args(&["-p", "hi", "--background-after", "90s"]), 600),
            90
        );
        assert_eq!(
            background_after_for(&args(&["--background-after", "off"]), 600),
            0
        );
        assert!(Args::try_parse_from(["arko", "--background-after", "soon"]).is_err());
    }

    #[test]
    fn only_the_known_commands_are_commands() {
        assert_eq!(command("/cost"), Some("/cost"));
        assert_eq!(command("/outside on"), Some("/outside on"));
        assert_eq!(
            split_live_command("/outside  off "),
            Some(("/outside", "off"))
        );
        assert_eq!(command("/outsider"), None);
        assert_eq!(command("/stop-workers"), Some("/stop-workers"));
        assert_eq!(command("/workers"), Some("/workers"));
        assert_eq!(command("/usr/bin is missing"), None);
        assert_eq!(command("/cost please"), None);
    }

    /// A session saved before workers ran in parallel, and before the file
    /// tools and `ask_user`, keeps `run_workflow_agent`, and is told what
    /// its agents lack.
    #[test]
    fn a_session_from_before_parallel_workers_is_told_what_it_lacks() {
        let opus = ModelSettings {
            model: resolve_model("opus").unwrap().id,
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let settings = AgentSettings {
            max_workers: NO_LIMIT,
            ..AgentSettings::for_test(&opus, &opus)
        };
        let main = agents::main_config(&settings).unwrap();
        let worker = agents::worker_config(&settings).unwrap();
        let mut old = main.clone();
        old.tool_config.available_tool_names.retain(|name| {
            !crew::LEAD_TOOL_NAMES.contains(&name.as_str())
                && !agents::FILE_TOOL_NAMES.contains(&name.as_str())
                && name != ASK_USER_TOOL_NAME
                && name != message::MESSAGE_USER
        });
        old.tool_config
            .available_tool_names
            .push(RUN_WORKFLOW_AGENT_TOOL_NAME.into());
        let saved = SavedSetup {
            name: WORKFLOW_NAME.into(),
            configs: [(MAIN.to_string(), old)].into(),
        };
        assert_eq!(
            unoffered(&saved, &[(MAIN, &main), (WORKER, &worker)]).unwrap(),
            "This session started without workers that run in parallel, the file tools, questions to you (ask_user), and answers to you while it works (message_user), so its agents don't get them. A new session does."
        );
    }

    /// A session fin (Arko's name before) saved resumes in Arko: its
    /// checkpoint names the workflow `fin`, and its agents' saved
    /// configurations say "You are fin", which Arko keeps for it. The main
    /// agent goes on, a worker it started continues, and a new worker
    /// starts, all as the session was saved.
    #[tokio::test]
    async fn a_session_saved_by_fin_resumes() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body.clone());
                let wait = |step| {
                    tool_call(
                        step,
                        crew::WAIT_FOR_AGENTS,
                        serde_json::json!({"timeout_seconds": 10}),
                    )
                };
                let start = |step, task| {
                    tool_call(
                        step,
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": task}),
                    )
                };
                Box::pin(async move {
                    if body.contains("You are agent run r1") {
                        if body.contains("Count the pears too.") {
                            complete("w2", "4 pears.")
                        } else {
                            complete("w1", "3 apples.")
                        }
                    } else if body.contains("You are agent run r2") {
                        complete("w3", "2 plums.")
                    } else if body.contains("2 plums.") {
                        complete("m9", "Plums: 2.")
                    } else if body.contains("call_m7") {
                        wait("m8")
                    } else if body.contains("Count the plums.") {
                        start("m7", "Count the plums.")
                    } else if body.contains("4 pears.") {
                        complete("m6", "Pears: 4.")
                    } else if body.contains("call_m4") {
                        wait("m5")
                    } else if body.contains("Ask it about pears.") {
                        tool_call(
                            "m4",
                            crew::MESSAGE_AGENT,
                            serde_json::json!({"to": "r1", "message": "Count the pears too."}),
                        )
                    } else if body.contains("3 apples.") {
                        complete("m3", "Apples: 3.")
                    } else if body.contains("call_m1") {
                        wait("m2")
                    } else {
                        start("m1", "Count the apples.")
                    }
                })
            }
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        let arko = (harness.main_config.clone(), harness.worker_config.clone());
        // The agents as fin built them.
        for config in [&mut harness.main_config, &mut harness.worker_config] {
            config.name = config.name.replace("Arko", "fin");
            config.role_description = config.role_description.replace("Arko", "fin");
            config.instructions = config.instructions.replace("Arko", "fin");
        }
        assert!(
            harness
                .main_config
                .role_description
                .starts_with("You are fin,")
        );
        harness.open(false).unwrap();
        let end = harness
            .turn(Prompt::User("How many apples?".into()), &mut inputs)
            .await;
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "Apples: 3.");
        harness.crew.as_ref().unwrap().shutdown().await;
        // fin named the workflow `fin`.
        let run = harness.session.run_dir();
        for entry in std::fs::read_dir(&run).unwrap() {
            let path = entry.unwrap().path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let text = std::fs::read_to_string(&path).unwrap();
                let text = text
                    .replace(r#""workflow_name": "arko""#, r#""workflow_name": "fin""#)
                    .replace(r#""workflow_name":"arko""#, r#""workflow_name":"fin""#);
                std::fs::write(&path, text).unwrap();
            }
        }
        let checkpoint = harness.session.latest_checkpoint().unwrap();
        assert_eq!(saved_setup(&checkpoint, &run).unwrap().name, "fin");

        // Arko builds its agents as Arko, and resumes the session.
        (harness.main_config, harness.worker_config) = arko;
        harness.open(true).unwrap();
        harness.start_new_workers();
        assert!(harness.unoffered.is_none(), "{:?}", harness.unoffered);
        let end = harness
            .turn(Prompt::User("Ask it about pears.".into()), &mut inputs)
            .await;
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "Pears: 4.");
        let end = harness
            .turn(Prompt::User("Count the plums.".into()), &mut inputs)
            .await;
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "Plums: 2.");
        let bodies = bodies.lock().unwrap().clone();
        let resumed = bodies
            .iter()
            .rfind(|body| body.contains("2 plums.") && !body.contains("You are agent run"))
            .unwrap();
        assert!(
            resumed.contains("You are fin,"),
            "the main agent's saved instructions"
        );
        assert!(!resumed.contains("You are Arko"));
        let follow_up = bodies
            .iter()
            .find(|body| {
                body.contains("Count the pears too.") && body.contains("You are agent run r1")
            })
            .unwrap();
        assert!(
            follow_up.contains("3 apples."),
            "r1 continued its conversation"
        );
        assert!(
            bodies
                .iter()
                .any(|body| body.contains("You are agent run r2"))
        );
        harness.crew.as_ref().unwrap().shutdown().await;

        // It is still fin's workflow, so it resumes again.
        let checkpoint = harness.session.latest_checkpoint().unwrap();
        let saved = saved_setup(&checkpoint, &harness.session.run_dir()).unwrap();
        assert_eq!(saved.name, "fin");
        harness.open(true).unwrap();
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    fn request(questions: &str) -> UserInputRequest {
        UserInputRequest {
            workflow_run_id: uuid::Uuid::new_v4(),
            agent_run_id: uuid::Uuid::new_v4(),
            agent_name: MAIN.into(),
            questions: questions.into(),
        }
    }

    /// The agent's questions reach the prompt, and the answer comes back;
    /// without a terminal, the agent hears that no one can answer. The
    /// asker never fails the turn.
    #[tokio::test]
    async fn the_agents_questions_are_answered_at_the_prompt() {
        let alone = Asker { questions: None };
        assert_eq!(alone.respond(request("Which?")).await.unwrap(), CANT_ASK);

        let (sender, mut questions) = mpsc::unbounded_channel();
        let asker = Arc::new(Asker {
            questions: Some(sender),
        });
        let asking = tokio::spawn({
            let asker = asker.clone();
            async move { asker.respond(request("Tabs or spaces?")).await }
        });
        let question = questions.recv().await.unwrap();
        assert_eq!(question.text, "Tabs or spaces?");
        question.answer.send("spaces".into()).unwrap();
        assert_eq!(asking.await.unwrap().unwrap(), "spaces");

        let asking = tokio::spawn({
            let asker = asker.clone();
            async move { asker.respond(request("Still there?")).await }
        });
        drop(questions.recv().await.unwrap());
        assert_eq!(asking.await.unwrap().unwrap(), "(The user didn't answer.)");
        drop(questions);
        assert_eq!(asker.respond(request("Hello?")).await.unwrap(), CANT_ASK);
    }

    /// A session started before an MCP server, the search indexes or
    /// skills were there is told that its agents don't get them.
    #[test]
    fn a_resumed_session_says_which_tools_its_agents_lack() {
        let opus = ModelSettings {
            model: resolve_model("opus").unwrap().id,
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let plain = AgentSettings::for_test(&opus, &opus);
        let saved = SavedSetup {
            name: WORKFLOW_NAME.into(),
            configs: [
                (MAIN.to_string(), agents::main_config(&plain).unwrap()),
                (WORKER.to_string(), agents::worker_config(&plain).unwrap()),
            ]
            .into(),
        };
        let fresh = |settings: &AgentSettings| {
            let main = agents::main_config(settings).unwrap();
            let worker = agents::worker_config(settings).unwrap();
            unoffered(&saved, &[(MAIN, &main), (WORKER, &worker)])
        };
        assert_eq!(fresh(&plain), None);
        let tools = [
            "mcp__github__create_issue".to_string(),
            "mcp__db__query".to_string(),
        ];
        let servers = ["github".to_string(), "db".to_string()];
        let folders = [PathBuf::from("/work/.claude/skills")];
        let note = fresh(&AgentSettings {
            mcp_tools: &tools,
            mcp_servers: &servers,
            skill_folders: &folders,
            index_tools: index::TOOL_NAMES,
            indexes: &[],
            ..plain
        })
        .unwrap();
        assert_eq!(
            note,
            "This session started without the search index tools, the tools of the MCP servers github and db, and skills, so its agents don't get them. A new session does."
        );
    }

    /// What `harness`'s agents are built from: its settings, without MCP
    /// servers, skills or indexes.
    fn agent_settings_of(harness: &Harness) -> AgentSettings<'_> {
        let setup = providers::Setup::new(harness.settings.openai_auth);
        AgentSettings::for_session(&harness.settings, Box::leak(Box::new(setup)))
    }

    /// A harness in `temp` on the model server's client, with workers;
    /// with a terminal when `interactive`. Returns it, its inputs, and
    /// what sends the user's input.
    pub(crate) fn harness(
        temp: &Path,
        client: LLMClient,
        interactive: bool,
    ) -> (Harness, Inputs, mpsc::UnboundedSender<Input>) {
        let luna = ModelSettings {
            model: resolve_model("luna").unwrap().id,
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let workspace = temp.join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let settings = Settings::for_test(&workspace, &luna);
        let setup = providers::Setup::new(None);
        let agent_settings = AgentSettings::for_session(&settings, &setup);
        let main_config = agents::main_config(&agent_settings).unwrap();
        let worker_config = agents::worker_config(&agent_settings).unwrap();
        let root = temp.join("arko");
        let session = Session::create(&root).unwrap();
        let client = Arc::new(client);
        let output = Arc::new(Output::new(None, true));
        let stats = Arc::new(Mutex::new(Stats::default()));
        let feed = remote::Feed::new(MAIN);
        // (A ChatGPT login's client counts as a key's here: no test
        // depends on the difference.)
        let keys = keys::Keys::new(
            client.clone(),
            client
                .has_provider(&arko_ai::Provider::OpenAI)
                .then_some(providers::OpenAiAuth::ApiKey),
            None,
            Some(feed.clone()),
        );
        let (crew, woken) = Crew::new(crew::Setup {
            session_id: session.id.clone(),
            session_dir: session.dir.clone(),
            output: output.clone(),
            max_running: NO_LIMIT,
            cache_1h: false,
            cache_lifetime: arko_ai::prompt_cache::PromptCacheLifetime::Known(Duration::from_secs(
                300,
            )),
            feed: Some(feed.clone()),
            refuse: Some({
                let keys = keys.clone();
                Arc::new(move |model: &arko_ai::Model| keys.refusal(model))
            }),
        });
        let (questions_sender, questions) = mpsc::unbounded_channel();
        let (input_sender, receiver) = mpsc::unbounded_channel();
        let (remote_actions, remote_input) = mpsc::unbounded_channel();
        let outside = Arc::new(outside::Outside::new(false, Vec::new(), workspace.clone()));
        let background = Arc::<shell::Background>::default();
        let handover = Arc::new(handover::Handover::new(
            Arc::default(),
            None,
            None,
            output.clone(),
            feed.clone(),
        ));
        let gauges = Arc::new(Gauges {
            models: Mutex::default(),
            conversations: Mutex::default(),
            workflow: Mutex::new(None),
            crew: Mutex::new(None),
            stats: stats.clone(),
            outside: outside.clone(),
            handover: handover.clone(),
            keys: keys.clone(),
            text_replies: Default::default(),
            cc_compact: Default::default(),
            extra_rules: Default::default(),
            project: Default::default(),
            background: background.clone(),
            page: Mutex::default(),
            changed: Default::default(),
            title: Default::default(),
            published: Default::default(),
        });
        let details = Arc::new(details::Details::new(
            session.id.clone(),
            session.dir.clone(),
            root.clone(),
            gauges.clone(),
            None,
            Vec::new(),
            details::McpFacts::of(&mcp::Running::default(), &[], &root),
        ));
        let mut harness = Harness {
            ask_about_workers: false,
            input_source: Source::Terminal,
            details,
            settings,
            providers: providers::Setup::new(None),
            session,
            client,
            keys,
            main_config,
            worker_config,
            tools: Vec::new(),
            root,
            mcp: mcp::Running::default(),
            skill_folders: Vec::new(),
            indexes: None,
            unoffered: None,
            output,
            stats,
            workflow: None,
            printer: None,
            conversation: Conversation::New,
            conversations: Default::default(),
            project: Default::default(),
            unread: Vec::new(),
            switched: false,
            queue_lines: !interactive,
            exiting: false,
            reopen: false,
            edited: None,
            cut_back: None,
            refresh_turn: false,
            only_workers: false,
            images: Default::default(),
            pending_images: Vec::new(),
            pending_refs: Vec::new(),
            crew: Some(crew),
            asker: Arc::new(Asker {
                questions: interactive.then_some(questions_sender),
            }),
            one_shot: !interactive,
            exit_warned: false,
            jobs_shown: Vec::new(),
            quit_popup: None,
            news_hint_shown: false,
            feed,
            remote: None,
            remote_actions,
            remote_bind: None,
            gauges,
            outside,
            handover,
            background,
        };
        harness.open(false).unwrap();
        let inputs = Inputs {
            receiver,
            pending: None,
            questions,
            woken: Some(woken),
            remote: remote_input,
            requests: mpsc::unbounded_channel().1,
            finished: None,
            late: None,
        };
        (harness, inputs, input_sender)
    }

    fn summary(harness: &Harness) -> String {
        match &harness.conversation {
            Conversation::Answered(result) => result.summary.clone(),
            _ => panic!("the conversation wasn't answered"),
        }
    }

    /// The status is read without the cost ledger, which grows with every
    /// call: run with --ignored --nocapture for the times, with 10,000
    /// entries, of a read and of what each read did before (the list price
    /// from a copy of the ledger).
    #[tokio::test]
    #[ignore]
    async fn reading_the_status_doesnt_walk_the_ledger() {
        let temp = tempfile::tempdir().unwrap();
        let (harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), false);
        let workflow = harness.workflow().clone();
        for n in 0..10_000 {
            workflow
                .context()
                .budget
                .add_manual_cost(0.0001, &format!("cost {n}"), None);
        }
        let reads = 200;
        let start = std::time::Instant::now();
        for _ in 0..reads {
            std::hint::black_box(remote::Meter::read(harness.gauges.as_ref()));
        }
        let read = start.elapsed() / reads;
        let start = std::time::Instant::now();
        for _ in 0..reads {
            std::hint::black_box(providers::api_price(&workflow.cost_ledger()));
        }
        let ledger = start.elapsed() / reads;
        println!(
            "10,000 ledger entries: a status read {read:?}; a copy of the ledger and its list price {ledger:?}"
        );
        assert!((remote::Meter::read(harness.gauges.as_ref()).cost - 1.0).abs() < 1e-6);
    }

    /// `/stop-workers WORKER` stops that one; a worker not running, or none
    /// by that name, is said.
    #[tokio::test]
    async fn one_worker_is_stopped_by_name() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        #[derive(Clone, Default)]
        struct Printed(Arc<Mutex<Vec<String>>>);
        impl ui::Printer for Printed {
            fn print(&mut self, text: String) -> Result<(), ()> {
                self.0
                    .lock()
                    .unwrap()
                    .extend(text.lines().map(str::to_string));
                Ok(())
            }
        }
        let hold = Arc::new(tokio::sync::Notify::new());
        let server = ModelServer::start(Arc::new({
            let hold = hold.clone();
            move |_, body: String| {
                let hold = hold.clone();
                Box::pin(async move {
                    if body.contains("You are agent run r1") {
                        hold.notified().await;
                        complete("w", "Counted.")
                    } else if body.contains("Started r1") {
                        complete("m2", "r1 is counting.")
                    } else {
                        tool_call(
                            "m1",
                            crew::START_AGENT,
                            serde_json::json!({"agent": WORKER, "task": "Count the files."}),
                        )
                    }
                })
            }
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), false);
        let end = harness
            .turn(Prompt::User("How many files?".into()), &mut inputs)
            .await;
        assert!(matches!(end, TurnEnd::Answered));
        let crew = harness.crew.clone().unwrap();
        assert_eq!(crew.running(), ["r1"]);
        let printed = Printed::default();
        harness.output = Arc::new(Output::new(Some(Box::new(printed.clone())), false));
        let said = |command: &str, harness: &Harness| {
            harness.command(command);
            printed
                .0
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(
            said("/stop-workers r9", &harness),
            "There is no worker r9. /workers lists them."
        );
        assert_eq!(
            said("/stop-workers r1", &harness),
            "Stopping r1: a message from the agent resumes it."
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while !crew.running().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(said("/stop-workers r1", &harness), "r1 isn't running.");
        assert_eq!(command("/stop-workers r1"), Some("/stop-workers r1"));
        crew.shutdown().await;
    }

    /// The status gives what the main agent's model accepts from the start,
    /// and each worker's context, as its latest request reported it, with
    /// its handover size.
    #[tokio::test]
    async fn the_status_reports_the_models_room_and_each_workers_context() {
        use crate::mock_model::{ModelServer, complete, tool_call, with_input};
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    with_input(complete("w", "There are 3 files."), 7_777)
                } else if body.contains("There are 3 files.") {
                    complete("m3", "Final: 3 files.")
                } else if body.contains("Started r1") {
                    complete("m2", "r1 is counting.")
                } else {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": "Count the files."}),
                    )
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), false);
        let luna = resolve_model("luna").unwrap();
        let reading = remote::Meter::read(harness.gauges.as_ref());
        assert_eq!(reading.context_window, Some(luna.max_input_tokens));
        assert!(reading.agents.is_empty());
        let end = harness
            .turn(Prompt::User("How many files?".into()), &mut inputs)
            .await;
        assert!(matches!(end, TurnEnd::Answered));
        let end =
            tokio::time::timeout(Duration::from_secs(30), harness.finish_workers(&mut inputs))
                .await
                .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        // /workers, as data.
        let workers = harness.details.workers();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].id, "r1");
        assert_eq!(workers[0].task, "Count the files.");
        assert_eq!(workers[0].report.as_deref(), Some("There are 3 files."));
        let cost = harness.details.cost();
        assert!(
            cost.main.input > 0 && cost.workers.input >= 7_777,
            "{cost:?}"
        );
        let reading = remote::Meter::read(harness.gauges.as_ref());
        assert_eq!(reading.agents.len(), 1, "{:?}", reading.agents);
        let r1 = &reading.agents[0];
        assert_eq!(r1.id, "r1");
        assert_eq!(r1.context_tokens, Some(7_777));
        assert_eq!(r1.handover_at, Some(200_000), "luna's own size");
        assert!(!r1.compacts);
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// What /session, /mcp and /skills say is there as data too, as the
    /// session changes.
    #[tokio::test]
    async fn the_details_answer_as_data() {
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
        let session = harness.details.session();
        assert_eq!(session.id, harness.session.id);
        assert_eq!(session.workspace, harness.settings.workspace);
        assert_eq!(session.main.model, resolve_model("luna").unwrap().id);
        assert_eq!(
            session.main.shown,
            harness.settings.describe(&harness.settings.main)
        );
        assert_eq!(
            session.workers.as_ref().map(|workers| workers.max_running),
            Some(None)
        );
        assert!(!session.outside);
        assert!(harness.live_command("/outside on", Source::Terminal));
        assert!(
            harness.details.session().outside,
            "published by the command"
        );
        let mcp = harness.details.mcp();
        assert!(mcp.servers.is_empty());
        assert_eq!(mcp.user_file, temp.path().join("arko").join(mcp::USER_FILE));
        let skills = harness.details.skills().unwrap();
        assert!(skills.skills.is_empty());
        assert_eq!(skills.own_folder, temp.path().join("arko").join("skills"));
        assert!(harness.details.workers().is_empty());
        let cost = harness.details.cost();
        assert_eq!((cost.billed(), cost.embeddings), (0.0, None));
    }

    /// What a front end asks waits while Arko only waits for the workers
    /// (no terminal), and a switch between Chat and Agent waits for what the
    /// user typed to reach the agent: each says so.
    #[tokio::test]
    async fn requests_say_what_they_wait_for() {
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
        harness.open(false).unwrap();
        harness.only_workers = true;
        let (reply, answer) = oneshot::channel();
        harness
            .request(crate::engine::Request::RefreshCache { reply })
            .await;
        assert_eq!(
            answer.await.unwrap(),
            Err("Arko is only waiting for the workers now.".to_string())
        );
        harness.only_workers = false;
        harness.unread = vec!["also the tests".into()];
        let (reply, answer) = oneshot::channel();
        harness
            .request(crate::engine::Request::SwitchMode {
                to: Mode::Chat,
                workspace: None,
                reply,
            })
            .await;
        let refused = answer.await.unwrap().unwrap_err();
        assert!(refused.contains("typed during the last turn"), "{refused}");
        assert_eq!(harness.settings.mode, Mode::Agent);
        if let Some(crew) = &harness.crew {
            crew.shutdown().await;
        }
    }

    /// A session is named by its first message's start until the user
    /// renames it (also during a turn); an empty name goes back.
    #[tokio::test]
    async fn the_session_is_named_by_its_first_message_or_by_the_user() {
        use crate::mock_model::{ModelServer, complete};
        let server = ModelServer::start(Arc::new(move |_, _| {
            Box::pin(async move { complete("1", "Done.") })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), false);
        let title = |harness: &Harness| remote::Meter::read(harness.gauges.as_ref()).title;
        let end = harness
            .turn(
                Prompt::User("Count the pears in the basket.".into()),
                &mut inputs,
            )
            .await;
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(title(&harness), "Count the pears in the basket.");
        assert_eq!(
            harness.session.load_settings().unwrap().title.as_deref(),
            Some("Count the pears in the basket.")
        );
        async fn rename(harness: &mut Harness, name: &str) -> Result<(), String> {
            let (reply, answer) = oneshot::channel();
            harness
                .request(crate::engine::Request::Rename {
                    title: name.into(),
                    reply,
                })
                .await;
            answer.await.unwrap()
        }
        assert_eq!(rename(&mut harness, "Pears").await, Ok(()));
        assert_eq!(title(&harness), "Pears");
        assert_eq!(rename(&mut harness, "").await, Ok(()));
        assert_eq!(title(&harness), "Count the pears in the basket.");
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// `/new` and `/resume N` mark where the conversation changes in the
    /// feed: a client showing one conversation at a time starts after the
    /// mark, one keeping them all draws it as a separator.
    #[tokio::test]
    async fn new_and_resume_put_a_separator_in_the_feed() {
        use crate::mock_model::{ModelServer, complete};
        let server = ModelServer::start(Arc::new(move |_, _| {
            Box::pin(async move { complete("1", "Done.") })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        let turn = |text: &str| Prompt::User(text.into());
        let end = harness.turn(turn("Count the pears."), &mut inputs).await;
        assert!(matches!(end, TurnEnd::Answered));
        harness.new_conversation();
        let end = harness.turn(turn("Count the apples."), &mut inputs).await;
        assert!(matches!(end, TurnEnd::Answered));
        harness.resume_command("1").await;
        let marks: Vec<String> = harness
            .feed
            .subscribe()
            .0
            .items
            .into_iter()
            .filter(|item| item.kind == remote::Kind::Conversation)
            .map(|item| item.text)
            .collect();
        assert_eq!(
            marks,
            ["New conversation", "Conversation 1 · Count the pears."]
        );
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// A resumed conversation's size is in the status before its next
    /// request says it.
    #[tokio::test]
    async fn a_resumed_conversation_reports_its_size_at_once() {
        use crate::mock_model::{ModelServer, complete, with_input};
        let server = ModelServer::start(Arc::new(move |_, _| {
            Box::pin(async move { with_input(complete("1", "Done."), 4_242) })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), false);
        let end = harness
            .turn(Prompt::User("Do it.".into()), &mut inputs)
            .await;
        assert!(matches!(end, TurnEnd::Answered));
        // As a new Arko resumes it.
        lock(&harness.stats).context_tokens = None;
        harness.open(true).unwrap();
        let reading = remote::Meter::read(harness.gauges.as_ref());
        assert_eq!(reading.context_tokens, Some(4_242));
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// With -p, the agent starts a worker and replies; Arko waits for the
    /// worker, and its report starts a turn whose reply is the answer.
    #[tokio::test]
    async fn a_one_shot_turn_waits_for_its_workers_and_hears_their_news() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let hold = release.clone();
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            seen.lock().unwrap().push(body.clone());
            let hold = hold.clone();
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    hold.notified().await;
                    complete("w", "There are 3 files.")
                } else if body.contains("There are 3 files.") {
                    complete("m3", "Final: 3 files.")
                } else if body.contains("Started r1") {
                    complete("m2", "r1 is counting; its report follows.")
                } else {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": "Count the files."}),
                    )
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), false);
        let end = harness
            .turn(Prompt::User("How many files?".into()), &mut inputs)
            .await;
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "r1 is counting; its report follows.");
        let crew = harness.crew.clone().unwrap();
        assert_eq!(crew.running(), ["r1"]);
        assert!(harness.news_coming());

        // An app asks before quitting with a worker running.
        let (popup, mut asks) = mpsc::unbounded_channel();
        harness.quit_popup = Some(popup);
        harness.ask_about_workers = true;
        assert!(!harness.may_exit(Some(Vec::new())));
        let asked = asks.try_recv().unwrap();
        assert_eq!(
            (asked.jobs.len(), asked.workers),
            (0, vec!["r1".to_string()])
        );
        harness.quit_popup = None;

        release.notify_one();
        let end =
            tokio::time::timeout(Duration::from_secs(30), harness.finish_workers(&mut inputs))
                .await
                .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "Final: 3 files.");
        assert!(!harness.news_coming());
        let bodies = bodies.lock().unwrap().clone();
        let news = bodies
            .iter()
            .find(|body| body.contains("There are 3 files.") && !body.contains("You are agent run"))
            .unwrap();
        assert!(
            news.contains("Arko started this turn for it; the user hasn't written"),
            "{news}"
        );
        crew.shutdown().await;
    }

    /// Both directions share arko's mailbox, and a wait returns the report whole.
    #[tokio::test]
    async fn workers_and_lead_exchange_messages_and_reports_mid_turn() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let server = ModelServer::start(Arc::new(move |_, body: String| Box::pin(async move {
            if body.contains("You are agent run r1") {
                if body.contains("Use the blue branch.") { complete("w3", "Blue branch checked.") }
                else if body.contains("Sent. Your lead") {
                    tool_call("w2", crew::WAIT_FOR_AGENTS, serde_json::json!({"timeout_seconds": 10}))
                } else { tool_call("w1", crew::MESSAGE_AGENT, serde_json::json!({"to":"lead", "message":"Which branch should I check?"})) }
            } else if body.contains("Blue branch checked.") { complete("m5", "The blue branch is checked.") }
            else if body.contains("Use the blue branch.") { tool_call("m4", crew::WAIT_FOR_AGENTS, serde_json::json!({"runs":["r1"],"all":true,"timeout_seconds":10})) }
            else if body.contains("Which branch should I check?") { tool_call("m3", crew::MESSAGE_AGENT, serde_json::json!({"to":"r1", "message":"Use the blue branch."})) }
            else if body.contains("Started r1") { tool_call("m2", crew::WAIT_FOR_AGENTS, serde_json::json!({"timeout_seconds":10})) }
            else { tool_call("m1", crew::START_AGENT, serde_json::json!({"agent":WORKER,"task":"Check a branch after asking which one."})) }
        }))).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        let end = tokio::time::timeout(
            Duration::from_secs(30),
            harness.turn(Prompt::User("Check it.".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "The blue branch is checked.");
        harness.flush_printer().await;
        assert!(has(
            &harness.feed,
            "call",
            "message lead: Which branch should I check?",
            ""
        ));
        // The worker's own transcript, its place in the lead's, and its
        // summary.
        let (snapshot, _) = harness.feed.subscribe();
        assert!(
            snapshot
                .items
                .iter()
                .any(|item| item.kind == remote::Kind::WorkerLine
                    && item.agent == remote::MAIN_AGENT
                    && item.worker.as_deref() == Some("r1"))
        );
        assert!(snapshot.items.iter().any(|item| item.agent == "r1"
            && item.kind == remote::Kind::Reply
            && item.text == "Blue branch checked."));
        let r1 = &snapshot.workers[0];
        assert_eq!(r1.id, "r1");
        assert_eq!(r1.title, "Check a branch after asking which one.");
        assert_eq!(r1.state, remote::WorkerState::Finished);
        assert_eq!(r1.tool_calls, 2);
        let ledger = harness.workflow().context().budget.total_spent();
        assert!(harness.workflow().cost_ledger().entries.iter().any(|entry| {
            matches!(&entry.source, CostSource::Agent { agent_name, .. } if agent_name == WORKER)
                && entry.amount > 0.0
        }));
        assert_eq!(harness.spent(), ledger);
        let reading = remote::Meter::read(harness.gauges.as_ref());
        assert_eq!(reading.cost, ledger);
        assert_eq!(reading.api_price, None, "nothing ran on a plan");
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// Ctrl-C preserves the worker and the workflow; /new transfers its ownership.
    #[tokio::test]
    async fn a_lead_stop_and_new_conversation_keep_workers_alive() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let worker_release = Arc::new(tokio::sync::Notify::new());
        let lead_waiting = Arc::new(tokio::sync::Notify::new());
        let hold = worker_release.clone();
        let ready = lead_waiting.clone();
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            let hold = hold.clone();
            let ready = ready.clone();
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    hold.notified().await;
                    complete("w", "Worker survived.")
                } else if body.contains("Worker survived.") {
                    complete("m4", "Received the surviving worker's report.")
                } else if body.contains("Fresh lead") {
                    if body.contains("agent `worker`") {
                        complete("m3", "I own r1 now.")
                    } else {
                        tool_call("m2", crew::LIST_AGENTS, serde_json::json!({}))
                    }
                } else if body.contains("Started r1") {
                    ready.notify_one();
                    std::future::pending::<String>().await
                } else {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent":WORKER,"task":"Stay running."}),
                    )
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
        let workflow = harness.workflow().clone();
        let interrupt = tokio::spawn(async move {
            lead_waiting.notified().await;
            sender.send(Input::Interrupt).unwrap();
            sender
        });
        let end = tokio::time::timeout(
            Duration::from_secs(25),
            harness.turn(Prompt::User("Start background work.".into()), &mut inputs),
        )
        .await
        .unwrap();
        let _sender = interrupt.await.unwrap();
        assert!(matches!(end, TurnEnd::Stopped));
        assert!(Arc::ptr_eq(&workflow, harness.workflow()));
        assert_eq!(harness.crew.as_ref().unwrap().running(), ["r1"]);
        let previous = workflow.subagents()[0].caller_run_id;
        harness.conversation = Conversation::New;
        let end = tokio::time::timeout(
            Duration::from_secs(15),
            harness.turn(Prompt::User("Fresh lead".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "I own r1 now.");
        let current = match &harness.conversation {
            Conversation::Answered(result) => result.agent_run_id,
            _ => unreachable!(),
        };
        assert_ne!(previous, current);
        assert_eq!(workflow.subagents()[0].caller_run_id, current);
        worker_release.notify_one();
        let end =
            tokio::time::timeout(Duration::from_secs(20), harness.finish_workers(&mut inputs))
                .await
                .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "Received the surviving worker's report.");
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    #[tokio::test]
    async fn resuming_a_session_restarts_its_interrupted_native_workers() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let workers = Arc::new(AtomicUsize::new(0));
        let seen = workers.clone();
        let started = Arc::new(tokio::sync::Notify::new());
        let ready = started.clone();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let kept = bodies.clone();
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            let seen = seen.clone();
            let ready = ready.clone();
            kept.lock().unwrap().push(body.clone());
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                        ready.notify_one();
                        std::future::pending::<String>().await
                    } else {
                        complete("w2", "Recovered the worker.")
                    }
                } else if body.contains("Recovered the worker.") {
                    complete("m3", "Worker recovery finished.")
                } else if body.contains("Started r1") {
                    complete("m2", "Worker is running.")
                } else {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent":WORKER,"task":"Resume this work."}),
                    )
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        assert!(matches!(
            harness
                .turn(Prompt::User("Start it.".into()), &mut inputs)
                .await,
            TurnEnd::Answered
        ));
        tokio::time::timeout(Duration::from_secs(10), started.notified())
            .await
            .unwrap();
        harness.crew.as_ref().unwrap().shutdown().await;
        harness.open(true).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(25), harness.finish_workers(&mut inputs))
                .await
                .unwrap(),
            TurnEnd::Answered
        ));
        assert_eq!(summary(&harness), "Worker recovery finished.");
        assert_eq!(workers.load(Ordering::SeqCst), 2);
        // The lead isn't told that r1 stopped as Arko closed: it goes on.
        let told = bodies.lock().unwrap().clone();
        assert!(
            told.iter()
                .filter(|body| !body.contains("You are agent run r1"))
                .all(|body| !body.contains("stopped before it finished")),
            "{told:#?}"
        );
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    #[tokio::test]
    async fn legacy_sessions_explain_retired_tools_and_keep_their_archives() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            Box::pin(async move {
                // The stub's successful result allows ordinary completion after an obsolete call.
                if body.contains("This session uses the old worker tools.")
                    && body.contains("call_old")
                {
                    complete("done", "A new Arko session is needed for native workers.")
                } else if body.contains("Try the old tool") {
                    tool_call(
                        "old",
                        "start_worker",
                        serde_json::json!({"task":"Archived work"}),
                    )
                } else {
                    complete("saved", "Saved with old tools.")
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        harness
            .main_config
            .tool_config
            .available_tool_names
            .retain(|name| !crew::LEAD_TOOL_NAMES.contains(&name.as_str()));
        harness
            .main_config
            .tool_config
            .available_tool_names
            .push("start_worker".into());
        harness
            .tools
            .push(Arc::new(crew::LegacyTool("start_worker")));
        harness.crew = None;
        harness.open(false).unwrap();
        assert!(matches!(
            harness
                .turn(Prompt::User("Save it.".into()), &mut inputs)
                .await,
            TurnEnd::Answered
        ));
        let archive = harness.session.dir.join("workers");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::write(archive.join("workers.json"), "old workers").unwrap();
        harness.tools.clear();
        harness.open(true).unwrap();
        assert!(harness.crew.is_none());
        assert!(
            harness
                .unoffered
                .as_ref()
                .unwrap()
                .contains("/new resets this conversation but keeps its old tools")
        );
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(15),
                harness.turn(Prompt::User("Try the old tool".into()), &mut inputs)
            )
            .await
            .unwrap(),
            TurnEnd::Answered
        ));
        assert_eq!(
            std::fs::read_to_string(archive.join("workers.json")).unwrap(),
            "old workers"
        );
    }

    #[tokio::test]
    async fn a_finished_worker_keeps_its_worktree_and_context_for_follow_up() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let server = ModelServer::start(Arc::new(move |_, body: String| Box::pin(async move {
            if body.contains("You are agent run r1") {
                if body.contains("Review the second pass.") { complete("w2", "Second pass reviewed with the original context.") }
                else { complete("w1", "First pass reviewed.") }
            } else if body.contains("Take over the saved worker") {
                if body.contains("Review the committed files.") { complete("m8", "Ownership moved.") }
                else { tool_call("m7", crew::LIST_AGENTS, serde_json::json!({})) }
            } else if body.contains("Second pass reviewed with the original context.") { complete("m6", "Both passes done.") }
            else if body.contains("Review the second pass.") { tool_call("m5", crew::WAIT_FOR_AGENTS, serde_json::json!({"all":true,"timeout_seconds":10})) }
            else if body.contains("Follow up with r1") { tool_call("m4", crew::MESSAGE_AGENT, serde_json::json!({"to":"r1","message":"Review the second pass."})) }
            else if body.contains("First pass reviewed.") { complete("m3", "First pass done.") }
            else if body.contains("Started r1") { tool_call("m2", crew::WAIT_FOR_AGENTS, serde_json::json!({"all":true,"timeout_seconds":10})) }
            else { tool_call("m1", crew::START_AGENT, serde_json::json!({"agent":WORKER,"task":"Review the committed files.","isolated":true})) }
        }))).await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.invalid"],
            vec!["config", "user.name", "Test"],
            vec!["commit", "--allow-empty", "-qm", "Initial"],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&harness.settings.workspace)
                .status()
                .unwrap();
            assert!(status.success());
        }
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(20),
                harness.turn(Prompt::User("Review it.".into()), &mut inputs)
            )
            .await
            .unwrap(),
            TurnEnd::Answered
        ));
        let first = harness.workflow().subagents()[0].clone();
        let isolated = first.isolated.as_ref().unwrap();
        assert_eq!(isolated.path, harness.session.dir.join("worktrees/r1"));
        assert_eq!(
            isolated.branch.as_deref(),
            Some(format!("arko/{}/r1", harness.session.id).as_str())
        );
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(20),
                harness.turn(Prompt::User("Follow up with r1".into()), &mut inputs)
            )
            .await
            .unwrap(),
            TurnEnd::Answered
        ));
        assert_eq!(summary(&harness), "Both passes done.");
        let second = harness.workflow().subagents()[0].clone();
        assert_eq!(first.run_id, second.run_id);
        assert_eq!(first.workspace, second.workspace);
        assert!(second.isolated.unwrap().path.exists());
        // A failed lead starts a fresh conversation after resume. Its workers
        // still belong to its saved run until the new lead's first tool.
        {
            let mut results = harness.workflow().context().agent_results.write();
            let lead = results
                .iter_mut()
                .rfind(|run| run.agent_name == MAIN)
                .unwrap();
            lead.status = AgentRunStatus::Failed;
            lead.task.set_state(arko_agents::TaskState::Failed {
                reason: "Saved model failure".into(),
                cause: None,
                error_kind: None,
            });
        }
        harness.workflow().checkpoint().await.unwrap();
        harness.open(true).unwrap();
        assert!(matches!(harness.conversation, Conversation::New));
        assert!(matches!(
            harness
                .turn(
                    Prompt::User("Take over the saved worker".into()),
                    &mut inputs
                )
                .await,
            TurnEnd::Answered
        ));
        assert_eq!(summary(&harness), "Ownership moved.");
        let lead = match &harness.conversation {
            Conversation::Answered(run) => run.agent_run_id,
            _ => unreachable!(),
        };
        assert_eq!(harness.workflow().subagents()[0].caller_run_id, lead);
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// The runtime injects reports during a running lead turn without a wait tool.
    #[tokio::test]
    async fn a_worker_report_arrives_at_the_leads_next_tool_boundary() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let worker_release = Arc::new(tokio::sync::Notify::new());
        let lead_release = Arc::new(tokio::sync::Notify::new());
        let hold_worker = worker_release.clone();
        let hold_lead = lead_release.clone();
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            let hold_worker = hold_worker.clone();
            let hold_lead = hold_lead.clone();
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    hold_worker.notified().await;
                    complete("w1", "The report arrived mid-turn.")
                } else if body.contains("The report arrived mid-turn.") {
                    complete("m3", "I read the report before ending my turn.")
                } else if body.contains("Started r1") {
                    hold_lead.notified().await;
                    tool_call(
                        "m2",
                        message::MESSAGE_USER,
                        serde_json::json!({"message":"Still working."}),
                    )
                } else {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent":WORKER,"task":"Report while the lead works."}),
                    )
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        let workflow = harness.workflow().clone();
        let release = tokio::spawn(async move {
            worker_release.notify_one();
            while workflow.subagents().iter().all(|run| run.report.is_none()) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            lead_release.notify_one();
        });
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(20),
                harness.turn(Prompt::User("Do both.".into()), &mut inputs)
            )
            .await
            .unwrap(),
            TurnEnd::Answered
        ));
        release.await.unwrap();
        assert_eq!(
            summary(&harness),
            "I read the report before ending my turn."
        );
        assert!(!harness.crew.as_ref().unwrap().has_mail());
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    #[tokio::test]
    async fn resuming_with_pending_reports_wakes_the_idle_prompt() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let release = Arc::new(tokio::sync::Notify::new());
        let hold = release.clone();
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            let hold = hold.clone();
            Box::pin(async move {
                if body.contains("You are agent run r1") {
                    hold.notified().await;
                    complete("w", "A report saved for later.")
                } else if body.contains("A report saved for later.") {
                    complete("m3", "Read the saved report.")
                } else if body.contains("Started r1") {
                    complete("m2", "Waiting in the background.")
                } else {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent":WORKER,"task":"Save a report."}),
                    )
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        assert!(matches!(
            harness
                .turn(Prompt::User("Start it.".into()), &mut inputs)
                .await,
            TurnEnd::Answered
        ));
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !harness.crew.as_ref().unwrap().has_mail() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        harness.crew.as_ref().unwrap().shutdown().await;
        inputs.drain_woken();
        harness.open(true).unwrap();
        assert!(harness.crew.as_ref().unwrap().running().is_empty());
        assert!(harness.crew.as_ref().unwrap().has_mail());
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), inputs.next_event(true))
                .await
                .unwrap(),
            Event::Woken
        ));
        assert!(matches!(
            harness.deliver_news(&mut inputs).await,
            News::Done
        ));
        assert_eq!(summary(&harness), "Read the saved report.");
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    #[tokio::test]
    async fn a_session_with_workers_disabled_resumes_without_a_migration_notice() {
        let server = crate::mock_model::ModelServer::start(Arc::new(move |_, _| {
            Box::pin(async { crate::mock_model::complete("m", "Done.") })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        harness.settings.max_workers = 0;
        harness.crew = None;
        harness
            .main_config
            .tool_config
            .available_tool_names
            .retain(|name| !crew::LEAD_TOOL_NAMES.contains(&name.as_str()));
        harness.open(false).unwrap();
        assert!(matches!(
            harness
                .turn(Prompt::User("Do it.".into()), &mut inputs)
                .await,
            TurnEnd::Answered
        ));
        harness.open(true).unwrap();
        assert!(harness.unoffered.is_none());
        assert!(harness.crew.is_none());
    }

    /// The agent's question is put to the user, and the next line typed
    /// answers it.
    #[tokio::test]
    async fn the_agent_asks_and_the_next_line_answers() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            Box::pin(async move {
                if body.contains("Two spaces, please.") {
                    complete("m2", "Indented with two spaces.")
                } else {
                    tool_call(
                        "m1",
                        ASK_USER_TOOL_NAME,
                        serde_json::json!({"questions": "Tabs or spaces?"}),
                    )
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
        let answering = tokio::spawn(async move {
            // The answer is typed once the question is up.
            tokio::time::sleep(Duration::from_millis(300)).await;
            sender
                .send(Input::Line("Two spaces, please.".into()))
                .unwrap();
            sender
        });
        let end = tokio::time::timeout(
            Duration::from_secs(30),
            harness.turn(Prompt::User("Indent the file.".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "Indented with two spaces.");
        drop(answering.await.unwrap());
    }

    /// An OpenAI organization that isn't verified for reasoning summaries
    /// refuses them: the request goes again without them and the turn goes
    /// on. The session says so once, no later request asks for them, and
    /// the main agent's saved configuration asks for none either.
    #[tokio::test]
    async fn refused_reasoning_summaries_are_dropped_with_one_notice() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body.clone());
                Box::pin(async move {
                    if body.contains(r#""summary":"auto""#) {
                        let error = serde_json::json!({"error": {
                            "message": "Your organization must be verified to generate reasoning summaries. Please go to: https://platform.openai.com/settings/organization/general and click on Verify Organization.",
                            "type": "invalid_request_error",
                            "param": "reasoning.summary",
                            "code": "unsupported_value",
                        }})
                        .to_string();
                        format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",
                            error.len()
                        )
                    } else if body.contains("Second.") {
                        complete("m3", "Again.")
                    } else if body.contains("call_m1") {
                        complete("m2", "Listed.")
                    } else {
                        tool_call("m1", "list_files", serde_json::json!({"directory": "."}))
                    }
                })
            }
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        for (message, reply) in [("First.", "Listed."), ("Second.", "Again.")] {
            let end = tokio::time::timeout(
                Duration::from_secs(30),
                harness.turn(Prompt::User(message.into()), &mut inputs),
            )
            .await
            .unwrap();
            assert!(matches!(end, TurnEnd::Answered), "{message}");
            assert_eq!(summary(&harness), reply);
        }
        harness.flush_printer().await;
        let bodies = bodies.lock().unwrap().clone();
        let asked = bodies
            .iter()
            .filter(|body| body.contains(r#""summary":"auto""#))
            .count();
        assert_eq!((asked, bodies.len()), (1, 4), "{bodies:#?}");
        let told = shown(&harness.feed)
            .into_iter()
            .filter(|(kind, text, _)| kind == "notice" && text == providers::THINKING_REFUSED)
            .count();
        assert_eq!(told, 1);
        let Conversation::Answered(result) = &harness.conversation else {
            unreachable!()
        };
        let generation = &result.task.agent_config.llm_config.generation_config;
        assert!(
            matches!(
                generation.reasoning,
                Some(arko_ai::ReasoningConfig::OpenAI {
                    include_summaries: false,
                    ..
                })
            ),
            "{:?}",
            generation.reasoning
        );
    }

    /// What the user types while the agent makes its last request comes
    /// too late for the turn: once the agent replies, a new turn takes it
    /// to the agent (once), as the workers' news starts one, and the agent
    /// answers it.
    #[tokio::test]
    async fn what_the_agent_finished_before_reading_starts_a_turn() {
        use crate::mock_model::{ModelServer, complete};
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let release = Arc::new(tokio::sync::Notify::new());
        let server = ModelServer::start(Arc::new({
            let (bodies, hold) = (bodies.clone(), release.clone());
            move |_, body: String| {
                bodies.lock().unwrap().push(body.clone());
                let hold = hold.clone();
                Box::pin(async move {
                    if body.contains("Also count the directories.") {
                        complete("m2", "And 2 directories.")
                    } else {
                        hold.notified().await;
                        complete("m1", "3 files.")
                    }
                })
            }
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, sender) = harness(temp.path(), server.client(), true);
        let feed = harness.feed.clone();
        let typing = tokio::spawn(async move {
            until(|| feed.activity().working).await;
            sender
                .send(Input::Line("Also count the directories.".into()))
                .unwrap();
            until(|| has(&feed, "user", "Also count the directories.", "note")).await;
            release.notify_one();
            sender
        });
        let end = tokio::time::timeout(
            Duration::from_secs(30),
            harness.turn(Prompt::User("Count the files.".into()), &mut inputs),
        )
        .await
        .unwrap();
        let _sender = typing.await.unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "And 2 directories.");
        assert!(harness.unread.is_empty());
        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2, "{bodies:#?}");
        assert!(
            bodies[1].contains("What I typed during your last turn"),
            "{}",
            bodies[1]
        );
        assert_eq!(
            bodies[1].matches("Also count the directories.").count(),
            1,
            "{}",
            bodies[1]
        );
        let shown = shown(&harness.feed);
        let said = |text: &str| {
            shown
                .iter()
                .any(|(kind, shown, _)| kind == "notice" && shown.contains(text))
        };
        assert!(said("so a new turn takes it to the agent"), "{shown:#?}");
        assert!(!said("goes with your next message"), "{shown:#?}");
        let replies: Vec<&str> = shown
            .iter()
            .filter(|(kind, _, _)| kind == "reply")
            .map(|(_, text, _)| text.as_str())
            .collect();
        assert_eq!(replies, ["3 files.", "And 2 directories."]);
    }

    #[test]
    fn remote_takes_an_optional_address_and_not_with_a_prompt() {
        let parse = |args: &[&str]| Args::try_parse_from(args).map(|args| args.remote);
        assert_eq!(parse(&["arko"]).unwrap(), None);
        assert_eq!(
            parse(&["arko", "--remote"]).unwrap().as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            parse(&["arko", "--remote", "0.0.0.0:8765"])
                .unwrap()
                .as_deref(),
            Some("0.0.0.0:8765")
        );
        assert!(parse(&["arko", "-p", "hi", "--remote"]).is_err());
        assert!(command("/remote off").is_some() && command("/remotely").is_none());
    }

    /// The session's status says where its web page is served, with its
    /// links, while it is: the full-screen view's QR code and an app read it
    /// there.
    #[tokio::test]
    async fn the_status_has_the_web_page_while_it_is_served() {
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
        assert_eq!(remote::Meter::read(harness.gauges.as_ref()).page, None);
        harness
            .start_remote(remote::Bind {
                ip: std::net::Ipv4Addr::LOCALHOST.into(),
                port: Some(0),
            })
            .await;
        let addr = harness.remote.as_ref().expect("the page is on").addr();
        let page = remote::Meter::read(harness.gauges.as_ref()).page.unwrap();
        assert_eq!(page.addr, addr);
        assert!(
            page.links[0]
                .url
                .starts_with(&format!("http://{addr}/#key=")),
            "{page:?}"
        );
        harness.stop_remote().await;
        assert_eq!(remote::Meter::read(harness.gauges.as_ref()).page, None);
    }

    /// Serves `harness`'s web page on a free port; returns an HTTP client,
    /// the page's address and a session cookie.
    async fn web(harness: &mut Harness) -> (reqwest::Client, String, String) {
        harness
            .start_remote(remote::Bind {
                ip: std::net::Ipv4Addr::LOCALHOST.into(),
                port: Some(0),
            })
            .await;
        let addr = harness.remote.as_ref().expect("the page is on").addr();
        let key = std::fs::read_to_string(harness.session.dir.join(remote::KEY_FILE)).unwrap();
        let http = arko_ai::http_client();
        let base = format!("http://{addr}");
        let response = http
            .post(format!("{base}/api/session"))
            .body(serde_json::json!({ "key": key.trim() }).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        let cookie = response.headers()["set-cookie"].to_str().unwrap();
        let cookie = cookie.split(';').next().unwrap().to_string();
        (http, base, cookie)
    }

    /// The page's history, as (kind, text, delivery).
    fn shown(feed: &remote::Feed) -> Vec<(String, String, String)> {
        feed.subscribe()
            .0
            .items
            .iter()
            .map(|item| {
                let item = serde_json::to_value(item).unwrap();
                let text = |name: &str| item[name].as_str().unwrap_or_default().to_string();
                (text("kind"), text("text"), text("delivery"))
            })
            .collect()
    }

    fn has(feed: &remote::Feed, kind: &str, text: &str, delivery: &str) -> bool {
        shown(feed)
            .iter()
            .any(|item| item == &(kind.to_string(), text.to_string(), delivery.to_string()))
    }

    async fn until(condition: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("in time");
    }

    /// The agent's question shows on the page, and the answer sent from
    /// it reaches the agent; the page's history has the turn.
    #[tokio::test]
    async fn the_web_page_answers_the_agents_question() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            Box::pin(async move {
                if body.contains("Two spaces, please.") {
                    complete("m2", "Indented with two spaces.")
                } else {
                    tool_call(
                        "m1",
                        ASK_USER_TOOL_NAME,
                        serde_json::json!({"questions": "Tabs or spaces?"}),
                    )
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        let (http, base, cookie) = web(&mut harness).await;
        let feed = harness.feed.clone();
        let answering = tokio::spawn(async move {
            until(|| feed.activity().question.is_some()).await;
            let question = feed.activity().question.unwrap();
            assert_eq!(question.text, "Tabs or spaces?");
            let response = http
                .post(format!("{base}/api/answer"))
                .header("cookie", cookie)
                .body(
                    serde_json::json!({"question": question.id, "text": "Two spaces, please."})
                        .to_string(),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 202);
        });
        let end = tokio::time::timeout(
            Duration::from_secs(30),
            harness.turn(Prompt::User("Indent the file.".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "Indented with two spaces.");
        answering.await.unwrap();
        let feed = &harness.feed;
        assert!(has(feed, "question", "Tabs or spaces?", ""));
        assert!(has(feed, "user", "Two spaces, please.", "answer"));
        assert!(has(feed, "reply", "Indented with two spaces.", ""));
        assert_eq!(shown(feed).last().unwrap().0, "turn");
        assert!(!feed.activity().working && feed.activity().question.is_none());
        harness.stop_remote().await;
    }

    /// A message sent from the page while the agent works reaches it as a
    /// note; when the agent is idle, it comes in as typed lines do. The
    /// agent's calls show on the page.
    #[tokio::test]
    async fn a_message_from_the_web_page_is_a_note_while_the_agent_works() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let release = Arc::new(tokio::sync::Notify::new());
        let hold = release.clone();
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            let hold = hold.clone();
            Box::pin(async move {
                if body.contains("Also count the directories.") {
                    complete("m2", "3 files, 2 directories.")
                } else if body.contains("Count the files.") {
                    hold.notified().await;
                    tool_call("m1", crew::LIST_AGENTS, serde_json::json!({}))
                } else {
                    complete("m3", "Nothing to count.")
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), true);
        let (http, base, cookie) = web(&mut harness).await;
        let post = {
            let (http, base, cookie) = (http.clone(), base.clone(), cookie.clone());
            move |text: &'static str| {
                let (http, base, cookie) = (http.clone(), base.clone(), cookie.clone());
                async move {
                    let response = http
                        .post(format!("{base}/api/message"))
                        .header("cookie", cookie)
                        .body(serde_json::json!({ "text": text }).to_string())
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(response.status(), 202);
                }
            }
        };
        let feed = harness.feed.clone();
        let sending = tokio::spawn({
            let post = post.clone();
            async move {
                until(|| feed.activity().working).await;
                post("Also count the directories.").await;
                until(|| has(&feed, "user", "Also count the directories.", "note")).await;
                release.notify_one();
            }
        });
        let end = tokio::time::timeout(
            Duration::from_secs(30),
            harness.turn(Prompt::User("Count the files.".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "3 files, 2 directories.");
        sending.await.unwrap();
        assert!(has(&harness.feed, "call", "list workers", ""));

        post("Thanks.").await;
        let message = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Event::Remote(action) = inputs.next_event(true).await {
                    break action;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(message, Action::Message("Thanks.".into()));
        harness.stop_remote().await;
    }

    /// A command's warning (or error) reaches the feed with its level, so
    /// the clients show it in its colour; its other lines stay plain.
    #[tokio::test]
    async fn a_commands_warnings_keep_their_level_in_the_feed() {
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, _inputs, _sender) = harness(temp.path(), LLMClient::new(), true);
        harness.mirrored(|harness| {
            let out = harness.output.clone();
            out.line("All is well.");
            out.line(&out.warn("1 worker is running: /exit again stops it."));
            out.line(&out.error("The server didn't start."));
        });
        let (snapshot, _) = harness.feed.subscribe();
        let outputs: Vec<(String, Option<Level>)> = snapshot
            .items
            .iter()
            .filter(|item| item.kind == remote::Kind::Output)
            .map(|item| (item.text.clone(), item.level))
            .collect();
        assert_eq!(
            outputs,
            [
                ("All is well.".to_string(), None),
                (
                    "1 worker is running: /exit again stops it.".to_string(),
                    Some(Level::Warn)
                ),
                ("The server didn't start.".to_string(), Some(Level::Error)),
            ]
        );
    }

    /// Commands sent from the page run, and their output shows on it; the
    /// page can't quit Arko.
    #[tokio::test]
    async fn commands_from_the_web_page_show_their_output_there() {
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, sender) = harness(temp.path(), LLMClient::new(), true);
        let actions = harness.remote_actions.clone();
        let feed = harness.feed.clone();
        let typing = async move {
            actions.send(Action::Message("/exit".into())).unwrap();
            actions.send(Action::Message("/workers".into())).unwrap();
            until(|| shown(&feed).iter().any(|item| item.0 == "output")).await;
            sender.send(Input::Eof).unwrap();
        };
        let (_, ()) = tokio::time::timeout(
            Duration::from_secs(20),
            futures_util::future::join(harness.repl(&mut inputs), typing),
        )
        .await
        .unwrap();
        let shown = shown(&harness.feed);
        assert!(
            shown
                .iter()
                .any(|item| item.0 == "notice"
                    && item.1.contains("/exit works at the terminal only")),
            "{shown:?}"
        );
        assert!(has(&harness.feed, "user", "/workers", "command"));
        assert!(has(
            &harness.feed,
            "output",
            "No workers yet: the agent starts them as it needs them.",
            ""
        ));
    }

    /// Puts `harness` on the models `main` and `worker` with `setup`, with
    /// Arko's shell and terminal tools (agents in Claude Code are offered
    /// the tools registered for them, and only those). Returns the
    /// terminal managers, to shut down after.
    fn on_models(
        harness: &mut Harness,
        main: &ModelSettings,
        worker: &ModelSettings,
        setup: &providers::Setup,
    ) -> Managers {
        let shell = Shell::detect(WindowsShell::Cmd);
        let managers = Managers::open(&harness.session, &shell).unwrap();
        harness.tools = managers.tools(&shell, harness.handover.runs());
        harness.background = managers.background.clone();
        harness.outside = Arc::new(outside::Outside::new(
            harness.settings.allow_outside_workspace,
            managers.list(),
            harness.settings.workspace.clone(),
        ));
        let agent_settings = AgentSettings {
            providers: setup,
            max_workers: NO_LIMIT,
            ..AgentSettings::for_test(main, worker)
        };
        harness.main_config = agents::main_config(&agent_settings).unwrap();
        harness.worker_config = agents::worker_config(&agent_settings).unwrap();
        harness.settings.main = main.clone();
        harness.settings.worker = worker.clone();
        harness.providers = setup.clone();
        harness.open(false).unwrap();
        harness.start_new_workers();
        managers
    }

    /// Claude Code settings that run arko's scripted stand-in for the
    /// `claude` CLI, with its script, log and state in `fake`.
    fn fake_claude_code(fake: &Path) -> providers::Setup {
        let mut setup = providers::Setup::new(None);
        let python = std::env::var("ARKO_TEST_PYTHON")
            .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.into());
        setup.claude_code.cli_path = Some(python.into());
        setup.claude_code.cli_args = vec![
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../arko_framework/arko_agents/tests/fixtures/fake_claude_agent.py"
            )
            .into(),
        ];
        for (name, file) in [
            ("FAKE_CLAUDE_SCRIPT", "script.json"),
            ("FAKE_CLAUDE_LOG", "log.jsonl"),
            ("FAKE_CLAUDE_STATE", "state"),
        ] {
            setup
                .claude_code
                .env
                .insert(name.into(), fake.join(file).display().to_string());
        }
        setup
    }

    /// The fake CLI's log, and the arguments it was started with.
    fn fake_log(fake: &Path) -> (Vec<serde_json::Value>, Vec<String>) {
        let log: Vec<serde_json::Value> = std::fs::read_to_string(fake.join("log.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let argv = serde_json::from_value(
            log.iter().find(|entry| entry["event"] == "argv").unwrap()["argv"].clone(),
        )
        .unwrap();
        (log, argv)
    }

    /// The argument after `flag`.
    fn after(argv: &[String], flag: &str) -> Option<String> {
        argv.iter()
            .position(|arg| arg == flag)
            .map(|index| argv[index + 1].clone())
    }

    /// A turn on a model on OpenRouter: Arko sends OpenRouter's own
    /// reasoning, the 1-hour cache on a Claude slug, and its tools, and the
    /// cost is what OpenRouter reports.
    #[tokio::test]
    async fn a_turn_on_openrouter_sends_its_settings_and_counts_its_cost() {
        use crate::mock_model::{ModelServer, openrouter_tool_call};
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            seen.lock().unwrap().push(body);
            Box::pin(async move {
                openrouter_tool_call(
                    "1",
                    "complete_task",
                    serde_json::json!({"summary": "Done on OpenRouter."}),
                    0.0125,
                )
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) =
            harness(temp.path(), server.openrouter_client(), false);
        let model = ModelSettings {
            model: "openrouter:anthropic/claude-opus-5.5".into(),
            effort: Effort::Xhigh,
            fast: false,
            cache_1h: true,
            handover_at: Some(500_000),
        };
        let managers = on_models(&mut harness, &model, &model, &providers::Setup::new(None));
        let end = tokio::time::timeout(
            Duration::from_secs(30),
            harness.turn(Prompt::User("Do it.".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "Done on OpenRouter.");
        let body: serde_json::Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(body["model"], "anthropic/claude-opus-5.5");
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["cache_control"]["ttl"], "1h");
        let tools: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        assert!(tools.contains(&"shell"), "{tools:?}");
        assert!(
            (harness.spent() - 0.0125).abs() < 1e-9,
            "{}",
            harness.spent()
        );
        harness.crew.as_ref().unwrap().shutdown().await;
        managers.shutdown().await;
    }

    /// A turn on a Gemini model: Arko sends its thinking level (xhigh
    /// effort thinks high) and its tools, and the cost is priced from the
    /// registry.
    #[tokio::test]
    async fn a_turn_on_gemini_sends_its_thinking_level_and_counts_its_cost() {
        use crate::mock_model::{ModelServer, gemini_tool_call};
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            seen.lock().unwrap().push(body);
            Box::pin(async move {
                gemini_tool_call(
                    "1",
                    "complete_task",
                    serde_json::json!({"summary": "Done on Gemini."}),
                )
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) =
            harness(temp.path(), server.gemini_client(), false);
        let model = ModelSettings {
            model: "gemini-3.1-pro-preview".into(),
            effort: Effort::Xhigh,
            fast: false,
            cache_1h: false,
            handover_at: Some(200_000),
        };
        let managers = on_models(&mut harness, &model, &model, &providers::Setup::new(None));
        let end = tokio::time::timeout(
            Duration::from_secs(30),
            harness.turn(Prompt::User("Do it.".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "Done on Gemini.");
        let body: serde_json::Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(body["model"], "gemini-3.1-pro-preview");
        assert_eq!(
            body["generation_config"]["thinking_level"], "high",
            "{body}"
        );
        assert!(body.to_string().contains("\"shell\""), "Arko's tools");
        // 1000 input tokens at $2/M and 20 output at $12/M.
        assert!(
            (harness.spent() - 0.00224).abs() < 1e-9,
            "{}",
            harness.spent()
        );
        harness.crew.as_ref().unwrap().shutdown().await;
        managers.shutdown().await;
    }

    /// A turn on a Claude Code model (`cc:opus`), through arko's scripted
    /// stand-in for the `claude` CLI: the CLI is offered Arko's own tools and
    /// none of Claude Code's, Arko's shell runs the command and its guard
    /// refuses one outside the working directory, and the turn's spending
    /// shows as plan usage, not dollars.
    #[tokio::test]
    async fn a_turn_in_claude_code_runs_arko_s_tools_and_shows_plan_usage() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("fake");
        std::fs::create_dir_all(&fake).unwrap();
        // Outside the working directory and the temporary directories,
        // which the guard lets commands change; it doesn't exist, so a
        // command the guard let through would change nothing.
        let outside = format!("~/arko-guard-test-{}", uuid::Uuid::new_v4().simple());
        let script = serde_json::json!([[
            {"rate_limit": {"status": "allowed", "unifiedWindows": {
                "five_hour": {"utilization": 0.12}, "seven_day": {"utilization": 0.03}}}},
            {"text": "Making the file.", "input_tokens": 1200},
            {"tool": "mcp__arko__shell", "input": {"command": "echo hi > made.txt"}},
            {"tool": "mcp__arko__shell", "input": {"command": format!("rm -rf {outside}")}},
            {"tool": "mcp__arko__complete_task", "input": {"summary": "Made it."}},
        ]]);
        std::fs::write(fake.join("script.json"), script.to_string()).unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), LLMClient::new(), false);
        let cc = ModelSettings {
            model: "cc:claude-opus-5-5".into(),
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let managers = on_models(&mut harness, &cc, &cc, &fake_claude_code(&fake));

        let end = tokio::time::timeout(
            Duration::from_secs(60),
            harness.turn(Prompt::User("Make a file.".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "Made it.");
        let workspace = &harness.settings.workspace;
        assert_eq!(
            std::fs::read_to_string(workspace.join("made.txt"))
                .unwrap()
                .trim(),
            "hi"
        );

        let (log, argv) = fake_log(&fake);
        assert_eq!(
            after(&argv, "--tools").as_deref(),
            Some(""),
            "no built-ins: {argv:?}"
        );
        assert_eq!(after(&argv, "--model").as_deref(), Some("claude-opus-5-5"));
        assert_eq!(after(&argv, "--effort").as_deref(), Some("high"));
        assert!(
            after(&argv, "--system-prompt").is_some_and(|prompt| prompt.contains("You are Arko")),
            "Arko's prompt replaces Claude Code's: {argv:?}"
        );
        let served: Vec<String> =
            log.iter().find(|entry| entry["event"] == "tools").unwrap()["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_string())
                .collect();
        for tool in [
            "shell",
            "terminal_start",
            "read_file",
            "ask_user",
            "start_agent",
        ] {
            assert!(served.iter().any(|name| name == tool), "{tool}: {served:?}");
        }
        let refused = log
            .iter()
            .filter(|entry| entry["event"] == "post")
            .find(|entry| entry["tool"] == "mcp__arko__shell" && entry["is_error"] == true)
            .expect("the command guard refused the rm");
        assert!(
            refused["output"]
                .as_str()
                .unwrap()
                .contains("which is outside the working directory"),
            "{refused}"
        );

        harness.flush_printer().await;
        let stats = harness.stats.lock().unwrap().clone();
        assert_eq!(
            stats.plan_usage.as_deref(),
            Some("Claude plan 5 h 12%, 7 d 3%")
        );
        assert_eq!(harness.spent(), 0.0, "the plan, not dollars");
        let reading = remote::Meter::read(harness.gauges.as_ref());
        assert_eq!(reading.cost, 0.0);
        // What the same calls would cost at Claude Opus 5.5's list prices.
        let api_price = reading.api_price.unwrap();
        assert!(api_price > 0.0, "{api_price}");
        let plans = harness.stats.lock().unwrap().plans.clone();
        let lines =
            providers::cost_report(&harness.workflow().cost_ledger(), &harness.settings, &plans);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            [
                "$0.00 billed in this session · 1k input and 0k output tokens",
                &format!("≈ ${api_price:.2} at API prices: the Claude plan covers it"),
                "Claude plan 5 h 12%, 7 d 3%",
            ]
        );
        assert_eq!(
            reading.plan_usage.as_deref(),
            Some("Claude plan 5 h 12%, 7 d 3%")
        );
        harness.crew.as_ref().unwrap().shutdown().await;
        managers.shutdown().await;
    }

    /// The workers can run on another provider than the main agent: here
    /// the main agent is on the OpenAI mock and its worker in Claude Code,
    /// with Arko's tools only, and the worker's report reaches the main
    /// agent.
    #[tokio::test]
    async fn a_worker_in_claude_code_reports_to_a_main_agent_elsewhere() {
        use crate::mock_model::{ModelServer, complete, tool_call};
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            Box::pin(async move {
                if body.contains("There are 3 files.") {
                    complete("m3", "Final: 3 files.")
                } else if body.contains("Started r1") {
                    complete("m2", "r1 is counting; its report follows.")
                } else {
                    tool_call(
                        "m1",
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": "Count the files."}),
                    )
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("fake");
        std::fs::create_dir_all(&fake).unwrap();
        let script = serde_json::json!([[
            {"tool": "mcp__arko__shell", "input": {"command": "ls | wc -l"}},
            {"tool": "mcp__arko__complete_task", "input": {"summary": "There are 3 files."}},
        ]]);
        std::fs::write(fake.join("script.json"), script.to_string()).unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), false);
        let luna = harness.settings.main.clone();
        let cc = ModelSettings {
            model: "cc:claude-fable-5-1".into(),
            effort: Effort::Medium,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let managers = on_models(&mut harness, &luna, &cc, &fake_claude_code(&fake));
        assert!(harness.main_config.claude_code.is_none());
        assert!(harness.worker_config.runs_in_claude_code());

        let end = tokio::time::timeout(
            Duration::from_secs(60),
            harness.turn(Prompt::User("How many files?".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        if summary(&harness) != "Final: 3 files." {
            let end =
                tokio::time::timeout(Duration::from_secs(60), harness.finish_workers(&mut inputs))
                    .await
                    .unwrap();
            assert!(matches!(end, TurnEnd::Answered));
        }
        assert_eq!(summary(&harness), "Final: 3 files.");
        let (_, argv) = fake_log(&fake);
        assert_eq!(after(&argv, "--tools").as_deref(), Some(""), "{argv:?}");
        assert_eq!(after(&argv, "--model").as_deref(), Some("claude-fable-5-1"));
        assert_eq!(after(&argv, "--effort").as_deref(), Some("medium"));
        harness.crew.as_ref().unwrap().shutdown().await;
        managers.shutdown().await;
    }

    /// `/cost` on the ChatGPT login: nothing billed, what the calls would
    /// cost at API prices, and the plan.
    #[tokio::test]
    async fn a_chatgpt_login_turn_bills_nothing_and_shows_its_api_price() {
        use crate::mock_model::{ModelServer, complete};
        let server = ModelServer::start(Arc::new(move |_, _| {
            Box::pin(async move { complete("1", "Done.") })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (mut harness, mut inputs, _sender) =
            harness(temp.path(), server.chatgpt_client(), false);
        let end = tokio::time::timeout(
            Duration::from_secs(30),
            harness.turn(Prompt::User("Do it.".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(harness.spent(), 0.0);
        let reading = remote::Meter::read(harness.gauges.as_ref());
        let api_price = reading.api_price.expect("the calls ran on the plan");
        assert!(api_price > 0.0);
        // As /cost reads it from the ledger.
        let ledger = providers::api_price(&harness.workflow().cost_ledger()).unwrap();
        assert!((api_price - ledger).abs() < 1e-9, "{api_price} {ledger}");
        assert_eq!(reading.costs.main.calls, 1);
        assert_eq!(reading.costs.main.billed, 0.0);
        assert!((reading.costs.main.list_price - api_price).abs() < 1e-9);
        assert_eq!(reading.costs.workers, remote::CostPart::default());
        let lines = providers::cost_report(
            &harness.workflow().cost_ledger(),
            &harness.settings,
            &Default::default(),
        );
        assert_eq!(
            lines[0].text,
            "$0.00 billed in this session · 0k input and 0k output tokens"
        );
        assert_eq!(
            lines[1].text,
            format!("≈ ${api_price:.2} at API prices: the ChatGPT plan covers it")
        );
        harness.crew.as_ref().unwrap().shutdown().await;
    }

    /// `/cost` in a mixed session: the main agent in Claude Code, on the
    /// plan, and its worker on an API key, each part with its own figure.
    #[tokio::test]
    async fn a_mixed_session_s_cost_shows_each_part() {
        use crate::mock_model::{ModelServer, complete};
        // The worker answers once the lead's first turn has ended, so its
        // report starts a turn of its own instead of coming as a note in
        // that one (which a busy machine could make happen).
        let first_turn_over = Arc::new(tokio::sync::Semaphore::new(0));
        let server = ModelServer::start(Arc::new({
            let first_turn_over = first_turn_over.clone();
            move |_, _| {
                let first_turn_over = first_turn_over.clone();
                Box::pin(async move {
                    let _over = first_turn_over.acquire().await;
                    complete("w", "There are 3 files.")
                })
            }
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("fake");
        std::fs::create_dir_all(&fake).unwrap();
        let script = serde_json::json!([
            [
                {"tool": "mcp__arko__start_agent", "input": {"agent": WORKER, "task": "Count the files."}, "input_tokens": 2000},
                {"tool": "mcp__arko__complete_task", "input": {"summary": "r1 is counting."}},
            ],
            [{"tool": "mcp__arko__complete_task", "input": {"summary": "Final: 3 files."}}],
        ]);
        std::fs::write(fake.join("script.json"), script.to_string()).unwrap();
        let (mut harness, mut inputs, _sender) = harness(temp.path(), server.client(), false);
        let luna = harness.settings.main.clone();
        let cc = ModelSettings {
            model: "cc:claude-opus-5-5".into(),
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let managers = on_models(&mut harness, &cc, &luna, &fake_claude_code(&fake));
        let end = tokio::time::timeout(
            Duration::from_secs(60),
            harness.turn(Prompt::User("How many files?".into()), &mut inputs),
        )
        .await
        .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        first_turn_over.add_permits(1);
        let end =
            tokio::time::timeout(Duration::from_secs(60), harness.finish_workers(&mut inputs))
                .await
                .unwrap();
        assert!(matches!(end, TurnEnd::Answered));
        assert_eq!(summary(&harness), "Final: 3 files.");
        let lines = providers::cost_report(
            &harness.workflow().cost_ledger(),
            &harness.settings,
            &Default::default(),
        );
        let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(texts[0].contains("billed in this session"), "{texts:?}");
        assert!(
            texts[1].starts_with("main agent: ≈ $")
                && texts[1].contains("at API prices, on the Claude plan"),
            "{texts:?}"
        );
        assert!(
            texts[2].starts_with("workers: $") && texts[2].contains("billed per token"),
            "{texts:?}"
        );
        assert!(lines[1].detail && lines[2].detail);
        let reading = remote::Meter::read(harness.gauges.as_ref());
        assert!(reading.api_price.unwrap() > reading.cost && reading.cost > 0.0);
        harness.crew.as_ref().unwrap().shutdown().await;
        managers.shutdown().await;
    }
}

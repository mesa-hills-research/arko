//! A session as a front end drives it: [`Engine::open`] opens (or creates)
//! it and gives the front end its [`Seam`], what it shows the session with
//! and sends the user's input through; [`Engine::run`] runs it until the
//! user quits, and [`Engine::shutdown`] stops what it started. The command
//! line (`cli`) is one such front end: the full-screen view or the line
//! prompt, or -p's one message ([`Engine::run_prompt`]).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::agents::{AgentSettings, MAIN};
use crate::credentials::{Need, VOYAGE_KEY};
use crate::crew::Crew;
use crate::harness::{Conversation, Harness, Managers, Prompt, TurnEnd};
use crate::inputs::{Asker, Inputs};
use crate::options::{Options, SessionChoice};
use crate::prompter::Prompter;
use crate::remote::Level;
use crate::session::{Holder, LockError, Session, SessionLock};
use crate::shell::{Shell, WindowsShell};
use crate::start::{
    background_after_for, client_asking, default_model, new_settings, project_from_options,
    workspace,
};
use crate::status::Gauges;
use crate::ui::{Input, Output, Stats};
use crate::{
    agents, credentials, crew, handover, index, keys, mcp, outside, project, providers, remote,
    session, skills,
};

/// Why a session didn't open.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpenError {
    /// The session is open in another Arko (this process's too): who, when
    /// it said so (an Arko from before sessions said doesn't).
    Locked { holder: Option<Holder> },
    /// What went wrong, as the user reads it: credentials missing, the
    /// user cancelled, a session that isn't there.
    Failed(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Locked { holder } => write!(
                f,
                "the session is open in {}. Close it there first",
                holder
                    .as_ref()
                    .map_or("another Arko".to_string(), Holder::describe)
            ),
            OpenError::Failed(message) => f.write_str(message),
        }
    }
}

impl OpenError {
    /// `session` couldn't be locked.
    fn from_lock(error: LockError) -> Self {
        match error {
            LockError::Held(holder) => OpenError::Locked { holder },
            LockError::Io(error) => {
                OpenError::Failed(format!("could not lock the session: {error}"))
            }
        }
    }
}

impl std::error::Error for OpenError {}

impl From<String> for OpenError {
    fn from(message: String) -> Self {
        OpenError::Failed(message)
    }
}

/// Which session is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    /// Its folder.
    pub dir: PathBuf,
    /// Arko's folder.
    pub root: PathBuf,
    /// Where its agents work.
    pub workspace: PathBuf,
    /// It was created as it opened (else resumed).
    pub new: bool,
}

/// What a front end shows an open session with, and sends the user's input
/// through.
pub struct Seam {
    pub session: SessionInfo,
    /// What happens in the session, as items, for the front end to show.
    pub feed: remote::Feed,
    /// The session's figures (cost, context, settings, workers), read on
    /// demand.
    pub meter: Arc<dyn remote::Meter>,
    /// The same figures, sent as they change.
    pub status: tokio::sync::watch::Receiver<Arc<remote::Reading>>,
    /// What `/cost`, `/session`, `/mcp`, `/skills` and `/workers` say, as
    /// data.
    pub details: Arc<crate::details::Details>,
    /// The user's input.
    pub input: mpsc::UnboundedSender<Input>,
    /// The API keys, which the front end can add and remove.
    pub keys: Arc<keys::Keys>,
    /// The background jobs quitting would stop, to ask the user about
    /// (when the front end asks before quitting: [`FrontEnd::asks_to_quit`]).
    pub quit_asks: mpsc::UnboundedReceiver<Vec<arko_agents::terminal::BackgroundJob>>,
    /// The session's facts for a banner.
    pub banner: crate::ui::Banner,
    /// The main agent's handover size as the session started.
    pub handover_at: usize,
    /// The agents' thinking shows whole from the start.
    pub show_thinking: bool,
}

/// How the front end shows the session, which changes what Arko prints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrontEnd {
    /// The front end draws the banner itself, boxed (the full-screen view):
    /// Arko prints only its last lines.
    pub boxed_banner: bool,
    /// Quitting with background jobs running asks the front end
    /// ([`Seam::quit_asks`]), which answers with `Input::Quit`; else the
    /// line prompt's warning.
    pub asks_to_quit: bool,
}

/// How [`Engine::run`] or [`Engine::run_prompt`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    /// The message was answered (-p).
    Answered,
    /// The turn was stopped, or the user quit during it (-p).
    Stopped,
    /// The turn failed (-p).
    Failed,
    /// The user quit.
    Quit,
}

/// A session running on threads of its own: see [`spawn`].
pub struct Running {
    /// The session opened (its seam, to show it and send it input), or why
    /// it didn't.
    pub opened: oneshot::Receiver<Result<Seam, OpenError>>,
    /// How it ended, once it has shut down and its threads are gone. (It
    /// fails when the session didn't open.)
    pub ended: oneshot::Receiver<RunEnd>,
}

/// Opens the session `options` choose and runs it until the user quits
/// (`Input::Quit`), on a thread and a runtime of its own: several sessions
/// run side by side in one process, each with its own settings (its
/// folder's `.env`) and its log lines tagged with its ID
/// ([`ProcessLog`](crate::logging::ProcessLog)), and nothing it started
/// outlives it. The front end is an app's: it draws the banner itself, asks
/// before quitting with jobs running ([`Seam::quit_asks`]), and shows
/// Arko's lines from the feed.
pub fn spawn(options: Options, prompter: Arc<dyn Prompter>) -> Running {
    static SESSIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let number = SESSIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (opened_sender, opened) = oneshot::channel();
    let (ended_sender, ended) = oneshot::channel();
    let context = Arc::new(crate::context::SessionContext::default());
    let name = format!("arko-s{number}");
    let thread = std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            crate::context::enter(context.clone());
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name(name)
                .on_thread_start(move || crate::context::enter(context.clone()))
                .enable_all()
                .build();
            let runtime = match runtime {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = opened_sender.send(Err(OpenError::Failed(error.to_string())));
                    return;
                }
            };
            let end = runtime.block_on(async move {
                let (mut engine, seam) = match Engine::open(options, prompter).await {
                    Ok(open) => open,
                    Err(error) => {
                        let _ = opened_sender.send(Err(error));
                        return None;
                    }
                };
                engine.output().to_feed(seam.feed.clone());
                engine.front_end(FrontEnd {
                    boxed_banner: true,
                    asks_to_quit: true,
                });
                let end = if opened_sender.send(Ok(seam)).is_ok() {
                    engine.run().await
                } else {
                    // No one to show it to.
                    RunEnd::Quit
                };
                engine.shutdown().await;
                Some(end)
            });
            // Whatever the session left running stops here.
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            if let Some(end) = end {
                let _ = ended_sender.send(end);
            }
        });
    if let Err(error) = thread {
        log::error!("could not start a session's thread: {error}");
    }
    Running { opened, ended }
}

/// An open session.
pub struct Engine {
    /// Held while the session is open; released after shutdown.
    _lock: SessionLock,
    /// Sends the status as it changes.
    publisher: tokio::task::JoinHandle<()>,
    harness: Harness,
    inputs: Inputs,
    managers: Managers,
    remote_bind: Option<remote::Bind>,
    new: bool,
    boxed_banner: bool,
    quit_popup: Option<mpsc::UnboundedSender<Vec<arko_agents::terminal::BackgroundJob>>>,
}

impl Engine {
    /// Opens the session `options` choose, or creates it: its settings, the
    /// credentials it needs (asked for through `prompter`), its agents, MCP
    /// servers (approved through `prompter`), skills and search indexes,
    /// the feed, and the workflow, resumed where it was.
    pub async fn open(
        options: Options,
        prompter: Arc<dyn Prompter>,
    ) -> Result<(Engine, Seam), OpenError> {
        let root = match &options.root {
            Some(root) => root.clone(),
            None => session::data_root()?,
        };
        let remote_bind = options.remote;
        // Why the session doesn't run the last session's models, as Arko said
        // before the session started: the full-screen view opens over it, so
        // its banner says it again.
        let mut model_note = None;

        // The session: resumed with the settings it was started with, or new.
        let resumed = match &options.session {
            SessionChoice::Resume(id) => Some(Session::open(&root, id)?),
            SessionChoice::ContinueLatest => {
                let workspace = workspace(&options)?;
                Some(session::latest_in(&root, &workspace).ok_or_else(|| {
                    format!(
                        "no session to continue in {}. `arko --sessions` lists them",
                        workspace.display()
                    )
                })?)
            }
            SessionChoice::New => None,
        };
        // A session open elsewhere is left alone.
        let mut lock = resumed
            .as_ref()
            .map(|session| session.lock(options.app))
            .transpose()
            .map_err(OpenError::from_lock)?;
        if let Some(session) = &resumed {
            crate::context::name_session(&session.id);
        }
        let (session, settings, new) = match resumed {
            Some(session) => {
                let mut settings = session.load_settings()?;
                // Its settings and keys, as its own folder has them.
                credentials::use_env(credentials::Env::for_folder(
                    Some(&settings.workspace),
                    true,
                ));
                if options.model.is_some()
                    || options.worker_model.is_some()
                    || options.fast
                    || options.cache_1h.is_some()
                    || options.shell.is_some()
                    || options.openai_auth.is_some()
                {
                    prompter.note(
                        Level::Warn,
                        &format!(
                            "a resumed session keeps the models and shell it was started with ({})",
                            settings.describe(&settings.main)
                        ),
                    );
                }
                if options.text_replies.is_some() {
                    prompter.note(
                        Level::Warn,
                        &format!(
                            "a resumed session keeps how its main agent ends its turns: {}",
                            agents::replies_text(settings.text_replies)
                        ),
                    );
                }
                if options.cc_compact.is_some() {
                    prompter.note(
                        Level::Warn,
                        &format!(
                            "a resumed session keeps what its agents in Claude Code do at the handover size: {}",
                            agents::cc_text(!settings.cc_handover)
                        ),
                    );
                }
                if options.extra_rules.is_some() {
                    prompter.note(
                        Level::Warn,
                        &format!(
                            "a resumed session keeps its extra rules for small models: {}",
                            settings.extra_rules.describe()
                        ),
                    );
                }
                if !settings.workspace.is_dir() {
                    return Err(format!(
                        "the session's working directory {} no longer exists",
                        settings.workspace.display()
                    )
                    .into());
                }
                // The project's instructions can change at any time, and so can
                // when commands move to the background.
                let background = options.background_after.is_some_and(|after| {
                    std::mem::replace(&mut settings.background_after, after) != after
                });
                if project_from_options(&options, &mut settings) | background {
                    session
                        .save_settings(&settings)
                        .map_err(|error| format!("could not save the session: {error}"))?;
                }
                (Some(session), settings, false)
            }
            None => {
                credentials::use_env(credentials::Env::for_folder(
                    Some(&workspace(&options)?),
                    true,
                ));
                if options.model.is_none() && default_model(&options).is_none() {
                    prompter.need_credentials(&Need::Any).await?;
                }
                let (mut settings, note) = new_settings(&options, prompter.as_ref())?;
                model_note = note;
                let notes = providers::check_openrouter(
                    &mut settings,
                    options.handover_at.is_some(),
                    providers::openrouter_url(),
                )
                .await?;
                for note in notes {
                    prompter.note(Level::Info, &note);
                }
                (None, settings, true)
            }
        };

        // Sessions saved before the shell could be chosen ran PowerShell.
        let shell = Shell::detect(settings.windows_shell.unwrap_or(WindowsShell::Powershell));
        if new && options.shell.is_some() && !cfg!(windows) {
            prompter.note(
                Level::Warn,
                &format!(
                    "--shell picks the shell on Windows. Here commands run in {}",
                    shell.name
                ),
            );
        }

        // Missing credentials, or an auth file that fails, are asked for.
        let client = match options.given_client() {
            Some(client) => client,
            None => Arc::new(
                client_asking(prompter.as_ref(), || {
                    providers::client(options.codex_auth.as_deref(), &settings)
                })
                .await?,
            ),
        };
        let provider_setup = providers::Setup::new(settings.openai_auth);
        let agent_settings = AgentSettings::for_session(&settings, &provider_setup);
        // Settings the agents can't be built with leave no session behind.
        agents::main_config(&agent_settings)?;
        agents::worker_config(&agent_settings)?;
        // A project's MCP servers are started once the user approves them.
        let mcp_plan = mcp::plan(&root, &settings.workspace, &options.mcp_config)?;
        let mcp_answer = match &mcp_plan.approval {
            Some(approval) => prompter.approve_project_mcp(approval).await?,
            None => None,
        };
        let (mcp_servers, mcp_notes) = mcp_plan.decide(mcp_answer)?;

        let session = match session {
            Some(session) => session,
            None => {
                let session = Session::create(&root)
                    .map_err(|error| format!("could not create a session: {error}"))?;
                lock = Some(session.lock(options.app).map_err(OpenError::from_lock)?);
                crate::context::name_session(&session.id);
                session
            }
        };
        let lock = lock.expect("the session is locked");
        if new {
            session
                .save_settings(&settings)
                .map_err(|error| format!("could not save the session: {error}"))?;
            // The next session's default; see `providers::LastModels::remember`.
            if !options.one_shot
                && let Err(error) = providers::LastModels::remember(&root, &settings)
            {
                prompter.note(
                    Level::Warn,
                    &format!("could not keep this session's models for the next one: {error}"),
                );
            }
        }
        prompter.session_found(&session.dir);

        let managers = Managers::open(&session, &shell).map_err(|error| {
            // An Arko from before session locks holds only its terminal's.
            if error.contains("already in use") {
                OpenError::Locked { holder: None }
            } else {
                OpenError::Failed(error)
            }
        })?;
        // Outside access as the session left it; the flag turns it on below.
        let outside = Arc::new(outside::Outside::new(
            settings.allow_outside_workspace,
            managers.list(),
            settings.workspace.clone(),
        ));
        let one_shot = options.one_shot;
        managers
            .background
            .set_after(background_after_for(&options, settings.background_after));

        // MCP servers, skills and search indexes: what goes wrong with them is
        // said, and the session goes on without.
        let startup = Output::new(None, one_shot);
        for note in &mcp_notes {
            startup.line(&startup.warn(&format!("arko: {note}")));
        }
        let mcp = if mcp_servers.is_empty() {
            mcp::Running::default()
        } else {
            startup.line(&startup.dim(&format!(
                "Starting MCP servers: {}…",
                mcp_servers
                    .iter()
                    .map(|server| server.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
            mcp::Running::start(mcp_servers).await
        };
        for started in &mcp.servers {
            if let Err(error) = &started.result {
                startup.line(&startup.warn(&format!(
                    "arko: the MCP server `{}` (in {}) didn't start: {error}",
                    started.server.name,
                    started.server.file.display()
                )));
            }
        }
        let skill_folders = skills::folders(&settings.workspace, &root);
        let indexes = match index::Indexes::open(
            &root,
            &settings.workspace,
            credentials::var(VOYAGE_KEY),
            !one_shot,
        ) {
            Ok(indexes) => Some(indexes),
            Err(error) => {
                startup.line(&startup.warn(&format!("arko: search indexes are off: {error}")));
                None
            }
        };
        // What the remote page shows, kept from the start so that a page
        // opened later has the history.
        let feed = remote::Feed::new(MAIN);
        feed.hide_thinking(settings.hide_thinking);
        let keys = keys::Keys::new(
            client.clone(),
            keys::openai_way(&client).await,
            indexes.clone(),
            Some(feed.clone()),
        );
        // A key added in another session open on Arko's folder is used here too.
        keys.share(root.clone());
        let mcp_tools = mcp.tool_names();
        let mcp_started = mcp.started();
        let known_indexes = indexes
            .as_ref()
            .map(|indexes| indexes.known(&settings.workspace))
            .unwrap_or_default();
        let agent_settings = AgentSettings {
            mcp_tools: &mcp_tools,
            mcp_servers: &mcp_started,
            skill_folders: &skill_folders,
            index_tools: if indexes.is_some() {
                index::TOOL_NAMES
            } else {
                &[]
            },
            indexes: &known_indexes,
            ..agent_settings
        };
        let main_config = agents::main_config(&agent_settings)?;
        let worker_config = agents::worker_config(&agent_settings)?;
        // Where the runs' command channels are kept, to ask for handovers.
        let runs = Arc::new(handover::Runs::default());
        let mut tools = managers.tools(&shell, runs.clone());
        tools.extend(mcp.tools().iter().cloned());
        if let Some(indexes) = &indexes {
            tools.extend(indexes.tools());
        }
        let (input_sender, input) = mpsc::unbounded_channel();
        // Someone answers the agent's questions, and what they type while it
        // works reaches it as notes (else each line is a turn of its own).
        let interactive = options.interactive;
        // The front end attaches a printer, or a feed, once the session is open.
        let output = Arc::new(Output::new(None, one_shot));
        let stats = Arc::new(Mutex::new(Stats::default()));
        let (remote_actions, remote_input) = mpsc::unbounded_channel();
        let handover = Arc::new(handover::Handover::new(
            runs,
            settings.main_handover_at,
            settings.workers_handover_at,
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
            background: managers.background.clone(),
            page: Mutex::default(),
            changed: Default::default(),
            published: Default::default(),
        });
        // The main agent's questions are put to the user at the prompt; with
        // no terminal, it is told there is no one to ask.
        let (questions_sender, questions) = mpsc::unbounded_channel();
        let asker = Arc::new(Asker {
            questions: interactive.then_some(questions_sender),
        });
        // The workers, and those of a resumed session.
        let (crew, woken) = if settings.max_workers > 0 {
            let (crew, woken) = Crew::new(crew::Setup {
                session_id: session.id.clone(),
                session_dir: session.dir.clone(),
                output: output.clone(),
                max_running: settings.max_workers,
                cache_1h: settings.worker.cache_1h,
                cache_lifetime: client
                    .prompt_cache_lifetime(
                        &worker_config.llm_config.model,
                        &worker_config.llm_config.generation_config,
                    )
                    .await,
                feed: Some(feed.clone()),
                refuse: Some({
                    let keys = keys.clone();
                    Arc::new(move |model: &arko_ai::Model| keys.refusal(model))
                }),
            });
            (Some(crew), Some(woken))
        } else {
            (None, None)
        };

        let details = Arc::new(crate::details::Details::new(
            session.id.clone(),
            session.dir.clone(),
            root.clone(),
            gauges.clone(),
            indexes.clone(),
            skill_folders.clone(),
            crate::details::McpFacts::of(&mcp, &mcp_notes, &root),
        ));
        let mut harness = Harness {
            details,
            settings,
            providers: provider_setup.clone(),
            session,
            client,
            keys,
            main_config,
            worker_config,
            tools,
            root: root.clone(),
            mcp,
            skill_folders,
            indexes,
            unoffered: None,
            output: output.clone(),
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
            crew,
            asker,
            one_shot,
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
            background: managers.background.clone(),
        };
        harness.project = project::Found::read(&harness.settings.workspace);
        // Before the workflow opens: it may restart a worker that hadn't started.
        harness.give_workers_project();
        harness.open(!new)?;
        harness.start_new_workers();
        harness.publish_models();
        harness.outside_at_start(new, options.allow_outside_workspace);
        if !new && let Some(tokens) = options.handover_at {
            harness.handover_at_start(tokens);
        }
        if one_shot && let Some(note) = &harness.unoffered {
            output.line(&output.warn(&format!("arko: {note}")));
        }
        let inputs = Inputs {
            receiver: input,
            pending: None,
            questions,
            woken,
            remote: remote_input,
            // With -p, Arko exits when the agent finishes, jobs or not.
            finished: (!one_shot).then(|| managers.jobs.subscribe_finished()),
            late: None,
        };
        let (quit_popup, quit_asks) = mpsc::unbounded_channel();
        let seam = Seam {
            session: SessionInfo {
                id: harness.session.id.clone(),
                dir: harness.session.dir.clone(),
                root: root.clone(),
                workspace: harness.settings.workspace.clone(),
                new,
            },
            feed: harness.feed.clone(),
            meter: harness.gauges.clone(),
            status: harness.gauges.watch(),
            details: harness.details.clone(),
            input: input_sender,
            keys: harness.keys.clone(),
            quit_asks,
            // (With what Arko said before the front end opened.)
            banner: harness.header(model_note),
            handover_at: harness
                .settings
                .main
                .handover_at
                .unwrap_or(harness.settings.handover_at),
            show_thinking: harness.settings.show_thinking,
        };
        let publisher = harness.gauges.publish(&harness.feed);
        let engine = Engine {
            _lock: lock,
            publisher,
            harness,
            inputs,
            managers,
            remote_bind,
            new,
            boxed_banner: false,
            quit_popup: Some(quit_popup),
        };
        Ok((engine, seam))
    }

    /// Where Arko prints: the front end attaches a printer to it, or the
    /// feed it shows.
    pub(crate) fn output(&self) -> Arc<Output> {
        self.harness.output.clone()
    }

    /// Says how the front end shows the session; before [`run`](Self::run).
    pub fn front_end(&mut self, front: FrontEnd) {
        self.boxed_banner = front.boxed_banner;
        self.harness.quit_popup = if front.asks_to_quit {
            self.quit_popup.clone()
        } else {
            None
        };
    }

    /// Runs the session until the user quits: the banner, the web page if
    /// asked for, the turns a resumed session left unfinished, then the
    /// user's messages and commands.
    pub async fn run(&mut self) -> RunEnd {
        let harness = &mut self.harness;
        harness.banner(self.new, self.boxed_banner);
        if let Some(bind) = self.remote_bind {
            harness.start_remote(bind).await;
        }
        if !self.new {
            harness.restart_unfinished(&mut self.inputs).await;
        }
        if !harness.exiting {
            harness.repl(&mut self.inputs).await;
        }
        RunEnd::Quit
    }

    /// Sends `prompt` as the user's message, and waits for the answer and
    /// for the workers the agent started (their reports go to it).
    pub async fn run_prompt(&mut self, prompt: String) -> RunEnd {
        let harness = &mut self.harness;
        if !self.new {
            harness.restart_unfinished(&mut self.inputs).await;
        }
        let end = if harness.exiting {
            TurnEnd::Exit
        } else {
            harness.turn(Prompt::User(prompt), &mut self.inputs).await
        };
        let end = match end {
            TurnEnd::Answered => harness.finish_workers(&mut self.inputs).await,
            other => other,
        };
        match end {
            TurnEnd::Answered => RunEnd::Answered,
            TurnEnd::Stopped | TurnEnd::Exit => RunEnd::Stopped,
            TurnEnd::Failed => RunEnd::Failed,
        }
    }

    /// Stops what the session started: the web page (whose users are told),
    /// the workers (said, as they can be continued), the MCP servers, the
    /// index watchers and the terminal's jobs. The session is saved as it
    /// goes.
    pub async fn shutdown(self) {
        self.publisher.abort();
        let mut harness = self.harness;
        let output = harness.output.clone();
        if harness.remote.is_some() {
            harness
                .feed
                .notice(Level::Info, "Arko closed. The session is saved.");
            harness.stop_remote().await;
        }
        if let Some(crew) = &harness.crew {
            let running = crew.running().len();
            if running > 0 {
                output.line(&output.dim(&format!(
                    "Stopping {running} worker{}… (resuming the session lets the agent continue {})",
                    if running == 1 { "" } else { "s" },
                    if running == 1 { "it" } else { "them" }
                )));
            }
            crew.shutdown().await;
        }
        harness.mcp.shutdown().await;
        if let Some(indexes) = &harness.indexes {
            indexes.shutdown().await;
        }
        self.managers.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::agents::{Effort, ModelSettings, resolve_model};
    use crate::mock_model::{ModelServer, complete, reply, with_input};
    use crate::options::GivenClient;
    use crate::prompter::NoPrompter;
    use crate::providers::OpenAiAuth;
    use crate::session::Settings;
    use crate::shell::WindowsShell;

    /// The engine, its seam and its futures can go to any thread: a front
    /// end can run a session on a runtime of its own or on a shared one.
    /// (Checked as this compiles.)
    #[allow(dead_code)]
    fn engine_futures_are_send(options: Options, prompter: Arc<dyn Prompter>, mut engine: Engine) {
        fn send<T: Send>(_: T) {}
        send(Engine::open(options, prompter));
        send(engine.run());
        send(engine.run_prompt(String::new()));
        send(engine.shutdown());
        fn sendable<T: Send + 'static>() {}
        sendable::<Seam>();
    }

    /// Two sessions run side by side in one process, each on its own
    /// runtime: each reads its own folder's settings, answers its messages
    /// in its own feed, holds its own lock and tags its log lines with its
    /// ID; once closed, its threads are gone.
    #[tokio::test]
    async fn two_sessions_run_turns_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let log = temp.path().join("logs").join("arko-studio.log");
        let logging = crate::logging::ProcessLog::install(&log);
        let server = ModelServer::start(Arc::new(|_, body: String| {
            Box::pin(async move {
                if body.contains("Hello, first.") {
                    reply("a1", "First's answer.")
                } else {
                    reply("b1", "Second's answer.")
                }
            })
        }))
        .await;
        let start = |name: &str, shell: &str| {
            let workspace = temp.path().join(name);
            std::fs::create_dir_all(&workspace).unwrap();
            std::fs::write(workspace.join(".env"), format!("ARKO_SHELL={shell}\n")).unwrap();
            spawn(
                Options {
                    root: Some(root.clone()),
                    workspace: Some(workspace),
                    model: Some("luna".into()),
                    openai_auth: Some(OpenAiAuth::ApiKey),
                    cache_1h: Some(false),
                    text_replies: Some(true),
                    client: Some(GivenClient(Arc::new(server.client()))),
                    ..Options::default()
                },
                Arc::new(NoPrompter),
            )
        };
        let wait = Duration::from_secs(30);
        let (first, second) = (start("first", "powershell"), start("second", "cmd"));
        let first_seam = tokio::time::timeout(wait, first.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let second_seam = tokio::time::timeout(wait, second.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let shell_of = |id: &str| {
            Session::open(&root, id)
                .unwrap()
                .load_settings()
                .unwrap()
                .windows_shell
        };
        assert_eq!(
            shell_of(&first_seam.session.id),
            Some(WindowsShell::Powershell)
        );
        assert_eq!(shell_of(&second_seam.session.id), Some(WindowsShell::Cmd));

        // A session open in this process can't be opened again.
        let again = spawn(
            Options {
                root: Some(root.clone()),
                session: SessionChoice::Resume(first_seam.session.id.clone()),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        match tokio::time::timeout(wait, again.opened)
            .await
            .unwrap()
            .unwrap()
        {
            Err(OpenError::Locked {
                holder: Some(holder),
            }) => {
                assert_eq!(
                    (holder.app.as_str(), holder.pid),
                    ("arko-studio", std::process::id())
                );
            }
            Err(error) => panic!("{error}"),
            Ok(_) => panic!("opened twice"),
        }

        first_seam
            .input
            .send(Input::Line("Hello, first.".into()))
            .unwrap();
        second_seam
            .input
            .send(Input::Line("Hello, second.".into()))
            .unwrap();
        let texts = |seam: &Seam| -> Vec<String> {
            seam.feed
                .subscribe()
                .0
                .items
                .into_iter()
                .map(|item| item.text)
                .collect()
        };
        tokio::time::timeout(wait, async {
            loop {
                let (one, two) = (texts(&first_seam), texts(&second_seam));
                if one.iter().any(|text| text.contains("First's answer."))
                    && two.iter().any(|text| text.contains("Second's answer."))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !texts(&first_seam)
                .iter()
                .any(|text| text.contains("Second's"))
        );
        assert!(
            !texts(&second_seam)
                .iter()
                .any(|text| text.contains("First's"))
        );

        for seam in [&first_seam, &second_seam] {
            seam.input.send(Input::Quit(Some(Vec::new()))).unwrap();
        }
        assert_eq!(
            tokio::time::timeout(wait, first.ended)
                .await
                .unwrap()
                .unwrap(),
            RunEnd::Quit
        );
        assert_eq!(
            tokio::time::timeout(wait, second.ended)
                .await
                .unwrap()
                .unwrap(),
            RunEnd::Quit
        );
        // The sessions' locks are free again.
        assert_eq!(crate::session::holder(&first_seam.session.dir), None);

        #[cfg(target_os = "linux")]
        {
            let session_threads = || {
                std::fs::read_dir("/proc/self/task")
                    .unwrap()
                    .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
                    .filter(|name| name.starts_with("arko-s"))
                    .count()
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                while session_threads() > 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the sessions' threads end");
        }

        // The process's log tags each session's lines. (When another test
        // installed a logger first, there is nothing to read here.)
        if logging.is_ok() {
            log::logger().flush();
            let text = std::fs::read_to_string(&log).unwrap();
            for id in [&first_seam.session.id, &second_seam.session.id] {
                assert!(text.contains(&format!("] [{id}] ")), "{id}: {text}");
            }
        }
    }

    /// The status goes out when it changes (a request reports its usage),
    /// once per change: not again while the session is idle.
    #[tokio::test]
    async fn the_status_changes_when_a_request_reports_usage() {
        let temp = tempfile::tempdir().unwrap();
        let server = ModelServer::start(Arc::new(|_, _| {
            Box::pin(async { with_input(complete("a1", "Done."), 12_345) })
        }))
        .await;
        let root = temp.path().join("arko");
        let workspace = temp.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let luna = ModelSettings {
            model: resolve_model("luna").unwrap().id,
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let session = Session::create(&root).unwrap();
        session
            .save_settings(&Settings::for_test(&workspace, &luna))
            .unwrap();
        let options = Options {
            root: Some(root),
            session: SessionChoice::Resume(session.id.clone()),
            one_shot: true,
            interactive: false,
            client: Some(GivenClient(Arc::new(server.client()))),
            ..Options::default()
        };
        let (mut engine, mut seam) = Engine::open(options, Arc::new(NoPrompter)).await.unwrap();
        assert_eq!(seam.status.borrow().context_tokens, None);
        engine.front_end(FrontEnd::default());
        assert_eq!(engine.run_prompt("Do it.".into()).await, RunEnd::Answered);
        tokio::time::timeout(Duration::from_secs(10), async {
            while seam.status.borrow_and_update().context_tokens != Some(12_345) {
                seam.status.changed().await.unwrap();
            }
        })
        .await
        .expect("the status with the request's input");
        // Settled, it isn't sent again while nothing changes.
        tokio::time::sleep(Duration::from_secs(1)).await;
        seam.status.mark_unchanged();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!seam.status.has_changed().unwrap(), "nothing changed");
        engine.shutdown().await;
    }

    /// A session open in another Arko isn't opened: the front end is told
    /// who has it.
    #[tokio::test]
    async fn a_session_open_elsewhere_isnt_opened() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let workspace = temp.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let luna = ModelSettings {
            model: resolve_model("luna").unwrap().id,
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let session = Session::create(&root).unwrap();
        session
            .save_settings(&Settings::for_test(&workspace, &luna))
            .unwrap();
        let _held = session.lock("arko").unwrap();
        let options = Options {
            root: Some(root.clone()),
            session: SessionChoice::Resume(session.id.clone()),
            interactive: false,
            ..Options::default()
        };
        match Engine::open(options, Arc::new(NoPrompter)).await {
            Err(OpenError::Locked {
                holder: Some(holder),
            }) => {
                assert_eq!(holder.app, "arko");
                assert_eq!(holder.pid, std::process::id());
            }
            Err(error) => panic!("{error}"),
            Ok(_) => panic!("opened"),
        }
    }

    /// A session opens, answers a message and shuts down with no terminal:
    /// what it does reaches the front end through the feed, and the session
    /// is saved.
    #[tokio::test]
    async fn an_engine_answers_a_turn_without_a_terminal() {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body);
                Box::pin(async { complete("a1", "Hello from the engine.") })
            }
        }))
        .await;
        let root = temp.path().join("arko");
        let workspace = temp.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let luna = ModelSettings {
            model: resolve_model("luna").unwrap().id,
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let session = Session::create(&root).unwrap();
        session
            .save_settings(&Settings::for_test(&workspace, &luna))
            .unwrap();
        let options = Options {
            root: Some(root.clone()),
            session: SessionChoice::Resume(session.id.clone()),
            one_shot: true,
            interactive: false,
            client: Some(GivenClient(Arc::new(server.client()))),
            ..Options::default()
        };
        let (mut engine, seam) = Engine::open(options, Arc::new(NoPrompter)).await.unwrap();
        assert_eq!(seam.session.id, session.id);
        assert_eq!(seam.session.workspace, workspace);
        assert!(!seam.session.new);
        engine.front_end(FrontEnd::default());
        let end = tokio::time::timeout(
            Duration::from_secs(30),
            engine.run_prompt("Say hello.".into()),
        )
        .await
        .unwrap();
        assert_eq!(end, RunEnd::Answered);
        assert!(bodies.lock().unwrap()[0].contains("Say hello."));
        let (snapshot, _) = seam.feed.subscribe();
        let texts: Vec<&str> = snapshot
            .items
            .iter()
            .map(|item| item.text.as_str())
            .collect();
        assert!(
            texts
                .iter()
                .any(|text| text.contains("Hello from the engine.")),
            "{texts:?}"
        );
        engine.shutdown().await;
        assert!(
            Session::open(&root, &session.id)
                .unwrap()
                .latest_checkpoint()
                .is_some()
        );
    }
}

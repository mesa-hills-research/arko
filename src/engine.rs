//! A session as a front end drives it: [`Engine::open`] opens (or creates)
//! it and gives the front end its [`Seam`], what it shows the session with
//! and sends the user's input through. [`Engine::run`] runs it until the
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
    /// The session is open in another Arko, which may be in this process.
    /// `holder` says who has it, when that Arko said so (older versions
    /// don't).
    Locked { holder: Option<Holder> },
    /// What went wrong, as the user reads it, such as missing credentials,
    /// the user cancelling or a session that isn't there.
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
                OpenError::Failed(format!("could not lock the session ({error})"))
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
    /// The images the items show (`Item::images`), by ID.
    pub images: crate::images::Images,
    /// The session's figures (cost, context, settings, workers), read on
    /// demand.
    pub meter: Arc<dyn remote::Meter>,
    /// The same figures, sent as they change.
    pub status: tokio::sync::watch::Receiver<Arc<remote::Reading>>,
    /// What `/cost`, `/session`, `/mcp`, `/skills` and `/workers` say, and
    /// the search indexes, as data.
    pub details: Arc<crate::details::Details>,
    /// The user's input.
    pub input: mpsc::UnboundedSender<Input>,
    /// What the front end asks of the session, answered as it can.
    pub requests: mpsc::UnboundedSender<Request>,
    /// The API keys, which the front end can add and remove.
    pub keys: Arc<keys::Keys>,
    /// The background jobs quitting would stop, to ask the user about when
    /// the front end asks before quitting ([`FrontEnd::asks_to_quit`]).
    pub quit_asks: mpsc::UnboundedReceiver<crate::ui::QuitAsk>,
    /// The session's facts for a banner.
    pub banner: crate::ui::Banner,
    /// The main agent's handover size as the session started.
    pub handover_at: usize,
    /// The agents' thinking shows whole from the start.
    pub show_thinking: bool,
}

/// What a front end asks of an open session, with where the answer goes.
/// The answer is `Err` with the sentence to show when it can't be done.
#[derive(Debug)]
#[non_exhaustive]
pub enum Request {
    /// Gives the session this name, also during a turn. An empty name gives
    /// it back its conversation's.
    Rename { title: String, reply: Reply<()> },
    /// Moves the running call `item` (a `Kind::Call` item that
    /// `Reading::movable` lists) to the background, as Ctrl+B does for the
    /// agent shown. The call is answered with its job's ID, the agent goes
    /// on, and the call's result reaches it as a note. `Err` says why it
    /// can't move now (it ended, or it isn't one that can move).
    MoveCall { item: u64, reply: Reply<()> },
    /// Stops the background job `job` (as `JobSummary::id` names it) for the
    /// user. Its agent hears of it with its next turn, and an idle agent
    /// isn't woken. `Err` when it isn't running.
    StopJob { job: uuid::Uuid, reply: Reply<()> },
    /// A chat's instructions from the user (`None` for none), for its next
    /// conversations. They apply from the chat's first message, or else
    /// from its next new conversation, as the one it is in keeps its own. A
    /// new chat takes them in `Options::chat_instructions`.
    ChatInstructions {
        text: Option<String>,
        reply: Reply<()>,
    },
    /// Makes the session a chat or an agent's from its next message, and
    /// the conversation goes on in the new mode. `workspace` is the folder
    /// an agent works in (`None` for the one it worked in before, which a
    /// chat that never was an agent's doesn't have). It's refused while
    /// workers run, while their news or what the user typed hasn't reached
    /// the agent, and (to Chat) while background jobs run. The session then
    /// closes, and [`Engine::run`] ends with [`RunEnd::Reopen`], for the
    /// front end to open it again. Its web page stops with it, and
    /// `Options::remote` serves it again.
    SwitchMode {
        to: crate::session::Mode,
        workspace: Option<PathBuf>,
        reply: Reply<()>,
    },
    /// Edits the user's message `item` (a `Kind::User` item with a
    /// `message_index`). The conversation is cut back to just before it,
    /// and `message` goes in its place, starting a turn (the feed drops the
    /// items from `item` on). Between turns, with no worker running, for
    /// a message the conversation still has (not one before a handover).
    /// Changes the agent made to files stay.
    Edit {
        item: u64,
        message: crate::ui::Message,
        reply: Reply<()>,
    },
    /// Sends the user's latest message again for a new reply, as an edit of
    /// it with its own words.
    Regenerate { reply: Reply<()> },
    /// A new session with the current conversation up to the user's
    /// message `before` (a `Kind::User` item with a `message_index`, the
    /// message itself left out), or all of it (`None`), to go on apart
    /// from this one. Replies the new session's ID, which
    /// [`SessionChoice::Resume`] opens to go on there. Its settings are this
    /// one's. Before the conversation's first message, the branch has none
    /// of it, but goes on from the report the conversation began from, if
    /// it was handed over. Workers stay with this session. Between turns.
    /// A closed session is branched with [`crate::sessions::branch`].
    Branch {
        before: Option<u64>,
        reply: Reply<String>,
    },
    /// Serves the session's web page on `bind` (an address, a port, or
    /// both, as `--remote` takes them), in place of the one served, and
    /// replies its links, with the session's key, the local network's
    /// first. `None` serves it to the network, whatever it was served on
    /// before. It listens on every address of this computer (0.0.0.0), on
    /// the port the page had if it was served before and the port is free,
    /// else 8765, else any free one. Any device on the computer's networks
    /// that has a link can then open the page, and it's served without TLS,
    /// so what it shows and what is typed in it cross the network
    /// unencrypted. Say so where the page is shared, and offer `127.0.0.1`
    /// for this computer only (Arko in a terminal serves there by default).
    /// A blank address is taken as `None`. `Err` says why it isn't served
    /// (an address that isn't this computer's, a port in use), and the page
    /// served before goes on as it was.
    ShareWebPage {
        bind: Option<String>,
        reply: Reply<remote::Page>,
    },
    /// Stops the session's web page, so its links and the sessions of its
    /// clients no longer work. Its clients are told it was done in the app.
    StopWebPage { reply: Reply<()> },
    /// Gives the session's web page a new key, so that the old links no
    /// longer work. A page that is served goes on with the new key on its
    /// port, and the reply has its new links (`None` when it isn't served).
    NewWebPageKey { reply: Reply<Option<remote::Page>> },
    /// Keeps the main agent's prompt cache warm between turns. Where the
    /// provider allows, it does so out of band (nothing joins the
    /// conversation), else with a short message and its reply in the
    /// conversation (the user's item has `Delivery::Refresh`).
    RefreshCache { reply: Reply<Refreshed> },
    /// The current conversation as the session saved it, for an export, as
    /// [`crate::sessions::conversation`] reads it. It's read in turn with
    /// what changes the conversation, once nothing can still add to it. No
    /// turn runs, nothing the user typed or the workers sent waits to reach
    /// the agent, and no input waits to be read. Otherwise `Err` says when
    /// to export instead, so an export never leaves out what the
    /// transcript shows. The errors of reading what was saved come back as
    /// they are.
    ExportConversation {
        reply: Reply<crate::sessions::SavedConversation>,
    },
}

/// How the prompt cache was kept warm ([`Request::RefreshCache`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Refreshed {
    /// Out of band, by a request that read `read` tokens from the cache and
    /// wrote `written`, for `cost` dollars.
    OutOfBand {
        read: usize,
        written: usize,
        cost: f64,
    },
    /// With a message and its reply, which follow in the feed.
    Message,
}

/// Where a request's answer goes.
pub type Reply<T> = oneshot::Sender<Result<T, String>>;

/// How the front end shows the session, which changes what Arko prints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrontEnd {
    /// The front end draws the banner itself in a box, as the full-screen
    /// view does, and Arko prints only its last lines.
    pub boxed_banner: bool,
    /// Quitting with background jobs running asks the front end
    /// ([`Seam::quit_asks`]), which answers with `Input::Quit`. Else the
    /// line prompt warns.
    pub asks_to_quit: bool,
    /// It asks with workers running too, not only with jobs (an app, where
    /// the full-screen view warns about them in a line).
    pub asks_about_workers: bool,
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
    /// The session switched between Chat and Agent ([`Request::SwitchMode`])
    /// and closed. Open it again ([`SessionChoice::Resume`]) to go on in its
    /// new mode, with `Options::remote` for its web page.
    Reopen,
}

/// A session running on threads of its own, as [`spawn`] starts it.
pub struct Running {
    /// The session opened (its seam, to show it and send it input), or why
    /// it didn't.
    pub opened: oneshot::Receiver<Result<Seam, OpenError>>,
    /// How it ended, once it has shut down. By then its lock is released and
    /// its runtime stopped, and its threads end within 2 s (a thread still
    /// blocked then is left to end on its own). It fails when the session
    /// didn't open.
    pub ended: oneshot::Receiver<RunEnd>,
}

/// Opens the session `options` choose and runs it until the user quits
/// (`Input::Quit`), on a thread and a runtime of its own. Several sessions
/// run side by side in one process this way, each with its own settings
/// (its folder's `.env`) and its log lines tagged with its ID
/// ([`ProcessLog`](crate::logging::ProcessLog)), and nothing it started
/// outlives it. The front end is taken to be an app's, which draws the
/// banner itself, asks before quitting with jobs running
/// ([`Seam::quit_asks`]), and shows Arko's lines from the feed.
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
                    asks_about_workers: true,
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

/// Whether a session is open in this process without a context of its own
/// (opened with [`Engine::open`], not [`spawn`]): it reads the process's
/// settings and keys, so it is the only one.
static ALONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The session open without a context of its own: another can open once
/// it is dropped.
struct Alone;

impl Drop for Alone {
    fn drop(&mut self) {
        ALONE.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// An open session.
pub struct Engine {
    /// Held while the session is open, and released after shutdown.
    _lock: SessionLock,
    /// The process's one session without a context of its own, if it is.
    _alone: Option<Alone>,
    /// Sends the status as it changes.
    publisher: tokio::task::JoinHandle<()>,
    /// Keeps the feed's background jobs in step with the terminal's.
    jobs: tokio::task::JoinHandle<()>,
    harness: Harness,
    inputs: Inputs,
    managers: Managers,
    remote_bind: Option<remote::Bind>,
    remote_port_fallback: bool,
    new: bool,
    boxed_banner: bool,
    quit_popup: Option<mpsc::UnboundedSender<crate::ui::QuitAsk>>,
}

impl Engine {
    /// Opens the session `options` choose, or creates it. That sets up its
    /// settings, the credentials it needs (asked for through `prompter`),
    /// its agents, MCP servers (approved through `prompter`), skills and
    /// search indexes, the feed, and the workflow, resumed where it was.
    ///
    /// A process opens one session this way, on the front end's own
    /// runtime. The session reads the process's settings and keys, and its
    /// log lines carry no session's ID. A second session opened this way is
    /// refused. Several sessions in one process each run with [`spawn`],
    /// which gives each its own settings, keys and log tag.
    pub async fn open(
        options: Options,
        prompter: Arc<dyn Prompter>,
    ) -> Result<(Engine, Seam), OpenError> {
        let alone = match crate::context::handle() {
            Some(_) => None,
            None if ALONE.swap(true, std::sync::atomic::Ordering::SeqCst) => {
                return Err(OpenError::Failed(
                    "A session is open in this process without arko::spawn. Open each with arko::spawn, which gives it its own settings and keys.".into(),
                ));
            }
            None => Some(Alone),
        };
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
            .map(|session| session.lock(options.app, options.app_name))
            .transpose()
            .map_err(OpenError::from_lock)?;
        if let Some(session) = &resumed {
            crate::context::name_session(&session.id);
        }
        let (session, mut settings, new) = match resumed {
            Some(session) => {
                let mut settings = session.load_settings()?;
                // Its settings and keys, as its own folder has them.
                credentials::use_env(credentials::Env::for_folder_in(
                    Some(&settings.workspace),
                    true,
                    Some(&root),
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
                            "a resumed session keeps how its main agent ends its turns ({})",
                            agents::replies_text(settings.text_replies)
                        ),
                    );
                }
                if options.cc_compact.is_some() {
                    prompter.note(
                        Level::Warn,
                        &format!(
                            "a resumed session keeps what its agents in Claude Code do at the handover size ({})",
                            agents::cc_text(!settings.cc_handover)
                        ),
                    );
                }
                if options.extra_rules.is_some() {
                    prompter.note(
                        Level::Warn,
                        &format!(
                            "a resumed session keeps its extra rules for small models ({})",
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
                // when commands move to the background and how long monitors'
                // warnings wake the agent.
                let background = options.background_after.is_some_and(|after| {
                    std::mem::replace(&mut settings.background_after, after) != after
                });
                let wakes = options.monitor_wakes.is_some_and(|wakes| {
                    std::mem::replace(&mut settings.monitor_wakes, wakes) != wakes
                });
                if project_from_options(&options, &mut settings) | background | wakes {
                    session
                        .save_settings(&settings)
                        .map_err(|error| format!("could not save the session ({error})"))?;
                }
                (Some(session), settings, false)
            }
            None => {
                credentials::use_env(credentials::Env::for_folder_in(
                    Some(&workspace(&options)?),
                    true,
                    Some(&root),
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
        {
            // Settings the agents can't be built with leave no session behind.
            let agent_settings = AgentSettings::for_session(&settings, &provider_setup);
            agents::main_config(&agent_settings)?;
            agents::worker_config(&agent_settings)?;
        }
        // A chat has no tools: no MCP servers, skills, indexes or workers. One
        // that was an agent's has them still (its agent is offered none), for
        // its agent's conversations, which resume with their tools.
        let chat = settings.mode == crate::session::Mode::Chat;
        let bare = chat && !settings.switched_mode;
        // A project's MCP servers are started once the user approves them.
        let (mcp_servers, mcp_notes) = if bare {
            (Vec::new(), Vec::new())
        } else {
            let mcp_plan = mcp::plan(
                &root,
                &settings.workspace,
                &options.mcp_config,
                options.mcp_config_required,
            )?;
            let mcp_answer = match &mcp_plan.approval {
                Some(approval) => prompter.approve_project_mcp(approval).await?,
                None => None,
            };
            mcp_plan.decide(mcp_answer)?
        };

        let session = match session {
            Some(session) => session,
            None => {
                let session = Session::create(&root)
                    .map_err(|error| format!("could not create a session ({error})"))?;
                lock = Some(
                    session
                        .lock(options.app, options.app_name)
                        .map_err(OpenError::from_lock)?,
                );
                crate::context::name_session(&session.id);
                session
            }
        };
        let lock = lock.expect("the session is locked");
        if new && chat {
            // A chat works in no folder of the user's: its own, which `arko
            // -c` never takes for a project's.
            settings.workspace = session.dir.clone();
        }
        if new {
            session
                .save_settings(&settings)
                .map_err(|error| format!("could not save the session ({error})"))?;
            // The next session's default; see `providers::LastModels::remember`.
            if !options.one_shot
                && let Err(error) = providers::LastModels::remember(&root, &settings)
            {
                prompter.note(
                    Level::Warn,
                    &format!("could not keep this session's models for the next one ({error})"),
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
        managers
            .jobs
            .set_warning_wakes_within(settings.monitor_wakes.within());

        // MCP servers, skills and search indexes: what goes wrong with them is
        // said, and the session goes on without.
        let startup = Output::new(None, one_shot);
        for note in &mcp_notes {
            startup.line(&startup.warn(&crate::ui::sentence(note)));
        }
        let mcp = if mcp_servers.is_empty() {
            mcp::Running::default()
        } else {
            startup.line(&startup.dim(&format!(
                "Starting MCP servers {}…",
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
                    "The MCP server `{}` (in {}) didn't start ({error}).",
                    started.server.name,
                    started.server.file.display()
                )));
            }
        }
        let skill_folders = if bare {
            Vec::new()
        } else {
            skills::folders(&settings.workspace, &root)
        };
        let indexes = if bare {
            Ok(None)
        } else {
            index::Indexes::open(
                &root,
                &settings.workspace,
                credentials::var(VOYAGE_KEY),
                !one_shot,
            )
            .map(Some)
        };
        let indexes = match indexes {
            Ok(indexes) => indexes,
            Err(error) => {
                startup.line(&startup.warn(&format!("Search indexes are off ({error}).")));
                None
            }
        };
        // What the remote page shows, kept from the start so that a page
        // opened later has the history.
        let feed = remote::Feed::new(MAIN);
        feed.hide_thinking(settings.hide_thinking);
        // An app's user reads what was left out in its transcript.
        if options.source == remote::Source::App {
            for note in &mcp_notes {
                feed.notice(Level::Warn, note);
            }
        }
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
            monitors: !one_shot,
            ..AgentSettings::for_session(&settings, &provider_setup)
        };
        let main_config = agents::main_config(&agent_settings)?;
        let worker_config = agents::worker_config(&agent_settings)?;
        // Where the runs' command channels are kept, to ask for handovers.
        let runs = Arc::new(handover::Runs::default());
        let mut tools = if bare {
            Vec::new()
        } else {
            managers.tools(&shell, runs.clone())
        };
        tools.extend(mcp.tools().iter().cloned());
        if let Some(indexes) = &indexes {
            tools.extend(indexes.tools());
        }
        let (input_sender, input) = mpsc::unbounded_channel();
        let (requests_sender, requests) = mpsc::unbounded_channel();
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
            feed: Some(feed.clone()),
            page: Mutex::default(),
            changed: Default::default(),
            title: Default::default(),
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
            ask_about_workers: false,
            input_source: options.source,
            app_name: options.app_name,
            shows_page_links: options.shows_page_links,
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
            reopen: false,
            edited: None,
            cut_back: None,
            cut_report: None,
            refresh_turn: false,
            only_workers: false,
            images: Default::default(),
            pending_images: Vec::new(),
            pending_refs: Vec::new(),
            crew,
            asker,
            one_shot,
            exit_warned: false,
            jobs_shown: Vec::new(),
            jobs_listed: Vec::new(),
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
            interrupted_told: false,
            user_active_at: std::time::SystemTime::now(),
        };
        // A chat has no project.
        if !chat {
            harness.project = project::Found::read(&harness.settings.workspace);
        }
        // Before the workflow opens: it may restart a worker that hadn't started.
        harness.give_workers_project();
        prompter.opening();
        harness.open(!new)?;
        harness.import_seed().await?;
        harness.start_new_workers();
        harness.publish_models();
        harness.outside_at_start(new, options.allow_outside_workspace);
        if !new && let Some(tokens) = options.handover_at {
            harness.handover_at_start(tokens);
        }
        if !new && let Some(tokens) = options.workers_handover_at {
            harness.workers_handover_at_start(tokens);
        }
        if one_shot && let Some(note) = &harness.unoffered {
            output.line(&output.warn(&crate::ui::sentence(note)));
        }
        let inputs = Inputs {
            receiver: input,
            pending: None,
            questions,
            woken,
            remote: remote_input,
            requests,
            // With -p, Arko exits when the agent finishes, jobs or not.
            finished: (!one_shot).then(|| managers.jobs.subscribe_finished()),
            late: None,
            // With -p, monitors wake no one (the agents have none).
            alerts: (!one_shot).then(|| managers.jobs.subscribe_alerts()),
            late_alert: None,
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
            images: harness.images.clone(),
            meter: harness.gauges.clone(),
            status: harness.gauges.watch(),
            details: harness.details.clone(),
            input: input_sender,
            requests: requests_sender,
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
        let jobs = crate::jobs::follower(&harness.gauges, &harness.feed);
        let engine = Engine {
            _lock: lock,
            _alone: alone,
            publisher,
            jobs,
            harness,
            inputs,
            managers,
            remote_bind,
            remote_port_fallback: options.remote_port_fallback,
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

    /// Says how the front end shows the session, before [`run`](Self::run).
    pub fn front_end(&mut self, front: FrontEnd) {
        self.boxed_banner = front.boxed_banner;
        self.harness.ask_about_workers = front.asks_about_workers;
        self.harness.quit_popup = if front.asks_to_quit {
            self.quit_popup.clone()
        } else {
            None
        };
    }

    /// Runs the session until the user quits. It shows the banner, serves
    /// the web page if asked for, finishes the turns a resumed session left
    /// unfinished, then takes the user's messages and commands.
    pub async fn run(&mut self) -> RunEnd {
        let harness = &mut self.harness;
        harness.banner(self.new, self.boxed_banner);
        if let Some(bind) = self.remote_bind {
            harness
                .start_remote_with_fallback(bind, self.remote_port_fallback)
                .await;
        }
        if !self.new {
            harness.restart_unfinished(&mut self.inputs).await;
        }
        if !harness.exiting {
            harness.repl(&mut self.inputs).await;
        }
        if harness.reopen {
            RunEnd::Reopen
        } else {
            RunEnd::Quit
        }
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

    /// Stops what the session started, which is the web page (whose users
    /// are told), the workers (with a line saying that resuming the session
    /// continues them), the MCP servers, the index watchers and the
    /// terminal's jobs. The session is saved as it goes.
    pub async fn shutdown(self) {
        self.publisher.abort();
        self.jobs.abort();
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
        // The background jobs stop before the MCP servers and the indexes
        // their moved calls wait on, so the jobs are saved as stopped by the
        // exit, and the session continued later names them.
        self.managers.shutdown().await;
        harness.mcp.shutdown().await;
        if let Some(indexes) = &harness.indexes {
            indexes.shutdown().await;
        }
        harness.gauges.release();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::agents::{Effort, ModelSettings, resolve_model};
    use crate::mock_model::{ModelServer, complete, reply, tool_call, with_input};
    use crate::options::GivenClient;
    use crate::prompter::NoPrompter;
    use crate::providers::OpenAiAuth;
    use crate::remote::{Kind, Source};
    use crate::session::Settings;
    use crate::shell::WindowsShell;

    /// The tests that open a session without a context of its own, one at a
    /// time (a process has one such session).
    static ALONE_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn concurrent_sessions_keep_their_preferred_port_or_bind_a_free_one() {
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reserved.local_addr().unwrap().port();
        let options = |name: &str| Options {
            root: Some(temp.path().join(name)),
            mode: crate::session::Mode::Chat,
            model: Some("luna".into()),
            openai_auth: Some(OpenAiAuth::ApiKey),
            cache_1h: Some(false),
            client: Some(GivenClient(Arc::new(server.client()))),
            remote: Some(remote::Bind {
                ip: std::net::Ipv4Addr::LOCALHOST.into(),
                port: Some(port),
            }),
            remote_port_fallback: true,
            ..Options::default()
        };
        // Both engines have their options before either can bind.
        let first = options("one");
        let second = options("two");
        drop(reserved);
        let first = spawn(first, Arc::new(NoPrompter));
        let second = spawn(second, Arc::new(NoPrompter));
        let mut one = first.opened.await.unwrap().unwrap();
        let mut two = second.opened.await.unwrap().unwrap();
        let page = |seam: &mut Seam| {
            let mut status = seam.status.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if let Some(page) = status.borrow().page.clone() {
                            break page;
                        }
                        status.changed().await.unwrap();
                    }
                })
                .await
                .unwrap()
            }
        };
        let (one_page, two_page) = tokio::join!(page(&mut one), page(&mut two));
        assert_ne!(one_page.addr.port(), two_page.addr.port());
        assert!(one_page.addr.port() == port || two_page.addr.port() == port);
        for addr in [one_page.addr, two_page.addr] {
            assert!(std::net::TcpStream::connect(addr).is_ok());
        }
        let fallback = if one_page.addr.port() != port {
            &one
        } else {
            &two
        };
        assert!(fallback.feed.subscribe().0.items.iter().any(|item| {
            item.text.contains("preferred web page port") && item.text.contains("instead")
        }));
        // Explicit requests still require their exact port, even in a
        // session whose startup page was allowed to fall back.
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = Some(taken.local_addr().unwrap().to_string());
        assert!(
            ask_for(&one, |reply| Request::ShareWebPage { bind, reply })
                .await
                .is_err()
        );
        quit(one, first.ended).await;
        quit(two, second.ended).await;
    }

    /// An app serves the session's web page, stops it and gives it a new
    /// key with requests that answer: served to the network when it gives
    /// no address (whatever the page was served on before), with what went
    /// wrong when it can't be served, and its clients told it was done in
    /// the app.
    #[tokio::test]
    async fn an_app_shares_the_web_page_with_requests() {
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let share = |bind: Option<&str>| {
            let bind = bind.map(str::to_string);
            ask_for(&seam, move |reply| Request::ShareWebPage { bind, reply })
        };
        let here = share(Some("127.0.0.1:0")).await.unwrap();
        assert!(here.addr.ip().is_loopback(), "{:?}", here.addr);
        let everywhere = share(None).await.unwrap();
        assert!(
            everywhere.addr.ip().is_unspecified(),
            "{:?}",
            everywhere.addr
        );
        // On the port the page had, which was free again.
        assert_eq!(everywhere.addr.port(), here.addr.port());
        // The local network's links first, this computer's last.
        assert!(
            everywhere.links.last().is_some_and(|link| link.loopback),
            "{:?}",
            everywhere.links
        );
        let here = share(Some("127.0.0.1")).await.unwrap();
        assert!(here.addr.ip().is_loopback(), "{:?}", here.addr);
        let key_of = |page: &crate::remote::Page| {
            page.links[0].url.split("#key=").nth(1).unwrap().to_string()
        };
        let old_key = key_of(&here);
        let renewed = ask_for(&seam, |reply| Request::NewWebPageKey { reply })
            .await
            .unwrap();
        let renewed = renewed.expect("served with the new key");
        assert_ne!(key_of(&renewed), old_key);
        assert_eq!(
            ask_for(&seam, |reply| Request::StopWebPage { reply }).await,
            Ok(())
        );
        let notices: Vec<String> = seam
            .feed
            .subscribe()
            .0
            .items
            .into_iter()
            .filter(|item| item.kind == Kind::Notice)
            .map(|item| item.text)
            .collect();
        assert!(
            notices
                .iter()
                .any(|text| text == "The web page was turned off in an app."),
            "{notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|text| text.starts_with("The session's key was changed in an app"))
        );
        assert!(!notices.iter().any(|text| text.contains("at the terminal")));
        assert_eq!(
            ask_for(&seam, |reply| Request::NewWebPageKey { reply }).await,
            Ok(None)
        );
        // A port in use: what to change.
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let refused = share(Some(&format!("127.0.0.1:{port}"))).await.unwrap_err();
        assert!(
            refused.contains("in use") && refused.contains("Choose another port"),
            "{refused}"
        );
        assert!(share(Some("nowhere at all")).await.is_err());

        // A share that can't be served leaves the page served as it was:
        // on another port, and on its own port at an address not here.
        let served = share(Some("127.0.0.1:0")).await.unwrap();
        let serves = |addr: std::net::SocketAddr| {
            std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok()
                && crate::remote::Meter::read(seam.meter.as_ref())
                    .page
                    .is_some_and(|page| page.addr == addr)
        };
        assert!(share(Some(&format!("127.0.0.1:{port}"))).await.is_err());
        assert!(serves(served.addr));
        assert!(share(Some("192.0.2.1")).await.is_err());
        assert!(serves(served.addr));
        quit(seam, ended).await;
    }

    /// A front end that shows the web page's links itself keeps the
    /// session's key out of the transcript, which the page's clients get
    /// too.
    #[tokio::test]
    async fn the_transcript_says_where_the_page_is_without_its_key() {
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let running = spawn(
            Options {
                root: Some(temp.path().join("arko")),
                workspace: Some(project),
                model: Some("luna".into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                shows_page_links: true,
                app_name: "Notes",
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let page = ask_for(&seam, |reply| Request::ShareWebPage { bind: None, reply })
            .await
            .unwrap();
        assert!(
            page.links[0].url.contains("#key="),
            "the app gets the links"
        );
        let said = || -> Vec<String> {
            seam.feed
                .subscribe()
                .0
                .items
                .into_iter()
                .map(|item| item.text)
                .collect()
        };
        assert!(
            said().iter().all(|text| !text.contains("#key=")),
            "{:?}",
            said()
        );
        assert!(
            said().iter().any(|text| text.contains("are in Notes")),
            "{:?}",
            said()
        );
        // Nor do the commands that show the links and their codes, typed
        // in the app.
        for command in ["/remote", "/remote qr", "/remote qr lo", "/remote new-key"] {
            let before = said().len();
            seam.input
                .send(Input::Message(app_message(command)))
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while said().len() <= before {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            idle(&seam).await;
            assert!(
                said().iter().all(|text| !text.contains("#key=")),
                "{command}: {:?}",
                said()
            );
        }
        assert!(
            said()
                .iter()
                .any(|text| text == "The web page's links and QR codes are in Notes."),
            "{:?}",
            said()
        );
        quit(seam, running.ended).await;
    }

    /// An MCP config file an app gives that can't be read doesn't keep the
    /// session from opening: its servers aren't started, and the
    /// transcript says why, naming it.
    #[tokio::test]
    async fn an_apps_mcp_config_that_cant_be_read_is_left_out() {
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let missing = temp.path().join("gone.json");
        let running = spawn(
            Options {
                root: Some(temp.path().join("arko")),
                workspace: Some(project),
                model: Some("luna".into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                mcp_config: vec![missing.clone()],
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap_or_else(|error| panic!("{error}"));
        let notices: Vec<String> = seam
            .feed
            .subscribe()
            .0
            .items
            .into_iter()
            .filter(|item| item.kind == Kind::Notice)
            .map(|item| item.text)
            .collect();
        assert!(
            notices
                .iter()
                .any(|text| text.contains("MCP servers not started") && text.contains("gone.json")),
            "{notices:?}"
        );
        quit(seam, running.ended).await;
    }

    /// How long a worker ran is kept with the session: when it opens again,
    /// the worker's time goes on from there.
    #[tokio::test]
    async fn a_workers_run_time_is_kept_with_the_session() {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body.clone());
                Box::pin(async move {
                    if body.contains("You are agent run r1") {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        complete("w1", "Counted three.")
                    } else if body.contains("Counted three.") {
                        reply("m3", "Noted.")
                    } else if body.contains("call_m1") {
                        reply("m2", "Started it.")
                    } else {
                        tool_call(
                            "m1",
                            crate::crew::START_AGENT,
                            serde_json::json!({"agent": crate::agents::WORKER, "task": "Count."}),
                        )
                    }
                })
            }
        }))
        .await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let id = seam.session.id.clone();
        seam.input
            .send(Input::Message(app_message("Start a worker.")))
            .unwrap();
        let finished = || {
            seam.feed.subscribe().0.workers.into_iter().find(|worker| {
                worker.id == "r1" && worker.state == crate::remote::WorkerState::Finished
            })
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            while finished().is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        idle(&seam).await;
        let ran = finished().unwrap().ran_ms;
        assert!(ran >= 150, "{ran}");
        quit(seam, ended).await;
        let kept = std::fs::read_to_string(
            Session::open(&root, &id)
                .unwrap()
                .dir
                .join("worker-times.json"),
        )
        .unwrap();
        assert!(kept.contains(&format!("\"r1\": {ran}")), "{kept}");

        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id)).await;
        let restored = seam
            .feed
            .subscribe()
            .0
            .workers
            .into_iter()
            .find(|worker| worker.id == "r1")
            .unwrap();
        assert_eq!(restored.ran_ms, ran);
        assert_eq!(restored.running_since, None);
        quit(seam, ended).await;
    }

    /// A front end hears when the session's workflow starts loading, after
    /// its questions: a terminal takes Ctrl-C for the session from then on.
    #[tokio::test]
    async fn the_front_end_hears_when_the_workflow_loads() {
        struct Hearing(Arc<std::sync::atomic::AtomicBool>);
        #[async_trait::async_trait]
        impl Prompter for Hearing {
            async fn need_credentials(&self, need: &credentials::Need) -> Result<(), String> {
                Err(need.message())
            }
            async fn approve_project_mcp(
                &self,
                _approval: &crate::prompter::McpApproval,
            ) -> Result<Option<bool>, String> {
                Ok(None)
            }
            fn note(&self, _level: crate::remote::Level, _text: &str) {}
            fn opening(&self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let heard = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let running = spawn(
            Options {
                root: Some(temp.path().join("arko")),
                workspace: Some(project),
                model: Some("luna".into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            Arc::new(Hearing(heard.clone())),
        );
        let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(heard.load(std::sync::atomic::Ordering::SeqCst));
        quit(seam, running.ended).await;
    }

    /// A reply streamed in a burst (more pieces than the workflow's
    /// broadcast of events holds) reaches the feed whole: none of it is
    /// lost however far behind the feed falls.
    #[tokio::test]
    async fn a_burst_of_streamed_text_reaches_the_feed_whole() {
        let temp = tempfile::tempdir().unwrap();
        let pieces: Vec<String> = (0..20_000).map(|n| format!("{} ", n % 10)).collect();
        let whole: String = pieces.concat();
        let server = ModelServer::start(Arc::new({
            let pieces = pieces.clone();
            move |_, _body: String| {
                let stream = crate::mock_model::reply_in_pieces("burst", &pieces);
                Box::pin(async move { stream })
            }
        }))
        .await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        seam.input
            .send(Input::Message(app_message("Count.")))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            while !seam
                .feed
                .subscribe()
                .0
                .items
                .iter()
                .any(|item| item.kind == Kind::Reply)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        idle(&seam).await;
        let items = seam.feed.subscribe().0.items;
        let reply = items.iter().find(|item| item.kind == Kind::Reply).unwrap();
        assert_eq!(reply.text.trim(), whole.trim());
        assert!(
            !items
                .iter()
                .any(|item| item.text.contains("events not shown")),
            "nothing missed"
        );
        quit(seam, ended).await;
    }

    /// A new session's workers can hand over at a size of their own, which
    /// the session keeps and a resume sets again.
    #[tokio::test]
    async fn the_workers_hand_over_at_a_size_of_their_own() {
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        std::fs::create_dir_all(&project).unwrap();
        let open = |session: SessionChoice, workers: usize| {
            let running = spawn(
                Options {
                    root: Some(root.clone()),
                    session,
                    workspace: Some(project.clone()),
                    model: Some("luna".into()),
                    openai_auth: Some(OpenAiAuth::ApiKey),
                    cache_1h: Some(false),
                    handover_at: Some(600_000),
                    workers_handover_at: Some(workers),
                    client: Some(GivenClient(Arc::new(server.client()))),
                    ..Options::default()
                },
                Arc::new(NoPrompter),
            );
            async move {
                let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                (seam, running.ended)
            }
        };
        let (seam, ended) = open(SessionChoice::New, 300_000).await;
        let id = seam.session.id.clone();
        let settings = Session::open(&root, &id).unwrap().load_settings().unwrap();
        assert_eq!(settings.main.handover_at, Some(600_000));
        assert_eq!(settings.worker.handover_at, Some(300_000));
        assert_eq!(settings.workers_handover_at, Some(300_000));
        assert_eq!(seam.meter.read().settings.workers_handover_at, 300_000);
        quit(seam, ended).await;
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), 250_000).await;
        let settings = Session::open(&root, &id).unwrap().load_settings().unwrap();
        assert_eq!(settings.workers_handover_at, Some(250_000));
        assert_eq!(seam.meter.read().settings.workers_handover_at, 250_000);
        quit(seam, ended).await;
    }

    /// A process opens one session without `spawn`: it reads the process's
    /// settings and keys, so a second is refused, until the first closes.
    #[tokio::test]
    async fn a_second_session_without_spawn_is_refused() {
        let _alone = ALONE_TESTS.lock().await;
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
        // (A test client and an API key: no login of the machine's.)
        let options = || {
            let session = Session::create(&root).unwrap();
            session
                .save_settings(&Settings {
                    openai_auth: Some(OpenAiAuth::ApiKey),
                    ..Settings::for_test(&workspace, &luna)
                })
                .unwrap();
            Options {
                root: Some(root.clone()),
                session: SessionChoice::Resume(session.id),
                interactive: false,
                client: Some(GivenClient(Arc::new(arko_ai::LLMClient::new()))),
                ..Options::default()
            }
        };
        let (first, _seam) = Engine::open(options(), Arc::new(NoPrompter)).await.unwrap();
        match Engine::open(options(), Arc::new(NoPrompter)).await {
            Err(OpenError::Failed(why)) => assert!(why.contains("arko::spawn"), "{why}"),
            Err(error) => panic!("{error}"),
            Ok(_) => panic!("a second opened"),
        }
        first.shutdown().await;
        let (again, _seam) = Engine::open(options(), Arc::new(NoPrompter)).await.unwrap();
        again.shutdown().await;
    }

    /// A front end adds and removes a session's keys from any thread: they
    /// are saved in the session's Arko folder, and read as the session reads
    /// them (a resumed session: its workspace's `.env`, not the current
    /// directory's).
    #[tokio::test]
    async fn a_sessions_keys_are_its_own_from_any_thread() {
        if std::env::var_os(credentials::OPENROUTER_KEY).is_some() {
            return; // (Set in this process: it comes first.)
        }
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        std::fs::create_dir_all(&project).unwrap();
        let id = {
            let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
            let id = seam.session.id.clone();
            quit(seam, ended).await;
            id
        };
        let theirs = "sk-or-v1-fedcba9876543210fedcba9876543210";
        std::fs::write(
            project.join(".env"),
            format!("OPENROUTER_API_KEY={theirs}\n"),
        )
        .unwrap();
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id)).await;
        let key: &'static credentials::Key = credentials::KEYS
            .into_iter()
            .find(|key| key.name == credentials::OPENROUTER_KEY)
            .unwrap();
        // (This thread isn't the session's.)
        let added = seam.keys.add(
            key,
            keys::Secret::new("sk-or-v1-0123456789abcdef0123456789abcdef"),
        );
        assert!(matches!(added, keys::Outcome::Done { .. }), "{added:?}");
        let saved = std::fs::read_to_string(root.join(".env")).unwrap();
        assert!(
            saved.contains("OPENROUTER_API_KEY="),
            "in the session's Arko folder"
        );
        let removed = seam.keys.remove(key);
        let keys::Outcome::Done { said, .. } = removed else {
            panic!("{removed:?}");
        };
        let workspace_env = project.join(".env").display().to_string();
        assert!(said.contains(&workspace_env), "{said}");
        quit(seam, ended).await;
    }

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
    /// ID. Once closed, its threads are gone.
    #[tokio::test]
    async fn two_sessions_run_turns_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let log = temp.path().join("logs").join("arko.log");
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
                // (Opened with the default identity.)
                assert_eq!(
                    (holder.app.as_str(), holder.pid),
                    ("app", std::process::id())
                );
                assert!(holder.describe().starts_with("an app (process "));
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
        // The sessions' locks are free again. (A process another test forks
        // may hold a lock's file for a moment, until it runs its program.)
        let released = std::time::Instant::now();
        while crate::session::holder(&first_seam.session.dir).is_some()
            && released.elapsed() < Duration::from_secs(2)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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

        // The process's log tags each session's lines. (No other test
        // installs a logger: one would have to read its own.)
        logging.expect("the process's log is this test's");
        log::logger().flush();
        let text = std::fs::read_to_string(&log).unwrap();
        for id in [&first_seam.session.id, &second_seam.session.id] {
            assert!(text.contains(&format!("] [{id}] ")), "{id}: {text}");
        }
    }

    /// The status goes out when it changes (a request reports its usage),
    /// once per change: not again while the session is idle.
    #[tokio::test]
    async fn the_status_changes_when_a_request_reports_usage() {
        let _alone = ALONE_TESTS.lock().await;
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

    /// An app's messages are tagged as its own, the turn says when it
    /// started, and the agent's question is answered with `Input::Answer`.
    #[tokio::test]
    async fn an_app_sends_messages_and_answers() {
        use arko_agents::builtins::ASK_USER_TOOL_NAME;
        let temp = tempfile::tempdir().unwrap();
        let server = ModelServer::start(Arc::new(|_, body: String| {
            Box::pin(async move {
                if body.contains("Two spaces.") {
                    reply("m2", "Indented with two spaces.")
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
        let workspace = temp.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let running = spawn(
            Options {
                root: Some(temp.path().join("arko")),
                workspace: Some(workspace),
                model: Some("luna".into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                text_replies: Some(true),
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        let wait = Duration::from_secs(30);
        let seam = tokio::time::timeout(wait, running.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        seam.input
            .send(Input::Message(crate::ui::Message {
                text: "Indent the file.".into(),
                images: Vec::new(),
            }))
            .unwrap();
        let question = tokio::time::timeout(wait, async {
            loop {
                if let Some(question) = seam.feed.activity().question {
                    return question;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(question.text, "Tabs or spaces?");
        assert!(seam.feed.activity().since.is_some(), "the turn's start");
        seam.input
            .send(Input::Answer {
                question: question.id,
                text: "Two spaces.".into(),
            })
            .unwrap();
        let items = || seam.feed.subscribe().0.items;
        tokio::time::timeout(wait, async {
            while !items()
                .iter()
                .any(|item| item.text == "Indented with two spaces.")
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let items = items();
        let user = items
            .iter()
            .find(|item| item.text == "Indent the file.")
            .unwrap();
        assert_eq!(user.source, Some(Source::App));
        let answer = items
            .iter()
            .find(|item| item.text == "Two spaces.")
            .unwrap();
        assert_eq!(answer.source, Some(Source::App));
        tokio::time::timeout(wait, async {
            while seam.feed.activity().working {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(seam.feed.activity().since, None);
        seam.input.send(Input::Quit(Some(Vec::new()))).unwrap();
        assert_eq!(
            tokio::time::timeout(wait, running.ended)
                .await
                .unwrap()
                .unwrap(),
            RunEnd::Quit
        );
    }

    /// A chat's agent is offered no tools and told the user's
    /// instructions: those set before its first message, and later ones
    /// from its next new conversation, also once it opens again. A chat
    /// works in its own folder, lists without one, and has no workers.
    #[tokio::test]
    async fn a_chat_has_no_tools_and_takes_the_users_instructions() {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body);
                Box::pin(async { reply("a", "Ahoy.") })
            }
        }))
        .await;
        let root = temp.path().join("arko");
        let running = spawn(
            Options {
                root: Some(root.clone()),
                mode: crate::session::Mode::Chat,
                chat_instructions: Some("Answer briefly.".into()),
                model: Some("luna".into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        let wait = Duration::from_secs(30);
        let seam = tokio::time::timeout(wait, running.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        /// Sends `text`, and waits until the feed has `count` replies.
        async fn ask(seam: &Seam, text: &str, count: usize) {
            seam.input
                .send(Input::Message(crate::ui::Message {
                    text: text.into(),
                    images: Vec::new(),
                }))
                .unwrap();
            let replies = || {
                seam.feed
                    .subscribe()
                    .0
                    .items
                    .iter()
                    .filter(|item| item.text == "Ahoy.")
                    .count()
            };
            tokio::time::timeout(Duration::from_secs(30), async {
                while replies() < count {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            idle(seam).await;
        }
        let instructions = |text: &str| {
            let (reply_to, answer) = tokio::sync::oneshot::channel();
            seam.requests
                .send(Request::ChatInstructions {
                    text: Some(text.into()),
                    reply: reply_to,
                })
                .unwrap();
            answer
        };
        // Before the first message, the new instructions replace the old.
        assert_eq!(instructions("Answer like a pirate.").await.unwrap(), Ok(()));
        ask(&seam, "Hello.", 1).await;
        let first = bodies.lock().unwrap()[0].clone();
        let request: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert!(
            request.get("tools").is_none() && request.get("tool_choice").is_none(),
            "{first}"
        );
        assert!(first.contains("# The user's instructions"), "{first}");
        assert!(first.contains("Answer like a pirate."));
        assert!(!first.contains("Tool Usage"));
        let listed = crate::sessions::list(&root);
        assert_eq!(listed[0].mode, crate::session::Mode::Chat);
        assert_eq!(listed[0].workspace, None);
        let settings = Session::open(&root, &seam.session.id)
            .unwrap()
            .load_settings()
            .unwrap();
        assert_eq!(settings.workspace, seam.session.dir);
        assert_eq!(settings.max_workers, 0);
        assert_eq!(
            settings.chat_instructions.as_deref(),
            Some("Answer like a pirate.")
        );
        assert!(!first.contains("Answer briefly."));
        // After it, the conversation keeps its own, and the next takes them.
        assert_eq!(instructions("Be brief.").await.unwrap(), Ok(()));
        ask(&seam, "Again.", 2).await;
        let second = bodies.lock().unwrap()[1].clone();
        assert!(second.contains("Answer like a pirate.") && second.contains("Hello."));
        assert!(!second.contains("Be brief."));
        ask(&seam, "/new", 2).await;
        ask(&seam, "Hi.", 3).await;
        let third = bodies.lock().unwrap()[2].clone();
        assert!(third.contains("Be brief.") && !third.contains("Hello."));
        assert!(!third.contains("Answer like a pirate."));
        seam.input.send(Input::Quit(Some(Vec::new()))).unwrap();
        assert_eq!(
            tokio::time::timeout(wait, running.ended)
                .await
                .unwrap()
                .unwrap(),
            RunEnd::Quit
        );

        // Opened again, it goes on with the conversation it was in.
        let running = spawn(
            Options {
                root: Some(root.clone()),
                session: SessionChoice::Resume(seam.session.id.clone()),
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        let seam = tokio::time::timeout(wait, running.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // (Its feed has the conversation's earlier reply.)
        ask(&seam, "And again.", 2).await;
        let fourth = bodies.lock().unwrap()[3].clone();
        assert!(fourth.contains("Hi.") && fourth.contains("Be brief."));
        assert!(!fourth.contains("Hello."));
        seam.input.send(Input::Quit(Some(Vec::new()))).unwrap();
        assert_eq!(
            tokio::time::timeout(wait, running.ended)
                .await
                .unwrap()
                .unwrap(),
            RunEnd::Quit
        );
    }

    /// A model server that replies "Reply N." to its Nth request, and the
    /// requests' bodies.
    async fn counting_server() -> (ModelServer, Arc<Mutex<Vec<String>>>) {
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                let count = {
                    let mut bodies = bodies.lock().unwrap();
                    bodies.push(body);
                    bodies.len()
                };
                // (A request that keeps the cache warm isn't streamed.)
                let streamed = body_streams(&bodies.lock().unwrap()[count - 1]);
                Box::pin(async move {
                    if streamed {
                        reply("a", &format!("Reply {count}."))
                    } else {
                        crate::mock_model::whole_reply("a", &format!("Reply {count}."), 900)
                    }
                })
            }
        }))
        .await;
        (server, bodies)
    }

    fn body_streams(body: &str) -> bool {
        body.contains("\"stream\":true")
    }

    /// An app's session on `server` in `root`, working in `project`.
    async fn open_app(
        root: &std::path::Path,
        project: &std::path::Path,
        server: &ModelServer,
        session: SessionChoice,
    ) -> (Seam, oneshot::Receiver<RunEnd>) {
        open_with(root, project, server.client(), "luna", session).await
    }

    /// An app's session in `root` on `model` through `client`, working in
    /// `project`.
    async fn open_with(
        root: &std::path::Path,
        project: &std::path::Path,
        client: arko_ai::LLMClient,
        model: &str,
        session: SessionChoice,
    ) -> (Seam, oneshot::Receiver<RunEnd>) {
        std::fs::create_dir_all(project).unwrap();
        let running = spawn(
            Options {
                root: Some(root.to_path_buf()),
                session,
                workspace: Some(project.to_path_buf()),
                model: Some(model.into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                text_replies: Some(true),
                client: Some(GivenClient(Arc::new(client))),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        (seam, running.ended)
    }

    /// Waits for the turn of the `count`th request, and gives its body.
    async fn answered(seam: &Seam, bodies: &Mutex<Vec<String>>, count: usize) -> String {
        tokio::time::timeout(Duration::from_secs(30), async {
            while bodies.lock().unwrap().len() < count || seam.feed.activity().working {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        bodies.lock().unwrap()[count - 1].clone()
    }

    fn app_message(text: &str) -> crate::ui::Message {
        crate::ui::Message {
            text: text.into(),
            images: Vec::new(),
        }
    }

    /// The user's messages and the replies in the feed.
    fn exchanged(seam: &Seam) -> Vec<String> {
        seam.feed
            .subscribe()
            .0
            .items
            .into_iter()
            .filter(|item| matches!(item.kind, Kind::User | Kind::Reply))
            .map(|item| item.text)
            .collect()
    }

    /// Waits until the session's turn ended.
    async fn idle(seam: &Seam) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while seam.feed.activity().working {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    /// Waits until a resumed session's feed has its conversation (it comes
    /// as the session starts running, after it opened).
    async fn resumed(seam: &Seam) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !seam
                .feed
                .subscribe()
                .0
                .items
                .iter()
                .any(|item| item.text == "Resumed.")
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    /// The feed's user's message `text`.
    fn user_item(seam: &Seam, text: &str) -> crate::remote::Item {
        seam.feed
            .subscribe()
            .0
            .items
            .into_iter()
            .find(|item| item.kind == Kind::User && item.text == text)
            .unwrap()
    }

    /// Sends `request` (made with the reply's sender) and gives its answer.
    async fn ask_for<T>(
        seam: &Seam,
        request: impl FnOnce(Reply<T>) -> Request,
    ) -> Result<T, String> {
        let (reply_to, answer) = oneshot::channel();
        seam.requests.send(request(reply_to)).unwrap();
        answer.await.unwrap()
    }

    /// Edits the user's message `item` to `text`.
    async fn edit(seam: &Seam, item: u64, text: &str) -> Result<(), String> {
        let message = app_message(text);
        ask_for(seam, move |reply| Request::Edit {
            item,
            message,
            reply,
        })
        .await
    }

    async fn quit(seam: Seam, ended: oneshot::Receiver<RunEnd>) {
        seam.input.send(Input::Quit(Some(Vec::new()))).unwrap();
        let end = tokio::time::timeout(Duration::from_secs(30), ended).await;
        assert_eq!(end.unwrap().unwrap(), RunEnd::Quit);
    }

    /// An app edits the user's message, or sends the latest again: the
    /// conversation is cut back to just before it and goes on from the new
    /// message, in the model's requests and in the feed. Only the user's
    /// messages can be edited, also those restored when the session opens
    /// again.
    #[tokio::test]
    async fn an_app_edits_and_regenerates_messages() {
        let temp = tempfile::tempdir().unwrap();
        let (server, bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let id = seam.session.id.clone();
        seam.input
            .send(Input::Message(app_message("One.")))
            .unwrap();
        answered(&seam, &bodies, 1).await;
        seam.input
            .send(Input::Message(app_message("Two.")))
            .unwrap();
        answered(&seam, &bodies, 2).await;
        assert_eq!(user_item(&seam, "One.").message_index, Some(0));
        let two = user_item(&seam, "Two.");
        assert!(two.message_index.is_some());

        // An edit: the conversation goes on from it.
        assert_eq!(edit(&seam, two.id, "Deux.").await, Ok(()));
        let third = answered(&seam, &bodies, 3).await;
        assert!(third.contains("One.") && third.contains("Reply 1.") && third.contains("Deux."));
        assert!(
            !third.contains("Two.") && !third.contains("Reply 2."),
            "{third}"
        );
        assert_eq!(exchanged(&seam), ["One.", "Reply 1.", "Deux.", "Reply 3."]);
        // Sent again: a new reply in place of the last.
        let regenerated = ask_for(&seam, |reply| Request::Regenerate { reply }).await;
        assert_eq!(regenerated, Ok(()));
        let fourth = answered(&seam, &bodies, 4).await;
        assert_eq!(fourth.matches("Deux.").count(), 1, "{fourth}");
        assert!(!fourth.contains("Reply 3."));
        assert_eq!(exchanged(&seam), ["One.", "Reply 1.", "Deux.", "Reply 4."]);
        // The first message: the conversation starts over.
        let one = user_item(&seam, "One.").id;
        assert_eq!(edit(&seam, one, "Uno.").await, Ok(()));
        let fifth = answered(&seam, &bodies, 5).await;
        assert!(fifth.contains("Uno.") && !fifth.contains("One.") && !fifth.contains("Deux."));
        assert_eq!(exchanged(&seam), ["Uno.", "Reply 5."]);
        // Only the user's messages.
        let reply = seam
            .feed
            .subscribe()
            .0
            .items
            .into_iter()
            .find(|item| item.text == "Reply 5.")
            .unwrap();
        assert_eq!(
            edit(&seam, reply.id, "No.").await,
            Err("This message can't be edited.".to_string())
        );
        quit(seam, ended).await;

        // Opened again, its restored messages can be edited.
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id)).await;
        resumed(&seam).await;
        let uno = user_item(&seam, "Uno.");
        assert_eq!(uno.message_index, Some(0));
        assert_eq!(edit(&seam, uno.id, "Eins.").await, Ok(()));
        let sixth = answered(&seam, &bodies, 6).await;
        assert!(
            sixth.contains("Eins.") && !sixth.contains("Uno."),
            "{sixth}"
        );
        quit(seam, ended).await;
    }

    /// The status tells when the main agent's latest request went out (the
    /// prompt cache lasts from then), what it held, and what the cache
    /// costs.
    #[tokio::test]
    async fn the_status_tells_of_the_latest_request_and_the_cache() {
        let temp = tempfile::tempdir().unwrap();
        let (server, bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        assert_eq!(seam.meter.read().last_request, None);
        let before = chrono::Utc::now().timestamp_millis();
        seam.input
            .send(Input::Message(app_message("One.")))
            .unwrap();
        answered(&seam, &bodies, 1).await;
        let after = chrono::Utc::now().timestamp_millis();
        let reading = seam.meter.read();
        let request = reading.last_request.clone().unwrap();
        assert_eq!(request.model, resolve_model("luna").unwrap().id);
        assert!((before..=after).contains(&request.at));
        let sizes = request.sizes.unwrap();
        assert!(sizes.system_prompt > 0 && sizes.user_text >= "One.".len());
        assert!(request.bytes_per_token > 0.0);
        assert_eq!(reading.cache.refreshed_at, Some(request.at));
        assert_eq!(reading.cache.tokens, reading.context_tokens);
        assert_eq!(reading.cache.on_plan, None);
        quit(seam, ended).await;
    }

    /// The prompt cache is kept warm out of band where the provider allows:
    /// the conversation's request with a ping after it, its output capped,
    /// and nothing joins the conversation or the feed.
    #[tokio::test]
    async fn the_cache_is_kept_warm_out_of_band() {
        let temp = tempfile::tempdir().unwrap();
        let (server, bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let refresh = || ask_for(&seam, |reply| Request::RefreshCache { reply });
        assert_eq!(
            refresh().await,
            Err("There is no conversation to keep warm yet.".to_string())
        );
        seam.input
            .send(Input::Message(app_message("One.")))
            .unwrap();
        answered(&seam, &bodies, 1).await;
        let before = chrono::Utc::now().timestamp_millis();
        let refreshed = refresh().await.unwrap();
        assert!(matches!(
            refreshed,
            crate::engine::Refreshed::OutOfBand { .. }
        ));
        let ping = bodies.lock().unwrap()[1].clone();
        assert!(ping.contains("One.") && ping.contains("Reply 1."), "{ping}");
        assert!(ping.contains("\"max_output_tokens\":16"), "{ping}");
        assert_eq!(exchanged(&seam), ["One.", "Reply 1."]);
        assert!(seam.meter.read().cache.refreshed_at >= Some(before));
        // The conversation goes on without the ping.
        seam.input
            .send(Input::Message(app_message("Two.")))
            .unwrap();
        let third = answered(&seam, &bodies, 3).await;
        assert!(!third.contains("Reply 2."), "{third}");
        quit(seam, ended).await;
    }

    /// Where the provider takes no refresh out of band (Gemini), a short
    /// message and its reply keep the cache warm, marked as such in the
    /// feed.
    #[tokio::test]
    async fn a_cache_without_refresh_is_kept_warm_with_a_message() {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                let acknowledged = body.contains("only to keep the prompt cache warm");
                bodies.lock().unwrap().push(body);
                Box::pin(async move {
                    crate::mock_model::gemini_reply(
                        "g",
                        if acknowledged {
                            "Acknowledged"
                        } else {
                            "Hello."
                        },
                    )
                })
            }
        }))
        .await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_with(
            &root,
            &project,
            server.gemini_client(),
            "gemini",
            SessionChoice::New,
        )
        .await;
        seam.input.send(Input::Message(app_message("Hi."))).unwrap();
        answered(&seam, &bodies, 1).await;
        let refreshed = ask_for(&seam, |reply| Request::RefreshCache { reply }).await;
        assert_eq!(refreshed, Ok(crate::engine::Refreshed::Message));
        answered(&seam, &bodies, 2).await;
        let items = seam.feed.subscribe().0.items;
        let refresh = items
            .iter()
            .find(|item| item.delivery == Some(crate::remote::Delivery::Refresh))
            .unwrap();
        assert_eq!(refresh.text, crate::remote::REFRESH_MESSAGE);
        assert_eq!(refresh.message_index, None);
        assert_eq!(
            exchanged(&seam),
            [
                "Hi.",
                "Hello.",
                crate::remote::REFRESH_MESSAGE,
                "Acknowledged"
            ]
        );
        quit(seam, ended).await;
    }

    /// The smallest PNG: one pixel.
    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn pixel() -> Vec<u8> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(PIXEL)
            .unwrap()
    }

    /// The user's images go to the model with their message, and the
    /// transcript shows them, as it shows an image a tool returns, on its
    /// call. Both are kept in the session's images, and come back with the
    /// conversation when the session opens again.
    #[tokio::test]
    async fn images_go_with_messages_and_come_back_from_tools() {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                let count = {
                    let mut bodies = bodies.lock().unwrap();
                    bodies.push(body.clone());
                    bodies.len()
                };
                Box::pin(async move {
                    if body.contains("Read it.") && count == 2 {
                        tool_call(
                            "r",
                            arko_agents::builtins::READ_FILE_TOOL_NAME,
                            serde_json::json!({"path": "pixel.png"}),
                        )
                    } else {
                        reply("a", &format!("Reply {count}."))
                    }
                })
            }
        }))
        .await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("pixel.png"), pixel()).unwrap();
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let attachment = crate::ui::Attachment {
            bytes: pixel().into(),
            mime: "image/png".into(),
            name: Some("dot.png".into()),
        };
        seam.input
            .send(Input::Message(crate::ui::Message {
                text: "Look at this.".into(),
                images: vec![attachment],
            }))
            .unwrap();
        let first = answered(&seam, &bodies, 1).await;
        assert!(first.contains(PIXEL), "the image goes with the message");
        let look = user_item(&seam, "Look at this.");
        assert_eq!(look.images.len(), 1);
        assert_eq!(look.images[0].label, "dot.png");
        let stored = seam.images.get(look.images[0].id).unwrap();
        assert_eq!(&*stored.bytes, &pixel()[..]);
        // A tool's image shows on its call.
        seam.input
            .send(Input::Message(app_message("Read it.")))
            .unwrap();
        answered(&seam, &bodies, 3).await;
        let items = seam.feed.subscribe().0.items;
        let call = items
            .iter()
            .find(|item| item.tool.as_deref() == Some(arko_agents::builtins::READ_FILE_TOOL_NAME))
            .unwrap();
        assert_eq!(call.images.len(), 1, "{call:?}");
        assert_eq!(
            &*seam.images.get(call.images[0].id).unwrap().bytes,
            &pixel()[..]
        );
        quit(seam, ended).await;

        // Opened again, the conversation's images are there.
        let id = std::fs::read_dir(root.join("sessions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .into_owned();
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id)).await;
        resumed(&seam).await;
        let look = user_item(&seam, "Look at this.");
        assert_eq!(look.images.len(), 1);
        assert_eq!(
            &*seam.images.get(look.images[0].id).unwrap().bytes,
            &pixel()[..]
        );
        let items = seam.feed.subscribe().0.items;
        let call = items
            .iter()
            .find(|item| item.tool.as_deref() == Some(arko_agents::builtins::READ_FILE_TOOL_NAME))
            .unwrap();
        assert_eq!(call.images.len(), 1);
        quit(seam, ended).await;
    }

    /// A branch is a new session with the conversation up to a message of
    /// the user's: it goes on apart from the session it came from, also
    /// once it opens again. One before the first message has none of it.
    /// A closed session is branched from what it saved
    /// (`sessions::branch`).
    #[tokio::test]
    async fn a_branch_goes_on_apart_from_its_session() {
        let temp = tempfile::tempdir().unwrap();
        let (server, bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let id = seam.session.id.clone();
        seam.input
            .send(Input::Message(app_message("One.")))
            .unwrap();
        answered(&seam, &bodies, 1).await;
        seam.input
            .send(Input::Message(app_message("Two.")))
            .unwrap();
        answered(&seam, &bodies, 2).await;
        // Before the first message: a branch with none of it.
        let one = user_item(&seam, "One.").id;
        let empty = ask_for(&seam, |reply| Request::Branch {
            before: Some(one),
            reply,
        })
        .await
        .unwrap();
        // Open, it can't be branched from what it saved.
        assert!(matches!(
            crate::sessions::branch(&root, &id),
            Err(crate::sessions::BranchError::Locked { holder: Some(_) })
        ));
        let two = user_item(&seam, "Two.").id;
        let branch = ask_for(&seam, |reply| Request::Branch {
            before: Some(two),
            reply,
        })
        .await
        .unwrap();
        // The session goes on as it was.
        assert_eq!(exchanged(&seam), ["One.", "Reply 1.", "Two.", "Reply 2."]);
        quit(seam, ended).await;
        let settings = Session::open(&root, &branch)
            .unwrap()
            .load_settings()
            .unwrap();
        assert_eq!(settings.branched_from.as_deref(), Some(id.as_str()));
        assert_eq!(settings.title.as_deref(), Some("Branch of One."));

        let open_branch = || {
            open_app(
                &root,
                &project,
                &server,
                SessionChoice::Resume(branch.clone()),
            )
        };
        let (seam, ended) = open_branch().await;
        resumed(&seam).await;
        assert_eq!(exchanged(&seam), ["One.", "Reply 1."]);
        seam.input
            .send(Input::Message(app_message("Deux.")))
            .unwrap();
        let third = answered(&seam, &bodies, 3).await;
        assert!(third.contains("One.") && third.contains("Reply 1.") && third.contains("Deux."));
        assert!(!third.contains("Two."), "{third}");
        quit(seam, ended).await;
        assert!(
            !Session::open(&root, &branch)
                .unwrap()
                .dir
                .join(crate::session::SEED_FILE)
                .exists()
        );

        // Opened, it keeps its name.
        let entry = |id: &str| {
            crate::sessions::list(&root)
                .into_iter()
                .find(|entry| entry.id == id)
                .unwrap()
        };
        assert_eq!(entry(&branch).title, "Branch of One.");
        // Opened again, it goes on from its own conversation.
        let (seam, ended) = open_branch().await;
        seam.input
            .send(Input::Message(app_message("Trois.")))
            .unwrap();
        let fourth = answered(&seam, &bodies, 4).await;
        assert!(
            fourth.contains("Deux.") && fourth.contains("Reply 3.") && !fourth.contains("Two.")
        );
        quit(seam, ended).await;

        // The branch before the first message starts from its own.
        let (seam, ended) = open_app(
            &root,
            &project,
            &server,
            SessionChoice::Resume(empty.clone()),
        )
        .await;
        assert!(exchanged(&seam).is_empty());
        seam.input
            .send(Input::Message(app_message("Uno.")))
            .unwrap();
        let fifth = answered(&seam, &bodies, 5).await;
        assert!(fifth.contains("Uno.") && !fifth.contains("One."), "{fifth}");
        quit(seam, ended).await;
        let settings = Session::open(&root, &empty)
            .unwrap()
            .load_settings()
            .unwrap();
        assert_eq!(settings.branched_from.as_deref(), Some(id.as_str()));

        // Closed, the session is branched from what it saved, as its own
        // request branches all of it.
        let saved = crate::sessions::branch(&root, &id).unwrap();
        let settings = Session::open(&root, &saved)
            .unwrap()
            .load_settings()
            .unwrap();
        assert_eq!(settings.branched_from.as_deref(), Some(id.as_str()));
        assert_eq!(settings.title.as_deref(), Some("Branch of One."));
        // A branch never opened is branched from its seed.
        let again = crate::sessions::branch(&root, &saved).unwrap();
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(again)).await;
        resumed(&seam).await;
        assert_eq!(exchanged(&seam), ["One.", "Reply 1.", "Two.", "Reply 2."]);
        quit(seam, ended).await;
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(saved)).await;
        resumed(&seam).await;
        assert_eq!(exchanged(&seam), ["One.", "Reply 1.", "Two.", "Reply 2."]);
        quit(seam, ended).await;
        // One renamed is named so.
        crate::sessions::rename(&root, &id, "Numbers").unwrap();
        let renamed = crate::sessions::branch(&root, &id).unwrap();
        let settings = Session::open(&root, &renamed)
            .unwrap()
            .load_settings()
            .unwrap();
        assert_eq!(settings.title.as_deref(), Some("Branch of Numbers"));
        // A session without a conversation isn't.
        let new = Session::create(&root).unwrap();
        new.save_settings(&settings).unwrap();
        let refused = crate::sessions::branch(&root, &new.id).unwrap_err();
        assert_eq!(refused, crate::sessions::BranchError::NoConversation);
        assert_eq!(
            refused.to_string(),
            "Send a message first, then branch from it."
        );
    }

    /// A session switches between Agent and Chat between turns: it opens
    /// again, and its conversation goes on with the new mode's tools and
    /// instructions (the same conversation, under the same cache key). A
    /// switch waits for the turn.
    #[tokio::test]
    async fn a_session_switches_between_chat_and_agent() {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body.clone());
                Box::pin(async move {
                    if body.contains("Slowly.") {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                    }
                    reply("a", "Ahoy.")
                })
            }
        }))
        .await;
        let root = temp.path().join("arko");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let wait = Duration::from_secs(30);
        let open = |session: SessionChoice| {
            let running = spawn(
                Options {
                    root: Some(root.clone()),
                    session,
                    workspace: Some(project.clone()),
                    model: Some("luna".into()),
                    openai_auth: Some(OpenAiAuth::ApiKey),
                    cache_1h: Some(false),
                    text_replies: Some(true),
                    client: Some(GivenClient(Arc::new(server.client()))),
                    ..Options::default()
                },
                Arc::new(NoPrompter),
            );
            async move {
                let seam = tokio::time::timeout(wait, running.opened)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                (seam, running.ended)
            }
        };
        // Sends `text`, and waits for the turn of the `count`th request.
        let ask = |seam: &Seam, text: &str, count: usize| {
            seam.input
                .send(Input::Message(crate::ui::Message {
                    text: text.into(),
                    images: Vec::new(),
                }))
                .unwrap();
            let feed = seam.feed.clone();
            let bodies = bodies.clone();
            async move {
                tokio::time::timeout(wait, async {
                    while bodies.lock().unwrap().len() < count || feed.activity().working {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                bodies.lock().unwrap()[count - 1].clone()
            }
        };
        let switch = |seam: &Seam, to: crate::session::Mode| {
            let (reply_to, answer) = tokio::sync::oneshot::channel();
            seam.requests
                .send(Request::SwitchMode {
                    to,
                    workspace: None,
                    reply: reply_to,
                })
                .unwrap();
            async move { answer.await.unwrap() }
        };
        let json = |body: &str| serde_json::from_str::<serde_json::Value>(body).unwrap();

        let (seam, ended) = open(SessionChoice::New).await;
        let id = seam.session.id.clone();
        let first = ask(&seam, "First.", 1).await;
        assert!(json(&first).get("tools").is_some(), "{first}");
        // A switch waits for the turn.
        seam.input
            .send(Input::Message(crate::ui::Message {
                text: "Slowly.".into(),
                images: Vec::new(),
            }))
            .unwrap();
        tokio::time::timeout(wait, async {
            while bodies.lock().unwrap().len() < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            switch(&seam, crate::session::Mode::Chat).await,
            Err("Wait for the turn to end.".to_string())
        );
        tokio::time::timeout(wait, async {
            while seam.feed.activity().working {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(switch(&seam, crate::session::Mode::Chat).await, Ok(()));
        assert_eq!(
            tokio::time::timeout(wait, ended).await.unwrap().unwrap(),
            RunEnd::Reopen
        );
        let settings = Session::open(&root, &id).unwrap().load_settings().unwrap();
        assert_eq!(settings.mode, crate::session::Mode::Chat);
        assert!(settings.switched_mode);

        // In Chat, the conversation goes on without tools.
        let (seam, ended) = open(SessionChoice::Resume(id.clone())).await;
        let chat = ask(&seam, "Second.", 3).await;
        assert!(json(&chat).get("tools").is_none(), "{chat}");
        assert!(chat.contains("First.") && chat.contains("in a chat with the user"));
        assert_eq!(
            json(&chat).get("prompt_cache_key"),
            json(&first).get("prompt_cache_key")
        );
        // Back to Agent, in the folder it worked in.
        assert_eq!(switch(&seam, crate::session::Mode::Agent).await, Ok(()));
        assert_eq!(
            tokio::time::timeout(wait, ended).await.unwrap().unwrap(),
            RunEnd::Reopen
        );
        let (seam, ended) = open(SessionChoice::Resume(id.clone())).await;
        let agent = ask(&seam, "Third.", 4).await;
        assert!(json(&agent).get("tools").is_some(), "{agent}");
        assert!(agent.contains("First.") && agent.contains("Second."));
        assert!(!agent.contains("in a chat with the user"));
        seam.input.send(Input::Quit(Some(Vec::new()))).unwrap();
        assert_eq!(
            tokio::time::timeout(wait, ended).await.unwrap().unwrap(),
            RunEnd::Quit
        );
    }

    /// A session's conversation is read whole from what it saved, also
    /// while it is open, for an export.
    #[tokio::test]
    async fn a_sessions_conversation_is_read_whole_from_what_it_saved() {
        let temp = tempfile::tempdir().unwrap();
        let (server, bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let id = seam.session.id.clone();
        for (text, count) in [("One.", 1), ("Two.", 2)] {
            seam.input.send(Input::Message(app_message(text))).unwrap();
            answered(&seam, &bodies, count).await;
        }
        let read = |number| crate::sessions::conversation(&root, &id, number);
        let shown = |saved: &crate::sessions::SavedConversation| -> Vec<(String, Option<u64>)> {
            saved
                .items
                .iter()
                .map(|item| (item.text.clone(), item.turn))
                .collect()
        };
        let open = read(None).unwrap();
        assert_eq!((open.number, open.title.as_str()), (1, "One."));
        assert_eq!(
            shown(&open),
            [
                ("One.".to_string(), Some(1)),
                ("Reply 1.".to_string(), Some(1)),
                ("Two.".to_string(), Some(2)),
                ("Reply 2.".to_string(), Some(2)),
            ]
        );
        assert!(open.reports.is_empty());
        seam.input.send(Input::Line("/new".into())).unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while crate::conversations::Conversations::load(&seam.session.dir)
                .is_none_or(|conversations| conversations.current.is_some())
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            read(None).err().as_deref(),
            Some("The session's new conversation has no messages yet.")
        );
        assert_eq!(shown(&read(Some(1)).unwrap()), shown(&open));
        quit(seam, ended).await;
        assert_eq!(shown(&read(Some(1)).unwrap()), shown(&open));
        assert_eq!(
            read(Some(2)).err().as_deref(),
            Some("The session has no conversation 2.")
        );
        assert!(crate::sessions::conversation(&root, "20990101-000000-none", None).is_err());
    }

    /// An app exports the conversation it has open through the session,
    /// which answers only once nothing can still add to it: not during a
    /// turn, nor with a note typed during it on its way to the agent, nor
    /// with input waiting to be read. Then the export has all of it, the
    /// notes typed during turns and a message the stopped turn went on
    /// with too, as the transcript shows them, also once reopened.
    #[tokio::test]
    async fn an_export_waits_until_nothing_can_still_add_to_the_conversation() {
        use crate::remote::Delivery;
        use std::sync::atomic::{AtomicBool, Ordering};
        let temp = tempfile::tempdir().unwrap();
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let release = Arc::new(AtomicBool::new(false));
        let server = ModelServer::start(Arc::new({
            let (bodies, release) = (bodies.clone(), release.clone());
            move |_, body: String| {
                let count = {
                    let mut bodies = bodies.lock().unwrap();
                    bodies.push(body);
                    bodies.len()
                };
                let release = release.clone();
                Box::pin(async move {
                    // The second reply waits until the test lets it go, and
                    // the fourth until its turn is stopped.
                    let started = std::time::Instant::now();
                    while (count == 2 && !release.load(Ordering::SeqCst) || count == 4)
                        && started.elapsed() < Duration::from_secs(30)
                    {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    reply("a", &format!("Reply {count}."))
                })
            }
        }))
        .await;
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let id = seam.session.id.clone();
        let export = |seam: &Seam| {
            let (reply_to, answer) = oneshot::channel();
            seam.requests
                .send(Request::ExportConversation { reply: reply_to })
                .unwrap();
            async move {
                tokio::time::timeout(Duration::from_secs(30), answer)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        let users = |items: &[crate::remote::Item]| -> Vec<(String, Option<Delivery>)> {
            items
                .iter()
                .filter(|item| matches!(item.kind, Kind::User | Kind::Reply))
                .map(|item| (item.text.clone(), item.delivery))
                .collect()
        };
        let texts = |saved: crate::sessions::SavedConversation| users(&saved.items);
        let until = |what: &'static str, done: Box<dyn Fn() -> bool + Send>| async move {
            tokio::time::timeout(Duration::from_secs(30), async {
                while !done() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        };
        let in_feed = |seam: &Seam, text: &'static str| {
            let feed = seam.feed.clone();
            Box::new(move || {
                feed.subscribe()
                    .0
                    .items
                    .iter()
                    .any(|item| item.text == text)
            }) as Box<dyn Fn() -> bool + Send>
        };
        let requests = |count: usize| {
            let bodies = bodies.clone();
            Box::new(move || bodies.lock().unwrap().len() == count) as Box<dyn Fn() -> bool + Send>
        };
        let after_the_reply = Err("Export after the reply finishes.".to_string());
        let (turn, note, reply) = (Some(Delivery::Turn), Some(Delivery::Note), None);
        let said = |text: &str, delivery: Option<Delivery>| (text.to_string(), delivery);

        seam.input
            .send(Input::Message(app_message("First question.")))
            .unwrap();
        answered(&seam, &bodies, 1).await;
        assert_eq!(
            texts(export(&seam).await.unwrap()),
            [said("First question.", turn), said("Reply 1.", reply)]
        );

        // During a turn, and with a note typed during it.
        seam.input
            .send(Input::Message(app_message(
                "Second question still working.",
            )))
            .unwrap();
        until("the second request", requests(2)).await;
        assert_eq!(export(&seam).await.map(texts), after_the_reply);
        seam.input
            .send(Input::Message(app_message("Also please count cherries.")))
            .unwrap();
        until("the note", in_feed(&seam, "Also please count cherries.")).await;
        assert_eq!(export(&seam).await.map(texts), after_the_reply);
        // The agent replied before it read the note, which then goes to it
        // in a turn of its own.
        release.store(true, Ordering::SeqCst);
        answered(&seam, &bodies, 3).await;
        let mut exchanged = vec![
            said("First question.", turn),
            said("Reply 1.", reply),
            said("Second question still working.", turn),
            said("Reply 2.", reply),
            said("Also please count cherries.", note),
            said("Reply 3.", reply),
        ];
        assert_eq!(texts(export(&seam).await.unwrap()), exchanged);

        // Stopped before the agent read a note: the note goes with the next
        // message, and until then an export would leave it out.
        seam.input
            .send(Input::Message(app_message("Fourth question.")))
            .unwrap();
        until("the fourth request", requests(4)).await;
        seam.input
            .send(Input::Message(app_message("And the figs.")))
            .unwrap();
        until("the second note", in_feed(&seam, "And the figs.")).await;
        seam.input.send(Input::Interrupt).unwrap();
        until("the turn to stop", {
            let feed = seam.feed.clone();
            Box::new(move || !feed.activity().working)
        })
        .await;
        assert_eq!(
            export(&seam).await.map(texts),
            Err("What you typed during the last reply hasn't reached the agent yet. Export after your next message.".to_string())
        );
        seam.input
            .send(Input::Message(app_message("Fifth.")))
            .unwrap();
        answered(&seam, &bodies, 5).await;
        exchanged.extend([
            said("Fourth question.", turn),
            said("And the figs.", note),
            said("Fifth.", turn),
            said("Reply 5.", reply),
        ]);
        assert_eq!(texts(export(&seam).await.unwrap()), exchanged);

        // A message sent just before is input waiting, or a turn already.
        seam.input
            .send(Input::Message(app_message("Sixth.")))
            .unwrap();
        assert_eq!(export(&seam).await.map(texts), after_the_reply);
        answered(&seam, &bodies, 6).await;
        exchanged.extend([said("Sixth.", turn), said("Reply 6.", reply)]);
        assert_eq!(texts(export(&seam).await.unwrap()), exchanged);
        quit(seam, ended).await;

        // Reopened, the transcript shows the same.
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id)).await;
        resumed(&seam).await;
        assert_eq!(users(&seam.feed.subscribe().0.items), exchanged);
        quit(seam, ended).await;
    }

    #[tokio::test]
    async fn a_turn_after_stop_keeps_its_number_when_exported_and_reopened() {
        let temp = tempfile::tempdir().unwrap();
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |index, body: String| {
                bodies.lock().unwrap().push(body.clone());
                Box::pin(async move {
                    if body.contains("Slow command.") && !body.contains("function_call_output") {
                        tool_call("slow", "shell", serde_json::json!({"command": "sleep 20"}))
                    } else {
                        reply(&format!("r{index}"), &format!("Reply {index}."))
                    }
                })
            }
        }))
        .await;
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let id = seam.session.id.clone();
        seam.input
            .send(Input::Message(app_message("One.")))
            .unwrap();
        answered(&seam, &bodies, 1).await;
        seam.input
            .send(Input::Message(app_message("Slow command.")))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while !seam
                .feed
                .subscribe()
                .0
                .items
                .iter()
                .any(|item| item.kind == Kind::Call)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        seam.input.send(Input::Interrupt).unwrap();
        answered(&seam, &bodies, 2).await;
        seam.input
            .send(Input::Message(app_message("Go on.")))
            .unwrap();
        answered(&seam, &bodies, 3).await;
        let reply_turn = |items: &[crate::remote::Item]| {
            items
                .iter()
                .rev()
                .find(|item| item.kind == Kind::Reply)
                .unwrap()
                .turn
        };
        let live = reply_turn(&seam.feed.subscribe().0.items);
        assert_eq!(live, Some(3));
        quit(seam, ended).await;
        let exported = crate::sessions::conversation(&root, &id, None).unwrap();
        assert_eq!(reply_turn(&exported.items), live);
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id)).await;
        resumed(&seam).await;
        assert_eq!(reply_turn(&seam.feed.subscribe().0.items), live);
        quit(seam, ended).await;
    }

    /// A session that stopped while a call ran (a crash: its folder as it
    /// was then) is branched from what it saved, and the branch's next
    /// request answers that call, as providers require.
    #[tokio::test]
    async fn a_branch_of_a_session_stopped_mid_call_answers_the_call() {
        let temp = tempfile::tempdir().unwrap();
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let crashed = temp.path().join("crashed");
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |index, body: String| {
                bodies.lock().unwrap().push(body.clone());
                Box::pin(async move {
                    if body.contains("Run the slow thing.")
                        && !body.contains("function_call_output")
                    {
                        tool_call(
                            "slow",
                            crate::shell::SHELL_TOOL_NAME,
                            serde_json::json!({"command": "sleep 20; echo slept"}),
                        )
                    } else {
                        reply(&format!("r{index}"), &format!("Reply {index}."))
                    }
                })
            }
        }))
        .await;
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let id = seam.session.id.clone();
        seam.input
            .send(Input::Message(app_message("Run the slow thing.")))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while !seam
                .feed
                .subscribe()
                .0
                .items
                .iter()
                .any(|item| item.kind == Kind::Call)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        // The crash: the session's folder as it is now.
        fn copy(from: &std::path::Path, to: &std::path::Path) {
            std::fs::create_dir_all(to).unwrap();
            for entry in std::fs::read_dir(from).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    copy(&path, &to.join(entry.file_name()));
                } else {
                    let _ = std::fs::copy(&path, to.join(entry.file_name()));
                }
            }
        }
        copy(&seam.session.dir, &crashed.join("sessions").join(&id));
        quit(seam, ended).await;

        let branch = crate::sessions::branch(&crashed, &id).unwrap();
        let (seam, ended) =
            open_app(&crashed, &project, &server, SessionChoice::Resume(branch)).await;
        resumed(&seam).await;
        let before = bodies.lock().unwrap().len();
        seam.input
            .send(Input::Message(app_message("Go on.")))
            .unwrap();
        let body = answered(&seam, &bodies, before + 1).await;
        let input: serde_json::Value = serde_json::from_str(&body).unwrap();
        let ids = |kind: &str| -> Vec<String> {
            input["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == kind)
                .map(|item| item["call_id"].as_str().unwrap_or_default().to_string())
                .collect()
        };
        assert_eq!(ids("function_call"), ["call_slow"]);
        assert_eq!(ids("function_call_output"), ["call_slow"]);
        assert!(body.contains("Go on."));
        quit(seam, ended).await;
    }

    /// The message whose turn handed a chat's conversation over goes on
    /// from the handover's summary when it is sent again or edited: the
    /// summary stays in what the model gets.
    #[tokio::test]
    async fn a_message_after_a_handover_keeps_the_summary_when_edited() {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body.clone());
                Box::pin(async move {
                    if body.contains(
                        "This conversation is getting long, so it will continue in a fresh one",
                    ) {
                        reply("s", "SUMMARY: the user greeted Arko twice.")
                    } else if body.contains("SUMMARY: the user greeted Arko twice.") {
                        reply("b", "Answered from the summary.")
                    } else if body.contains("Hello again.") {
                        with_input(reply("a", "Second answer."), 40_000)
                    } else {
                        with_input(reply("a", "First answer."), 1_000)
                    }
                })
            }
        }))
        .await;
        let running = spawn(
            Options {
                root: Some(temp.path().join("arko")),
                mode: crate::session::Mode::Chat,
                model: Some("luna".into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                handover_at: Some(30_000),
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        for (text, count) in [("Hello.", 1), ("Hello again.", 2), ("And once more.", 4)] {
            seam.input.send(Input::Message(app_message(text))).unwrap();
            answered(&seam, &bodies, count).await;
        }
        assert!(user_item(&seam, "And once more.").message_index.is_some());
        let last = |bodies: &Mutex<Vec<String>>| bodies.lock().unwrap().last().unwrap().clone();
        // Sent again: from the summary, without the conversation before it.
        let regenerated = ask_for(&seam, |reply| Request::Regenerate { reply }).await;
        assert_eq!(regenerated, Ok(()));
        let again = answered(&seam, &bodies, 5).await;
        assert!(
            again.contains("SUMMARY: the user greeted Arko twice."),
            "{again}"
        );
        assert!(again.contains("And once more.") && !again.contains("First answer."));
        // Edited: the same.
        let once_more = user_item(&seam, "And once more.").id;
        assert_eq!(edit(&seam, once_more, "Something else.").await, Ok(()));
        let edited = answered(&seam, &bodies, 6).await;
        assert!(
            edited.contains("SUMMARY: the user greeted Arko twice."),
            "{edited}"
        );
        assert!(edited.contains("Something else.") && !edited.contains("And once more."));
        assert_eq!(last(&bodies), edited);

        // A branch before that message keeps the summary: the branch's
        // first message goes on from it, also edited.
        let something = user_item(&seam, "Something else.").id;
        let branch = ask_for(&seam, |reply| Request::Branch {
            before: Some(something),
            reply,
        })
        .await
        .unwrap();
        quit(seam, running.ended).await;
        let branch_id = branch.clone();
        let running = spawn(
            Options {
                root: Some(temp.path().join("arko")),
                session: SessionChoice::Resume(branch),
                model: Some("luna".into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(exchanged(&seam).is_empty());
        seam.input
            .send(Input::Message(app_message("Branched.")))
            .unwrap();
        let branched = answered(&seam, &bodies, 7).await;
        assert!(
            branched.contains("SUMMARY: the user greeted Arko twice.")
                && branched.contains("Branched."),
            "{branched}"
        );
        assert!(!branched.contains("Something else.") && !branched.contains("First answer."));
        assert_eq!(
            branched
                .matches("SUMMARY: the user greeted Arko twice.")
                .count(),
            1
        );
        let first = user_item(&seam, "Branched.");
        assert_eq!(first.message_index, Some(0));
        assert_eq!(edit(&seam, first.id, "Branched again.").await, Ok(()));
        let again = answered(&seam, &bodies, 8).await;
        assert!(
            again.contains("SUMMARY: the user greeted Arko twice.")
                && again.contains("Branched again."),
            "{again}"
        );
        assert!(!again.contains("Branched.\n"));
        quit(seam, running.ended).await;
        // The conversation is named by its new first message, and the
        // branch by its own name.
        let entry = crate::sessions::list(&temp.path().join("arko"))
            .into_iter()
            .find(|entry| entry.id == branch_id)
            .unwrap();
        assert!(entry.title.starts_with("Branch of "), "{}", entry.title);
        let current = entry
            .conversations
            .iter()
            .find(|conversation| conversation.current)
            .unwrap();
        assert_eq!(current.title, "Branched again.");
    }

    /// At the handover size a chat writes a summary in text, and the fresh
    /// conversation answers the message it was asked before. The messages
    /// from before the handover can't be edited any more.
    #[tokio::test]
    async fn a_chat_hands_over_with_a_summary_and_answers() {
        let temp = tempfile::tempdir().unwrap();
        // (A conversation that starts over the size isn't handed over: the
        // first request is small.)
        let server = ModelServer::start(Arc::new(|_, body: String| {
            Box::pin(async move {
                if body.contains(
                    "This conversation is getting long, so it will continue in a fresh one",
                ) {
                    reply("s", "SUMMARY: the user greeted Arko twice.")
                } else if body.contains("SUMMARY: the user greeted Arko twice.") {
                    reply("b", "Answered from the summary.")
                } else if body.contains("Hello again.") {
                    with_input(reply("a", "Second answer."), 40_000)
                } else {
                    with_input(reply("a", "First answer."), 1_000)
                }
            })
        }))
        .await;
        let running = spawn(
            Options {
                root: Some(temp.path().join("arko")),
                mode: crate::session::Mode::Chat,
                model: Some("luna".into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                handover_at: Some(30_000),
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            Arc::new(NoPrompter),
        );
        let wait = Duration::from_secs(30);
        let seam = tokio::time::timeout(wait, running.opened)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let texts = || -> Vec<String> {
            seam.feed
                .subscribe()
                .0
                .items
                .into_iter()
                .map(|item| item.text)
                .collect()
        };
        for (text, expected) in [
            ("Hello.", "First answer."),
            ("Hello again.", "Second answer."),
            ("And once more.", "Answered from the summary."),
        ] {
            seam.input
                .send(Input::Message(crate::ui::Message {
                    text: text.into(),
                    images: Vec::new(),
                }))
                .unwrap();
            tokio::time::timeout(wait, async {
                while !texts().iter().any(|said| said == expected) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{:?}", texts()));
            idle(&seam).await;
        }
        assert!(
            texts()
                .iter()
                .any(|said| said.contains("fresh conversation")),
            "{:?}",
            texts()
        );
        // A message of the fresh conversation with an earlier one's words.
        seam.input
            .send(Input::Message(crate::ui::Message {
                text: "Hello again. And more.".into(),
                images: Vec::new(),
            }))
            .unwrap();
        tokio::time::timeout(wait, async {
            while texts()
                .iter()
                .filter(|said| *said == "Answered from the summary.")
                .count()
                < 2
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        idle(&seam).await;
        // The messages before the handover aren't in the agent's
        // conversation any more: they can't be edited, even where the fresh
        // conversation has a message with their words at their place.
        let hello_again = user_item(&seam, "Hello again.");
        assert_eq!(hello_again.message_index, None);
        let refused = edit(&seam, hello_again.id, "Edited.").await.unwrap_err();
        assert!(refused.contains("handed over"), "{refused}");
        // The message the fresh conversation answered can.
        let once_more = user_item(&seam, "And once more.");
        assert!(once_more.message_index.is_some(), "{once_more:?}");
        assert!(
            user_item(&seam, "Hello again. And more.")
                .message_index
                .is_some()
        );
        seam.input.send(Input::Quit(Some(Vec::new()))).unwrap();
        assert_eq!(
            tokio::time::timeout(wait, running.ended)
                .await
                .unwrap()
                .unwrap(),
            RunEnd::Quit
        );
    }

    /// A session open in another Arko isn't opened: the front end is told
    /// who has it.
    #[tokio::test]
    async fn a_session_open_elsewhere_isnt_opened() {
        let _alone = ALONE_TESTS.lock().await;
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
        let _held = session.lock("arko", "Arko in a terminal").unwrap();
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
        let _alone = ALONE_TESTS.lock().await;
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

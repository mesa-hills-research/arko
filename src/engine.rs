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
    background_after_for, builtin_skills_resumed, client_asking, default_model, new_settings,
    project_from_options, workspace,
};
use crate::status::Gauges;
use crate::ui::{Input, Output, Stats};
use crate::{
    agents, chooser, credentials, crew, handover, index, keys, mcp, outside, profiles, project,
    providers, remote, session, skills,
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
    /// Sends `input`, what the user sent (a message, an answer, answers
    /// from an answer panel, a dismissal, approval answers), as
    /// [`Seam::input`] does, and answers with what became of it. A refusal
    /// is the client's to show, where it keeps what the user typed, so the
    /// transcript doesn't repeat it. A send is accepted once it went (a turn
    /// started with it, the agent's note board took it, a worker got it, a
    /// command ran). A stop, a quit and the like are accepted as they are
    /// read. When the session closes before it reads a send, it's refused
    /// as [`RefusalKind::Closed`](crate::RefusalKind::Closed). [`Seam::send`]
    /// makes one.
    Send {
        input: crate::ui::Input,
        reply: oneshot::Sender<crate::Verdict>,
    },
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

impl Seam {
    /// Sends `input` ([`Request::Send`]), and returns where its verdict
    /// comes. A closed session refuses it at once.
    pub fn send(&self, input: crate::ui::Input) -> oneshot::Receiver<crate::Verdict> {
        let (reply, verdict) = oneshot::channel();
        if let Err(unsent) = self.requests.send(Request::Send { input, reply })
            && let Request::Send { reply, .. } = unsent.0
        {
            let _ = reply.send(crate::Verdict::closed());
        }
        verdict
    }
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
            let lingering = context.clone();
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
            // The session is over for its app. A model naming it finishes
            // first (each way it asks has its own limit), since nothing else
            // of the session's goes on.
            if let Some(end) = end {
                let _ = ended_sender.send(end);
            }
            runtime.block_on(lingering.lingered(crate::session_names::naming::MAX_RUN));
            // Whatever the session left running stops here.
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
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
                // So can who chooses the workers' models and how deep they go.
                let choice = options.worker_models.is_some_and(|choice| {
                    std::mem::replace(&mut settings.worker_models, choice) != choice
                });
                let depth = match options.worker_depth {
                    Some(depth) => {
                        let depth = crate::workers::check_depth(depth)?;
                        std::mem::replace(&mut settings.worker_depth, depth) != depth
                    }
                    None => false,
                };
                let cap = match options.worker_retry_cap {
                    Some(cap) => {
                        let cap = Some(crate::workers::check_retry_cap(cap)?);
                        std::mem::replace(&mut settings.worker_retry_cap, cap) != cap
                    }
                    None => false,
                };
                // So can the built-in skills, for the conversations that start
                // after it.
                let (chosen, skipped) = builtin_skills_resumed(&options, &settings.builtin_skills);
                for warning in &skipped {
                    prompter.note(Level::Warn, warning);
                }
                let skills = match chosen {
                    Some(skills) => {
                        settings.builtin_skills = skills;
                        true
                    }
                    None => false,
                };
                let copies = options.message_copies.is_some_and(|copies| {
                    std::mem::replace(&mut settings.message_copies, copies) != copies
                });
                // So can how workers are paid for.
                let billing = options.worker_billing.is_some_and(|billing| {
                    std::mem::replace(&mut settings.worker_billing, billing) != billing
                });
                let budget = match options.api_budget {
                    Some(budget) => {
                        let budget = budget.map(crate::billing::check_budget).transpose()?;
                        std::mem::replace(&mut settings.api_budget, budget) != budget
                    }
                    None => false,
                };
                if project_from_options(&options, &mut settings)
                    | background
                    | wakes
                    | choice
                    | depth
                    | cap
                    | skills
                    | copies
                    | billing
                    | budget
                {
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
                    &root,
                    None,
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
                    providers::client(options.codex_auth.as_deref(), &root, &settings)
                })
                .await?,
            ),
        };
        // The OpenRouter models it starts on count among the ones picked.
        for model in [&settings.worker.model, &settings.main.model] {
            crate::openrouter::remember(&root, model);
        }
        // Under US or EU each model goes to the address the list of models
        // names, which a new session read as it checked its models. A resumed
        // one reads it now, so that its first request goes to the right one.
        let routing = crate::openrouter::Settings::load(&root);
        if !new
            && routing.region() == crate::openrouter::Region::UsOrEu
            && [&settings.main.model, &settings.worker.model]
                .iter()
                .any(|model| model.to_lowercase().starts_with(providers::OPENROUTER))
        {
            let _ = crate::openrouter::checked(&root, &routing, None).await;
        }
        let provider_setup = providers::Setup::new(settings.openai_auth);
        {
            // Settings the agents can't be built with leave no session behind.
            let agent_settings = AgentSettings::for_session(&settings, &provider_setup);
            agents::main_config(&agent_settings)?;
            agents::worker_config(&agent_settings)?;
        }
        // A chat has no tools: no MCP servers, skills, indexes or workers. One
        // that was an agent's has the skills, indexes and workers still (its
        // agent is offered none), for its agent's conversations, which resume
        // with their tools. MCP servers are never started in a chat.
        let chat = settings.mode == crate::session::Mode::Chat;
        let bare = chat && !settings.switched_mode;
        // A project's MCP servers are started once the user approves them.
        let (mcp_servers, mcp_notes) = if chat {
            (Vec::new(), Vec::new())
        } else {
            let mcp_plan = mcp::plan(
                &root,
                &settings.workspace,
                &options.mcp_config,
                options.mcp_config_required,
            )?;
            // One question for each of the project's files that waits.
            let mut mcp_answers = Vec::new();
            for approval in &mcp_plan.approvals {
                mcp_answers.push(prompter.approve_project_mcp(approval).await?);
            }
            mcp_plan.decide(&mcp_answers)?
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

        let mut managers = Managers::open(&session, &shell).map_err(|error| {
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
            // (The cache's lifetime is known once the conversation opens.)
            .set_warning_wakes_within(settings.monitor_wakes.within(None));

        // MCP servers, skills and search indexes: what goes wrong with them is
        // said, and the session goes on without.
        let startup = Output::new(None, one_shot);
        for note in &mcp_notes {
            startup.line(&startup.warn(&crate::ui::sentence(note)));
        }
        let mcp_servers = if mcp_servers.is_empty() {
            mcp::Servers::of(Vec::new(), None)
        } else {
            startup.line(&startup.dim(&format!(
                "Starting MCP servers {}…",
                mcp_servers
                    .iter()
                    .map(|server| server.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
            mcp::Servers::start(mcp_servers, Some(&root)).await
        };
        for server in mcp_servers.known() {
            if let Some(mcp::McpState::Failed(error)) = mcp_servers.state(&server.name) {
                startup.line(&startup.warn(&format!(
                    "The MCP server `{}` (in {}) didn't start ({error}).",
                    server.name,
                    server.file.display()
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
        if let Some(key) = credentials::var(credentials::OPENROUTER_KEY) {
            keys.use_openrouter_key(&key);
        }
        // A key added in another session open on Arko's folder is used here too.
        keys.share_async(root.clone()).await;
        keys.use_codex_auth(options.codex_auth.clone());
        let mcp_started = mcp_servers.running();
        let known_indexes = indexes
            .as_ref()
            .map(|indexes| indexes.known(&settings.workspace))
            .unwrap_or_default();
        let profile_names = chooser::named_profiles(
            &profiles::Roots::new(&root)
                .workspace(&settings.workspace)
                .session(&session.dir),
        );
        let agent_settings = AgentSettings {
            profiles: &profile_names,
            mcp_servers: &mcp_started,
            mcp_for_workers: mcp_servers.any_for_workers(),
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
        let mut tools = managers.tools(&shell, runs.clone());
        // The tools a session doesn't give, whose definitions stand in for
        // those a saved run was offered: a chat's (it has no tools), and the
        // monitors with -p, since no one would hear their alerts (the
        // workflow offers each agent every tool it's given).
        let mut withheld = Vec::new();
        if bare {
            withheld = std::mem::take(&mut tools);
        } else if one_shot {
            // As a session with workers defines them, which adds a sentence
            // on monitors that last while workers run.
            if settings.max_workers > 0 {
                managers.define_monitors_with_workers();
            }
            let monitors;
            (monitors, tools) = tools.into_iter().partition(|tool| {
                arko_agents::terminal::MonitorTool::NAMES.contains(&tool.name().as_str())
            });
            withheld = monitors;
        }
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
        let stats = Arc::new(Mutex::new(Stats {
            cache_memory: Some(Arc::new(crate::cache_memory::Memory::open(&session.dir))),
            chatgpt: client.chatgpt_accounts().cloned(),
            ..Stats::default()
        }));
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
            worker_models: Default::default(),
            billing: Default::default(),
            monitor_wakes: Default::default(),
            worker_depth: Default::default(),
            worker_retry_cap: Default::default(),
            message_copies: Default::default(),
            builtin_skills: Default::default(),
            extensions: Default::default(),
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
            feed: Some(feed.clone()),
            approvals: crate::approvals::Approvals::new(
                Some(feed.clone()),
                interactive,
                Some(output.clone()),
            ),
        });
        // The main agent's questions, kept with the session.
        feed.keep_questions(session.dir.join(crate::questions::FILE));
        // The workers, and those of a resumed session.
        let (crew, woken) = if settings.max_workers > 0 {
            let (crew, woken) = Crew::new(crew::Setup {
                session_id: session.id.clone(),
                session_dir: session.dir.clone(),
                output: output.clone(),
                max_running: settings.max_workers,
                cache_1h: settings.worker.cache_1h,
                openai_auth: settings.openai_auth,
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
                asker: Some(asker.clone()),
                root: Some(root.clone()),
            });
            (Some(crew), Some(woken))
        } else {
            (None, None)
        };

        // (What `/mcp` and an app's session info say of a chat's servers.)
        let mcp_notes = if chat {
            vec![crate::details::CHAT_MCP_NOTE.to_string()]
        } else {
            mcp_notes
        };
        let details = Arc::new(crate::details::Details::new(
            session.id.clone(),
            session.dir.clone(),
            root.clone(),
            gauges.clone(),
            indexes.clone(),
            skill_folders.clone(),
            mcp_servers.clone(),
            mcp_notes,
        ));
        // The main agent adds and removes servers, except in a chat.
        let mcp_notices = mcp::changes::Notices::new(output.clone(), Some(feed.clone()));
        let mcp = mcp::Session {
            changes: (!chat).then(|| {
                let gauges = gauges.clone();
                Arc::new(mcp::changes::Changes {
                    root: root.clone(),
                    workspace: settings.workspace.clone(),
                    given: options.mcp_config.clone(),
                    servers: mcp_servers.clone(),
                    ask: Arc::new(asker.approvals.clone()),
                    notices: Some(mcp_notices.clone()),
                    session: session.id.clone(),
                    title: Arc::new(move || {
                        Some(crate::lock(&gauges.title).clone()).filter(|title| !title.is_empty())
                    }),
                })
            }),
            notices: Some(mcp_notices),
            servers: mcp_servers,
        };
        // New workers get `mcp` when their set holds one of its servers.
        if let Some(crew) = &crew {
            crew.set_mcp_hub(mcp.servers.hub.clone());
        }
        let mut harness = Harness {
            ask_about_workers: false,
            input_source: options.source,
            app_name: options.app_name,
            shows_page_links: options.shows_page_links,
            details,
            settings,
            providers: provider_setup.clone(),
            kept: crate::kept_notes::KeptNotes::new(&session.dir),
            session,
            client,
            keys,
            main_config,
            worker_config,
            tools,
            withheld,
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
            naming: Default::default(),
            exit_warned: false,
            jobs_shown: Vec::new(),
            jobs_listed: Vec::new(),
            quit_popup: None,
            news_hint_shown: false,
            alert_hint_shown: false,
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
            waiting: Vec::new(),
            labelled: None,
            main_reconfigured: false,
            earlier: Arc::default(),
            continue_asked: None,
            #[cfg(test)]
            notes_refused: false,
        };
        // A chat has no project.
        if !chat {
            harness.project = project::Found::read(&harness.settings.workspace);
        }
        // Before the workflow opens: it may restart a worker that hadn't started.
        harness.give_workers_project();
        prompter.opening();
        harness.open(!new)?;
        if !new {
            // The conversation it goes on in may hold a list that changed
            // while the session was closed. (Not in `open`, which also
            // rebuilds the workflow of a session that never closed.)
            harness.kept.tell_lead(harness.workflow());
        }
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
        // (Whatever the reason, what clients sent from now on is refused.)
        self.inputs.close_sends();
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
        self.inputs.close_sends();
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
        harness.mcp.servers.shutdown().await;
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

    /// How long a worker ran and how many calls it made are kept with the
    /// session. When it opens again the worker shows both, and a run it goes
    /// on with adds to them.
    #[tokio::test]
    async fn a_workers_run_time_and_calls_are_kept_with_the_session() {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body.clone());
                let list = || serde_json::json!({"directory": "."});
                Box::pin(async move {
                    if body.contains("You are agent run r1") {
                        // Two calls, then its report. Once it is told to go
                        // on, one more call and a second report.
                        if body.contains("call_w3") {
                            complete("w4", "Counted again.")
                        } else if body.contains("Once more.") {
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            tool_call("w3", "list_files", list())
                        } else if body.contains("call_w2") {
                            complete("w5", "Counted three.")
                        } else if body.contains("call_w1") {
                            tool_call("w2", "list_files", list())
                        } else {
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            tool_call("w1", "list_files", list())
                        }
                    } else if body.contains("Counted again.") {
                        reply("m6", "Fine.")
                    } else if body.contains("call_m4") {
                        reply("m5", "Asked.")
                    } else if body.contains("Ask r1 again.") {
                        tool_call(
                            "m4",
                            crate::crew::MESSAGE_AGENT,
                            serde_json::json!({"to": "r1", "message": "Once more."}),
                        )
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
        // Waits until r1 finished with `calls` calls.
        let finished = |seam: &Seam, calls| {
            seam.feed.subscribe().0.workers.into_iter().find(|worker| {
                worker.id == "r1"
                    && worker.state == crate::remote::WorkerState::Finished
                    && worker.tool_calls == calls
            })
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            while finished(&seam, 2).is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        idle(&seam).await;
        let ran = finished(&seam, 2).unwrap().ran_ms;
        assert!(ran >= 150, "{ran}");
        quit(seam, ended).await;
        let kept = |name: &str| {
            std::fs::read_to_string(Session::open(&root, &id).unwrap().dir.join(name)).unwrap()
        };
        assert!(
            kept("worker-times.json").contains(&format!("\"r1\": {ran}")),
            "{}",
            kept("worker-times.json")
        );
        assert!(
            kept("worker-calls.json").contains("\"r1\": 2"),
            "{}",
            kept("worker-calls.json")
        );

        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id)).await;
        let restored = finished(&seam, 2).unwrap();
        assert_eq!(restored.ran_ms, ran);
        assert_eq!(restored.running_since, None);
        // The worker goes on, and its figures with it.
        seam.input
            .send(Input::Message(app_message("Ask r1 again.")))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while finished(&seam, 3).is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(finished(&seam, 3).unwrap().ran_ms >= ran + 150);
        idle(&seam).await;
        quit(seam, ended).await;
    }

    /// A session continued with `-c` gets the kept notes once, in the
    /// conversation it goes on in, also when they changed while it was
    /// closed. One that never had a note gets no list.
    #[tokio::test]
    async fn a_continued_session_gets_the_kept_notes_once() {
        let temp = tempfile::tempdir().unwrap();
        let (server, bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let id = seam.session.id.clone();
        let lists = |bodies: &Mutex<Vec<String>>| -> Vec<usize> {
            let bodies = bodies.lock().unwrap();
            bodies
                .iter()
                .map(|body| body.matches("# Your kept notes").count())
                .collect()
        };
        seam.input
            .send(Input::Message(app_message("One.")))
            .unwrap();
        answered(&seam, &bodies, 1).await;
        quit(seam, ended).await;
        let (seam, ended) =
            open_app(&root, &project, &server, SessionChoice::Resume(id.clone())).await;
        seam.input
            .send(Input::Message(app_message("Two.")))
            .unwrap();
        answered(&seam, &bodies, 2).await;
        assert_eq!(lists(&bodies), [0, 0]);
        quit(seam, ended).await;
        // An app keeps a note in the closed session's folder.
        let dir = Session::open(&root, &id).unwrap().dir;
        crate::kept_notes::KeptNotes::new(&dir)
            .add(
                "Tear the build box down.",
                crate::kept_notes::Audience::Lead,
            )
            .unwrap();
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id)).await;
        for (count, message) in [(3, "Three."), (4, "Four.")] {
            seam.input
                .send(Input::Message(app_message(message)))
                .unwrap();
            answered(&seam, &bodies, count).await;
        }
        assert_eq!(lists(&bodies), [0, 0, 1, 1]);
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

    /// How long monitors' warnings wake the agent can be set for a new
    /// session, which keeps it, and again as it resumes.
    #[tokio::test]
    async fn monitor_wakes_are_set_for_a_new_session_and_on_resume() {
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        std::fs::create_dir_all(&project).unwrap();
        let open = |session: SessionChoice, wakes: Option<u64>| {
            let running = spawn(
                Options {
                    root: Some(root.clone()),
                    session,
                    workspace: Some(project.clone()),
                    model: Some("luna".into()),
                    openai_auth: Some(OpenAiAuth::ApiKey),
                    cache_1h: Some(false),
                    monitor_wakes: wakes.map(crate::monitors::Wakes::For),
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
        let wakes = |id: &str| {
            Session::open(&root, id)
                .unwrap()
                .load_settings()
                .unwrap()
                .monitor_wakes
        };
        let (seam, ended) = open(SessionChoice::New, Some(2_700)).await;
        let id = seam.session.id.clone();
        assert_eq!(wakes(&id), crate::monitors::Wakes::For(2_700));
        quit(seam, ended).await;
        // A resume without the option keeps the session's.
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), None).await;
        assert_eq!(wakes(&id), crate::monitors::Wakes::For(2_700));
        quit(seam, ended).await;
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), Some(1_800)).await;
        assert_eq!(wakes(&id), crate::monitors::Wakes::For(1_800));
        assert_eq!(
            seam.meter.read().settings.monitor_wakes,
            crate::monitors::Wakes::For(1_800)
        );
        quit(seam, ended).await;
    }

    /// The built-in skills a session leaves out are chosen by its options,
    /// else by ARKO_BUILTIN_SKILLS and ARKO_BUILTIN_SKILLS_OFF in a `.env`
    /// file. The session keeps the choice, and a resume changes it only for
    /// what it names.
    #[tokio::test]
    async fn built_in_skills_are_chosen_for_a_new_session_and_on_resume() {
        use crate::builtin_skills::BuiltinSkills;
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        std::fs::create_dir_all(&project).unwrap();
        let open = |session: SessionChoice, options: Options| {
            let running = spawn(
                Options {
                    root: Some(root.clone()),
                    session,
                    workspace: Some(project.clone()),
                    model: Some("luna".into()),
                    openai_auth: Some(OpenAiAuth::ApiKey),
                    cache_1h: Some(false),
                    client: Some(GivenClient(Arc::new(server.client()))),
                    ..options
                },
                Arc::new(NoPrompter),
            );
            async move {
                let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
                    .await
                    .unwrap()
                    .unwrap();
                (seam, running.ended)
            }
        };
        let choice = |id: &str| {
            Session::open(&root, id)
                .unwrap()
                .load_settings()
                .unwrap()
                .builtin_skills
        };
        let off = |names: &[&str]| BuiltinSkills {
            none: false,
            off: names.iter().map(|name| name.to_string()).collect(),
        };
        // By default a session has them all.
        let (seam, ended) = open(SessionChoice::New, Options::default()).await;
        let seam = seam.unwrap();
        let id = seam.session.id.clone();
        assert_eq!(choice(&id), BuiltinSkills::default());
        assert!(seam.details.skills().unwrap().builtin_off.is_empty());
        quit(seam, ended).await;
        // A `.env` file sets the default of a new session.
        std::fs::write(
            project.join(".env"),
            "ARKO_BUILTIN_SKILLS_OFF=\"repo-docs-style, user-facing-prose\"\n",
        )
        .unwrap();
        let (seam, ended) = open(SessionChoice::New, Options::default()).await;
        let seam = seam.unwrap();
        assert_eq!(
            choice(&seam.session.id),
            off(&["repo-docs-style", "user-facing-prose"])
        );
        assert_eq!(
            seam.details.skills().unwrap().builtin_off,
            ["user-facing-prose", "repo-docs-style"]
        );
        quit(seam, ended).await;
        std::fs::write(
            project.join(".env"),
            "ARKO_BUILTIN_SKILLS_OFF=repo-docs-style,user-facing-prose\n",
        )
        .unwrap();
        let (seam, ended) = open(SessionChoice::New, Options::default()).await;
        let seam = seam.unwrap();
        assert_eq!(
            choice(&seam.session.id),
            off(&["repo-docs-style", "user-facing-prose"])
        );
        quit(seam, ended).await;
        std::fs::write(project.join(".env"), "ARKO_BUILTIN_SKILLS=0\n").unwrap();
        let (seam, ended) = open(SessionChoice::New, Options::default()).await;
        let seam = seam.unwrap();
        let none = BuiltinSkills {
            none: true,
            off: Vec::new(),
        };
        assert_eq!(choice(&seam.session.id), none);
        quit(seam, ended).await;
        // The options come before the file.
        let options = Options {
            builtin_skills: Some(true),
            builtin_skills_off: Some(vec!["repo-docs-style".into()]),
            ..Options::default()
        };
        let (seam, ended) = open(SessionChoice::New, options).await;
        let seam = seam.unwrap();
        let id = seam.session.id.clone();
        assert_eq!(choice(&id), off(&["repo-docs-style"]));
        quit(seam, ended).await;
        // A resume without options keeps the session's, even when the file
        // says otherwise.
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), Options::default()).await;
        assert_eq!(choice(&id), off(&["repo-docs-style"]));
        quit(seam.unwrap(), ended).await;
        // With options, it changes what they name.
        let options = Options {
            builtin_skills_off: Some(vec!["user-facing-prose".into()]),
            ..Options::default()
        };
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), options).await;
        assert_eq!(choice(&id), off(&["user-facing-prose"]));
        quit(seam.unwrap(), ended).await;
        let options = Options {
            builtin_skills: Some(false),
            ..Options::default()
        };
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), options).await;
        let seam = seam.unwrap();
        assert_eq!(
            choice(&id),
            BuiltinSkills {
                none: true,
                off: vec!["user-facing-prose".into()]
            }
        );
        assert_eq!(seam.details.skills().unwrap().builtin_off.len(), 3);
        quit(seam, ended).await;
        let options = Options {
            builtin_skills: Some(true),
            builtin_skills_off: Some(Vec::new()),
            ..Options::default()
        };
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), options).await;
        assert_eq!(choice(&id), BuiltinSkills::default());
        quit(seam.unwrap(), ended).await;
    }

    /// A name that isn't a built-in skill is skipped with a warning, and the
    /// session opens with the names around it left out.
    #[tokio::test]
    async fn a_built_in_skill_that_does_not_exist_is_skipped_with_a_warning() {
        struct Hearing(Mutex<Vec<String>>);
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
            fn note(&self, _level: crate::remote::Level, text: &str) {
                crate::lock(&self.0).push(text.to_string());
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let hearing = Arc::new(Hearing(Mutex::default()));
        let running = spawn(
            Options {
                root: Some(temp.path().join("arko")),
                workspace: Some(project),
                model: Some("luna".into()),
                openai_auth: Some(OpenAiAuth::ApiKey),
                cache_1h: Some(false),
                builtin_skills_off: Some(vec!["repo-doc-style".into(), "user-facing-prose".into()]),
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            },
            hearing.clone(),
        );
        let seam = tokio::time::timeout(Duration::from_secs(30), running.opened)
            .await
            .unwrap()
            .unwrap()
            .expect("the session opens");
        let saved = Session::open(&temp.path().join("arko"), &seam.session.id)
            .unwrap()
            .load_settings()
            .unwrap();
        assert_eq!(saved.builtin_skills.off, ["user-facing-prose"]);
        let heard = crate::lock(&hearing.0).clone();
        assert!(
            heard.iter().any(|note| note.starts_with(
                "builtin_skills_off names `repo-doc-style`, which isn't a built-in skill"
            )),
            "{heard:?}"
        );
        quit(seam, running.ended).await;
    }

    /// Whether the agent that started a worker gets a copy of the user's
    /// messages to it can be set for a new session, which keeps it, and again
    /// as it resumes. A new session has copies off, and a resume without the
    /// option keeps the session's.
    #[tokio::test]
    async fn message_copies_are_set_for_a_new_session_and_on_resume() {
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        std::fs::create_dir_all(&project).unwrap();
        let open = |session: SessionChoice, copies: Option<bool>| {
            let running = spawn(
                Options {
                    root: Some(root.clone()),
                    session,
                    workspace: Some(project.clone()),
                    model: Some("luna".into()),
                    openai_auth: Some(OpenAiAuth::ApiKey),
                    cache_1h: Some(false),
                    message_copies: copies,
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
        let saved = |id: &str| {
            Session::open(&root, id)
                .unwrap()
                .load_settings()
                .unwrap()
                .message_copies
        };
        let (seam, ended) = open(SessionChoice::New, None).await;
        let id = seam.session.id.clone();
        assert!(!saved(&id), "a new session has copies off");
        assert!(!seam.meter.read().settings.message_copies);
        quit(seam, ended).await;
        // A resume with the option turns them on and saves that.
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), Some(true)).await;
        assert!(saved(&id));
        assert!(seam.meter.read().settings.message_copies);
        quit(seam, ended).await;
        // Without it, a resume keeps the session's.
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), None).await;
        assert!(saved(&id));
        assert!(seam.meter.read().settings.message_copies);
        quit(seam, ended).await;
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), Some(false)).await;
        assert!(!saved(&id));
        assert!(!seam.meter.read().settings.message_copies);
        quit(seam, ended).await;
        // A new session takes it too.
        let (seam, ended) = open(SessionChoice::New, Some(true)).await;
        assert!(saved(&seam.session.id));
        quit(seam, ended).await;
    }

    /// Who chooses the workers' models and how deep workers go can be set
    /// for a new session, which keeps them, and again as it resumes.
    #[tokio::test]
    async fn worker_settings_are_set_for_a_new_session_and_on_resume() {
        use crate::agents::ModelChoice;
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        std::fs::create_dir_all(&project).unwrap();
        let try_open = |session: SessionChoice, choice: Option<ModelChoice>, depth: Option<u32>| {
            let running = spawn(
                Options {
                    root: Some(root.clone()),
                    session,
                    workspace: Some(project.clone()),
                    model: Some("luna".into()),
                    openai_auth: Some(OpenAiAuth::ApiKey),
                    cache_1h: Some(false),
                    worker_models: choice,
                    worker_depth: depth,
                    client: Some(GivenClient(Arc::new(server.client()))),
                    ..Options::default()
                },
                Arc::new(NoPrompter),
            );
            async move {
                let opened = tokio::time::timeout(Duration::from_secs(30), running.opened)
                    .await
                    .unwrap()
                    .unwrap();
                (opened, running.ended)
            }
        };
        let open = |session: SessionChoice, choice: Option<ModelChoice>, depth: Option<u32>| {
            let opening = try_open(session, choice, depth);
            async move {
                let (opened, ended) = opening.await;
                (opened.unwrap(), ended)
            }
        };
        let saved = |id: &str| {
            let settings = Session::open(&root, id).unwrap().load_settings().unwrap();
            (settings.worker_models, settings.worker_depth)
        };
        let (seam, ended) = open(SessionChoice::New, None, None).await;
        let id = seam.session.id.clone();
        assert_eq!(saved(&id), (ModelChoice::Agent, 2));
        let live = seam.meter.read().settings;
        assert_eq!(
            (live.worker_models, live.worker_depth),
            (ModelChoice::Agent, 2)
        );
        quit(seam, ended).await;
        // A resume without the options keeps the session's, and with them
        // changes and saves them.
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), None, None).await;
        assert_eq!(saved(&id), (ModelChoice::Agent, 2));
        quit(seam, ended).await;
        let (seam, ended) = open(
            SessionChoice::Resume(id.clone()),
            Some(ModelChoice::Profiles),
            Some(3),
        )
        .await;
        assert_eq!(saved(&id), (ModelChoice::Profiles, 3));
        let live = seam.meter.read().settings;
        assert_eq!(
            (live.worker_models, live.worker_depth),
            (ModelChoice::Profiles, 3)
        );
        quit(seam, ended).await;
        // A new session takes them too.
        let (seam, ended) = open(SessionChoice::New, Some(ModelChoice::Session), Some(1)).await;
        assert_eq!(saved(&seam.session.id), (ModelChoice::Session, 1));
        let id = seam.session.id.clone();
        quit(seam, ended).await;
        // A depth outside 1 to 4 (an app can pass one) opens nothing, new or
        // resumed, and leaves the saved depth as it was.
        for depth in [0, 5, 99] {
            for session in [SessionChoice::New, SessionChoice::Resume(id.clone())] {
                let (opened, _ended) = try_open(session, None, Some(depth)).await;
                match opened {
                    Err(OpenError::Failed(why)) => assert_eq!(
                        why,
                        format!("the worker depth is a number from 1 to 4, not {depth}")
                    ),
                    Err(_) => panic!("expected a refusal of the depth"),
                    Ok(_) => panic!("a depth of {depth} opened a session"),
                }
            }
        }
        assert_eq!(saved(&id), (ModelChoice::Session, 1));
    }

    /// The worker retry cap can be set for a new session, which keeps it, and
    /// again as it resumes. A new session has none, and a resume without the
    /// option keeps the session's. A cap of 0 opens nothing.
    #[tokio::test]
    async fn the_worker_retry_cap_is_set_for_a_new_session_and_on_resume() {
        let temp = tempfile::tempdir().unwrap();
        let (server, _bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        std::fs::create_dir_all(&project).unwrap();
        let try_open = |session: SessionChoice, cap: Option<u32>| {
            let running = spawn(
                Options {
                    root: Some(root.clone()),
                    session,
                    workspace: Some(project.clone()),
                    model: Some("luna".into()),
                    openai_auth: Some(OpenAiAuth::ApiKey),
                    cache_1h: Some(false),
                    worker_retry_cap: cap,
                    client: Some(GivenClient(Arc::new(server.client()))),
                    ..Options::default()
                },
                Arc::new(NoPrompter),
            );
            async move {
                let opened = tokio::time::timeout(Duration::from_secs(30), running.opened)
                    .await
                    .unwrap()
                    .unwrap();
                (opened, running.ended)
            }
        };
        let open = |session: SessionChoice, cap: Option<u32>| {
            let opening = try_open(session, cap);
            async move {
                let (opened, ended) = opening.await;
                (opened.unwrap(), ended)
            }
        };
        let saved = |id: &str| {
            Session::open(&root, id)
                .unwrap()
                .load_settings()
                .unwrap()
                .worker_retry_cap
        };
        let (seam, ended) = open(SessionChoice::New, None).await;
        let id = seam.session.id.clone();
        assert_eq!(saved(&id), None);
        assert_eq!(seam.meter.read().settings.worker_retry_cap, None);
        quit(seam, ended).await;
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), Some(4)).await;
        assert_eq!(saved(&id), Some(4));
        assert_eq!(seam.meter.read().settings.worker_retry_cap, Some(4));
        quit(seam, ended).await;
        let (seam, ended) = open(SessionChoice::Resume(id.clone()), None).await;
        assert_eq!(saved(&id), Some(4));
        quit(seam, ended).await;
        let (seam, ended) = open(SessionChoice::New, Some(2)).await;
        assert_eq!(saved(&seam.session.id), Some(2));
        quit(seam, ended).await;
        for session in [SessionChoice::New, SessionChoice::Resume(id.clone())] {
            let (opened, _ended) = try_open(session, Some(0)).await;
            assert!(
                matches!(opened, Err(OpenError::Failed(why)) if why == "the worker retry cap is a number from 1, not 0")
            );
        }
        assert_eq!(saved(&id), Some(4));
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
        let start = |name: &str, shell: Option<&str>| {
            let workspace = temp.path().join(name);
            std::fs::create_dir_all(&workspace).unwrap();
            if let Some(shell) = shell {
                std::fs::write(workspace.join(".env"), format!("ARKO_SHELL={shell}\n")).unwrap();
            }
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
        // (The first sets no shell, so it gets the default.)
        let (first, second) = (start("first", None), start("second", Some("cmd")));
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
                        serde_json::json!({"questions": "Tabs or spaces?", "wait": true}),
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
                answers: Vec::new(),
                continue_uncached: false,
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
                    answers: Vec::new(),
                    continue_uncached: false,
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
                // (The tests' workers run on models billed per token.)
                worker_billing: Some(crate::billing::WorkerBilling::Api),
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
            answers: Vec::new(),
            continue_uncached: false,
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

    /// A session whose workers started workers lists them under their
    /// starters when it opens again: each worker's place in the family, what
    /// each cost before, and what a family cost together.
    #[tokio::test]
    async fn a_resumed_session_lists_its_workers_under_their_starters() {
        use crate::agents::WORKER;
        let server = ModelServer::start(Arc::new(|index, body: String| {
            let outputs = serde_json::from_str::<serde_json::Value>(&body).unwrap()["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .count();
            Box::pin(async move {
                let id = format!("a{index}");
                let response = if body.contains("You are agent run r2,") {
                    complete(&id, "Counted 41 files.")
                } else if body.contains("You are agent run r1,") {
                    match outputs {
                        0 => tool_call(
                            &id,
                            crew::START_AGENT,
                            serde_json::json!({"agent": WORKER, "task": "Count the files."}),
                        ),
                        1 => tool_call(&id, crew::WAIT_FOR_AGENTS, serde_json::json!({})),
                        _ => complete(&id, "Reviewed."),
                    }
                } else if outputs == 0 {
                    tool_call(
                        &id,
                        crew::START_AGENT,
                        serde_json::json!({"agent": WORKER, "task": "Review it.", "nested": true}),
                    )
                } else {
                    reply(&id, "Started.")
                };
                if body.contains("You are agent run") {
                    with_input(response, 50_000)
                } else {
                    response
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let id = seam.session.id.clone();
        seam.input
            .send(Input::Message(app_message("Review the files.")))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let workers = seam.feed.worker_summaries();
                if workers.len() == 2
                    && workers
                        .iter()
                        .all(|worker| worker.state == remote::WorkerState::Finished)
                    && !seam.feed.activity().working
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the workers finished");
        let before = seam.feed.worker_summaries();
        quit(seam, ended).await;

        // A new process: its feed knows the workers from the saved runs.
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id)).await;
        let workers = seam.feed.worker_summaries();
        let family: Vec<_> = workers
            .iter()
            .map(|worker| (worker.id.as_str(), worker.parent.as_deref(), worker.depth))
            .collect();
        assert_eq!(family, [("r1", None, 1), ("r2", Some("r1"), 2)]);
        let (r1, r2) = (&workers[0], &workers[1]);
        assert!(r1.cost_billed > 0.0 && r2.cost_billed > 0.0, "{workers:?}");
        for (restored, ran) in workers.iter().zip(&before) {
            assert!((restored.cost_billed - ran.cost_billed).abs() < 1e-9);
        }
        assert!((r1.family_cost_billed - (r1.cost_billed + r2.cost_billed)).abs() < 1e-9);
        assert!((r2.family_cost_billed - r2.cost_billed).abs() < 1e-9);
        quit(seam, ended).await;
    }

    async fn quit(seam: Seam, ended: oneshot::Receiver<RunEnd>) {
        seam.input.send(Input::Quit(Some(Vec::new()))).unwrap();
        let end = tokio::time::timeout(Duration::from_secs(30), ended).await;
        assert_eq!(end.unwrap().unwrap(), RunEnd::Quit);
    }

    /// A session an app runs is named by a model after it closes too: its
    /// thread keeps its runtime for the model that is still working, so the
    /// name comes and is kept for the next time the session is listed or
    /// opened. A session deleted by then gets none.
    #[tokio::test]
    async fn a_model_s_name_that_comes_after_the_session_closed_is_kept() {
        use crate::session_names::Way;
        use crate::session_names::fake::{Fake, Step, name_with};
        for deleted in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let (server, bodies) = counting_server().await;
            let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
            crate::session_names::save(&root, &[Way::ClaudePlan]).unwrap();
            let gate = Arc::new(tokio::sync::Notify::new());
            let fake = Fake::new(&[(Way::ClaudePlan, Step::Gated(gate.clone(), "Pear count"))]);
            name_with(&root, fake.clone());
            let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
            let id = seam.session.id.clone();
            seam.input
                .send(Input::Message(app_message("Count the pears.")))
                .unwrap();
            answered(&seam, &bodies, 1).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                while fake.ways_asked().is_empty() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            quit(seam, ended).await;
            let title = || {
                crate::sessions::list(&root)
                    .into_iter()
                    .next()
                    .map(|entry| entry.title)
            };
            assert_eq!(title().as_deref(), Some("Count the pears."));
            if deleted {
                crate::sessions::delete(&root, &id).unwrap();
            }
            // The model answers after the session closed.
            gate.notify_one();
            if deleted {
                tokio::time::sleep(Duration::from_millis(500)).await;
                assert!(!root.join("sessions").join(&id).exists());
            } else {
                tokio::time::timeout(Duration::from_secs(10), async {
                    while title().as_deref() != Some("Pear count") {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("the name came after the close");
            }
        }
    }

    /// The first message of a session is edited after the session was closed
    /// and opened again, while the first run of its model is still going on
    /// in the first session's thread. That run never names the session,
    /// whichever answer comes first, and a replacement that fails leaves the
    /// first words.
    #[tokio::test]
    async fn an_edit_after_reopening_ends_the_naming_that_went_on_from_before() {
        use crate::session_names::Way;
        use crate::session_names::fake::{Fake, Step, name_with};
        for replacement_fails in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let (server, bodies) = counting_server().await;
            let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
            crate::session_names::save(&root, &[Way::ClaudePlan]).unwrap();
            let (old, new) = (
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(tokio::sync::Notify::new()),
            );
            // The first ask is for the first words, the second for the edit.
            let fake = Fake::new(&[(
                Way::ClaudePlan,
                if replacement_fails {
                    Step::GatedFail(new.clone())
                } else {
                    Step::Gated(new.clone(), "New name")
                },
            )]);
            fake.next(Way::ClaudePlan, Step::Gated(old.clone(), "Old name"));
            name_with(&root, fake.clone());
            let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
            let id = seam.session.id.clone();
            seam.input
                .send(Input::Message(app_message("Count the pears.")))
                .unwrap();
            answered(&seam, &bodies, 1).await;
            wait_for(|| fake.ways_asked().len() == 1).await;
            // The session closes and opens again. Its first run goes on.
            quit(seam, ended).await;
            let (seam, ended) = open_app(&root, &project, &server, SessionChoice::Resume(id)).await;
            resumed(&seam).await;
            let first = user_item(&seam, "Count the pears.");
            assert_eq!(edit(&seam, first.id, "Count the apples.").await, Ok(()));
            answered(&seam, &bodies, 2).await;
            wait_for(|| fake.ways_asked().len() == 2).await;
            let title = || crate::sessions::list(&root).remove(0).title;
            // The replacement answers, or fails, and then the old run does.
            new.notify_one();
            if replacement_fails {
                tokio::time::sleep(Duration::from_millis(300)).await;
            } else {
                wait_for(|| title() == "New name").await;
            }
            old.notify_one();
            // The first run is still there to be answered.
            wait_for(|| {
                fake.given
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|name| name == "Old name")
            })
            .await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let expected = if replacement_fails {
                "Count the apples."
            } else {
                "New name"
            };
            assert_eq!(title(), expected, "replacement fails: {replacement_fails}");
            quit(seam, ended).await;
            assert_eq!(title(), expected);
        }
    }

    /// A session that closed and was deleted while its model works is asked
    /// no more ways, a way that bills included.
    #[tokio::test]
    async fn a_closed_session_that_is_deleted_is_asked_no_more_ways() {
        use crate::session_names::Way;
        use crate::session_names::fake::{Fake, Step, name_with};
        let temp = tempfile::tempdir().unwrap();
        let (server, bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        crate::session_names::save(&root, &[Way::ClaudePlan, Way::ClaudeKey]).unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        let fake = Fake::new(&[
            (Way::ClaudePlan, Step::GatedFail(gate.clone())),
            (Way::ClaudeKey, Step::Answer("Paid name")),
        ]);
        name_with(&root, fake.clone());
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let id = seam.session.id.clone();
        seam.input
            .send(Input::Message(app_message("Count the pears.")))
            .unwrap();
        answered(&seam, &bodies, 1).await;
        wait_for(|| fake.ways_asked().len() == 1).await;
        quit(seam, ended).await;
        crate::sessions::delete(&root, &id).unwrap();
        gate.notify_one();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            fake.ways_asked(),
            [Way::ClaudePlan],
            "no way for a deleted session"
        );
        assert!(!root.join("sessions").join(&id).exists());
    }

    /// Waits up to 10 s for `done`.
    async fn wait_for(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("it didn't happen in time");
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
                answers: Vec::new(),
                continue_uncached: false,
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
        // The profiles the session saved go with the branch.
        let profiles = root.join("sessions").join(&id).join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::write(
            profiles.join("reviewer.md"),
            "---\ndescription: Reviews\n---\n",
        )
        .unwrap();
        let saved = crate::sessions::branch(&root, &id).unwrap();
        assert_eq!(
            std::fs::read_to_string(
                root.join("sessions")
                    .join(&saved)
                    .join("profiles")
                    .join("reviewer.md")
            )
            .unwrap(),
            "---\ndescription: Reviews\n---\n"
        );
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
                    answers: Vec::new(),
                    continue_uncached: false,
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
                answers: Vec::new(),
                continue_uncached: false,
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

    /// A stub MCP server with one tool, `ping`. It appends `start` to the file
    /// `STUB_LOG` names as it starts, and `stop` as its input closes.
    const MCP_STUB: &str = r#"
import json, os, sys
def note(text):
    with open(os.environ["STUB_LOG"], "a") as log:
        log.write(text + "\n")
def reply(id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id, "result": result}) + "\n")
    sys.stdout.flush()
note("start")
for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message.get("method")
    if method == "initialize":
        reply(message["id"], {"protocolVersion": message["params"]["protocolVersion"],
              "capabilities": {"tools": {}}, "serverInfo": {"name": "stub", "version": "1"}})
    elif method == "tools/list":
        reply(message["id"], {"tools": [{"name": "ping", "description": "Answers pong.",
              "inputSchema": {"type": "object", "properties": {}}}]})
    else:
        reply(message["id"], {"content": [{"type": "text", "text": "pong"}]})
note("stop")
"#;

    /// A chat starts no MCP servers and doesn't ask about the project's
    /// `.mcp.json`, also when it was an agent's, whose conversations offered
    /// the servers' tools. An agent's session starts them as before, and
    /// each switch to Agent starts them again.
    #[tokio::test]
    async fn a_chat_starts_no_mcp_servers() {
        /// Counts the questions about the project's servers.
        struct Counting(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Prompter for Counting {
            async fn need_credentials(&self, need: &credentials::Need) -> Result<(), String> {
                Err(need.message())
            }
            async fn approve_project_mcp(
                &self,
                _approval: &crate::prompter::McpApproval,
            ) -> Result<Option<bool>, String> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(None)
            }
            fn note(&self, _level: crate::remote::Level, _text: &str) {}
        }
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body);
                Box::pin(async move { reply("a", "Ahoy.") })
            }
        }))
        .await;
        let root = temp.path().join("arko");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(mcp::PROJECT_FILE),
            r#"{"mcpServers": {"project": {"command": "arko-no-such-mcp-server"}}}"#,
        )
        .unwrap();
        let script = temp.path().join("stub.py");
        std::fs::write(&script, MCP_STUB).unwrap();
        let log = temp.path().join("stub.log");
        let python = std::env::var("ARKO_TEST_PYTHON")
            .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.into());
        let config = temp.path().join("mcp.json");
        std::fs::write(
            &config,
            serde_json::json!({"mcpServers": {"stub": {
                "command": python,
                "args": [script],
                "env": {"STUB_LOG": log},
            }}})
            .to_string(),
        )
        .unwrap();
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let asked_now = || asked.load(std::sync::atomic::Ordering::SeqCst);
        // How many times the stub started, and how many times it stopped.
        let events = |event: &str| {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == event)
                .count()
        };
        let wait = Duration::from_secs(30);
        let open = |session: SessionChoice, mode: crate::session::Mode| {
            let running = spawn(
                Options {
                    root: Some(root.clone()),
                    session,
                    mode,
                    workspace: Some(project.clone()),
                    mcp_config: vec![config.clone()],
                    model: Some("luna".into()),
                    openai_auth: Some(OpenAiAuth::ApiKey),
                    cache_1h: Some(false),
                    text_replies: Some(true),
                    client: Some(GivenClient(Arc::new(server.client()))),
                    ..Options::default()
                },
                Arc::new(Counting(asked.clone())),
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
        let ask = |seam: &Seam, text: &str, count: usize| {
            seam.input.send(Input::Message(app_message(text))).unwrap();
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
        let stopped = |count: usize| async move {
            tokio::time::timeout(wait, async {
                while events("stop") < count {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
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
        let offers_mcp = |body: &str| offered(body).contains(&"mcp".to_string());

        // An agent's session starts the server and offers `mcp`.
        let (seam, ended) = open(SessionChoice::New, crate::session::Mode::Agent).await;
        let id = seam.session.id.clone();
        assert_eq!((events("start"), asked_now()), (1, 1));
        assert_eq!(seam.details.mcp().servers.len(), 1);
        assert!(offers_mcp(&ask(&seam, "First.", 1).await));
        assert_eq!(switch(&seam, crate::session::Mode::Chat).await, Ok(()));
        assert_eq!(
            tokio::time::timeout(wait, ended).await.unwrap().unwrap(),
            RunEnd::Reopen
        );
        stopped(1).await;

        // As a chat it starts nothing and asks nothing, and its first
        // conversation, the agent's, goes on without tools.
        let (seam, ended) = open(
            SessionChoice::Resume(id.clone()),
            crate::session::Mode::Chat,
        )
        .await;
        assert_eq!((events("start"), asked_now()), (1, 1));
        let mcp = seam.details.mcp();
        assert!(mcp.servers.is_empty());
        assert_eq!(mcp.notes, [crate::details::CHAT_MCP_NOTE]);
        // `/mcp` says why, and not that no servers are set up.
        seam.input
            .send(Input::Message(app_message("/mcp")))
            .unwrap();
        let said = |seam: &Seam| {
            seam.feed
                .subscribe()
                .0
                .items
                .iter()
                .map(|item| item.text.clone())
                .collect::<Vec<_>>()
        };
        tokio::time::timeout(wait, async {
            while !said(&seam).contains(&crate::details::CHAT_MCP_NOTE.to_string()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !said(&seam)
                .iter()
                .any(|text| text.contains("No MCP servers"))
        );
        let chat = ask(&seam, "Second.", 2).await;
        assert!(!offers_mcp(&chat) && chat.contains("First."), "{chat}");
        assert_eq!(switch(&seam, crate::session::Mode::Agent).await, Ok(()));
        assert_eq!(
            tokio::time::timeout(wait, ended).await.unwrap().unwrap(),
            RunEnd::Reopen
        );

        // Back to Agent, it starts the server again.
        let (seam, ended) = open(SessionChoice::Resume(id), crate::session::Mode::Agent).await;
        assert_eq!((events("start"), asked_now()), (2, 2));
        assert!(
            !seam
                .details
                .mcp()
                .notes
                .contains(&crate::details::CHAT_MCP_NOTE.to_string())
        );
        assert!(offers_mcp(&ask(&seam, "Third.", 3).await));
        seam.input.send(Input::Quit(Some(Vec::new()))).unwrap();
        assert_eq!(
            tokio::time::timeout(wait, ended).await.unwrap().unwrap(),
            RunEnd::Quit
        );
        stopped(2).await;

        // A chat that never was an agent's starts none either.
        let (seam, ended) = open(SessionChoice::New, crate::session::Mode::Chat).await;
        assert_eq!((events("start"), asked_now()), (2, 2));
        assert!(!offers_mcp(&ask(&seam, "Hello.", 4).await));
        quit(seam, ended).await;
    }

    /// A session whose earlier runs offered an MCP server's tools (as an
    /// Arko before `mcp` saved them) opens when the server is no longer set
    /// up or fails to start, also when it was switched between Chat and
    /// Agent or branched, which keep their runs' configurations. A server
    /// that fails to start is reported. A run that offered the tool can
    /// still call it and is told the server isn't running, and new
    /// conversations are not offered it.
    #[tokio::test]
    async fn a_session_opens_without_the_servers_its_runs_offered() {
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body.clone());
                Box::pin(async move {
                    if body.contains("Call ping.") && !body.contains("isn't running") {
                        tool_call("c", "mcp__stub__ping", serde_json::json!({}))
                    } else {
                        reply("a", "Ahoy.")
                    }
                })
            }
        }))
        .await;
        let root = temp.path().join("arko");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let script = temp.path().join("stub.py");
        std::fs::write(&script, MCP_STUB).unwrap();
        let log = temp.path().join("stub.log");
        let python = std::env::var("ARKO_TEST_PYTHON")
            .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.into());
        let config = |name: &str, command: &str| {
            let file = temp.path().join(name);
            std::fs::write(
                &file,
                serde_json::json!({"mcpServers": {"stub": {
                    "command": command,
                    "args": [script],
                    "env": {"STUB_LOG": log},
                }}})
                .to_string(),
            )
            .unwrap();
            file
        };
        let running = config("running.json", &python);
        let failing = config("failing.json", "arko-no-such-mcp-server");
        let wait = Duration::from_secs(30);
        let open = |session: SessionChoice, mode: crate::session::Mode, files: Vec<PathBuf>| {
            let running = spawn(
                Options {
                    root: Some(root.clone()),
                    session,
                    mode,
                    workspace: Some(project.clone()),
                    mcp_config: files,
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
                let opened = tokio::time::timeout(wait, running.opened)
                    .await
                    .unwrap()
                    .unwrap();
                (opened, running.ended)
            }
        };
        let ask = |seam: &Seam, text: &str| {
            let before = bodies.lock().unwrap().len();
            seam.input.send(Input::Message(app_message(text))).unwrap();
            let feed = seam.feed.clone();
            let bodies = bodies.clone();
            async move {
                tokio::time::timeout(wait, async {
                    while bodies.lock().unwrap().len() <= before || feed.activity().working {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                bodies.lock().unwrap().last().unwrap().clone()
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
        let ping = "mcp__stub__ping";
        use crate::session::Mode::{Agent, Chat};

        // An agent's session, one switched to Chat and back, and a branch of
        // the first, each with a run that offered the server's tool.
        let (opened, ended) = open(SessionChoice::New, Agent, vec![running.clone()]).await;
        let seam = opened.unwrap();
        let plain = seam.session.id.clone();
        assert!(offered(&ask(&seam, "First.").await).contains(&"mcp".to_string()));
        quit(seam, ended).await;
        let (opened, ended) = open(SessionChoice::New, Agent, vec![running.clone()]).await;
        let seam = opened.unwrap();
        let switched = seam.session.id.clone();
        assert!(offered(&ask(&seam, "First.").await).contains(&"mcp".to_string()));
        assert_eq!(switch(&seam, Chat).await, Ok(()));
        tokio::time::timeout(wait, ended).await.unwrap().unwrap();
        let (opened, ended) = open(SessionChoice::Resume(switched.clone()), Chat, Vec::new()).await;
        let seam = opened.unwrap();
        assert_eq!(switch(&seam, Agent).await, Ok(()));
        tokio::time::timeout(wait, ended).await.unwrap().unwrap();
        for session in [&plain, &switched] {
            to_direct_tools(&Session::open(&root, session).unwrap().dir, ping);
        }
        let branched = crate::sessions::branch(&root, &plain).unwrap();

        // The server is gone from the configuration, or fails to start.
        let mut refused = Vec::new();
        for (session, name) in [
            (&plain, "plain"),
            (&switched, "switched"),
            (&branched, "branched"),
        ] {
            for (files, why) in [(Vec::new(), "removed"), (vec![failing.clone()], "failing")] {
                let configured = files.len();
                let (opened, ended) =
                    open(SessionChoice::Resume(session.clone()), Agent, files).await;
                match opened {
                    Ok(seam) => {
                        let servers = seam.details.mcp().servers;
                        assert_eq!(servers.len(), configured, "{name} {why}");
                        assert!(servers.iter().all(|server| server.tools.is_err()));
                        assert!(ask(&seam, "Again.").await.contains("Again."));
                        // A call to its tool says the server isn't running.
                        if (name, why) == ("switched", "removed") {
                            let called = ask(&seam, "Call ping.").await;
                            assert!(
                                called.contains(
                                    "The MCP server stub isn't running. It's no longer set up, or it failed to start."
                                ),
                                "{called}"
                            );
                        }
                        // A new conversation is not offered the tool.
                        seam.input
                            .send(Input::Message(app_message("/new")))
                            .unwrap();
                        let new = ask(&seam, "Newly.").await;
                        assert!(!new.contains(ping), "{name} {why}");
                        quit(seam, ended).await;
                    }
                    Err(error) => refused.push(format!("{name} {why}: {error}")),
                }
            }
        }
        assert!(refused.is_empty(), "{refused:#?}");
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

    /// The user answers one of two questions the agent waited for: the
    /// other stays open, and the fresh conversation of the handover that
    /// follows restates it, without the one answered.
    #[tokio::test]
    async fn a_handover_after_a_partial_answer_restates_the_question_left() {
        use arko_agents::builtins::ASK_USER_TOOL_NAME;
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body.clone());
                Box::pin(async move {
                    if body.contains("# Your open questions") {
                        complete("d", "Done after the handover.")
                    } else if body.contains("Call `handover_report`") {
                        tool_call(
                            "r",
                            arko_agents::handover::HANDOVER_REPORT_TOOL_NAME,
                            serde_json::json!({"report": "Asked two questions."}),
                        )
                    } else if body.contains("Still open: q2.") {
                        with_input(
                            tool_call(
                                "m",
                                crate::message::MESSAGE_USER,
                                serde_json::json!({"message": "Going on."}),
                            ),
                            40_000,
                        )
                    } else {
                        tool_call(
                            "a",
                            ASK_USER_TOOL_NAME,
                            serde_json::json!({
                                "questions": ["Tabs or spaces?", "Push now?"],
                                "wait": true
                            }),
                        )
                    }
                })
            }
        }))
        .await;
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let running = spawn(
            Options {
                root: Some(temp.path().join("arko")),
                workspace: Some(project),
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
        seam.input
            .send(Input::Message(app_message("Format the code.")))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while seam.feed.open_questions().len() < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        seam.input
            .send(Input::AnswerQuestions(crate::questions::Answers {
                answers: vec![crate::questions::QuestionAnswer::new(
                    "q1",
                    crate::questions::Answer {
                        text: Some("Spaces".into()),
                        ..Default::default()
                    },
                )],
            }))
            .unwrap();
        let fresh = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let found = bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|body| body.contains("# Your open questions"))
                    .cloned();
                if let Some(found) = found {
                    return found;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(fresh.contains("q2 Push now?"), "{fresh}");
        assert!(!fresh.contains("q1 Tabs or spaces?"), "{fresh}");
        let open = seam.feed.open_questions();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "q2");
        quit(seam, running.ended).await;
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
                    answers: Vec::new(),
                    continue_uncached: false,
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
                answers: Vec::new(),
                continue_uncached: false,
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

    /// With -p, the agents have no monitors, since no one would hear their
    /// alerts. The session continued without -p keeps its agents'
    /// configuration, as it keeps their way of replying, and its next
    /// message says it has no monitors.
    #[tokio::test]
    async fn a_session_started_with_p_has_no_monitors() {
        let _alone = ALONE_TESTS.lock().await;
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body);
                Box::pin(async { complete("a1", "Hello.") })
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
        for one_shot in [true, false] {
            let options = Options {
                root: Some(root.clone()),
                session: SessionChoice::Resume(session.id.clone()),
                one_shot,
                interactive: false,
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            };
            let (mut engine, seam) = match Engine::open(options, Arc::new(NoPrompter)).await {
                Ok(open) => open,
                Err(error) => panic!("{error}"),
            };
            engine.front_end(FrontEnd::default());
            assert_eq!(
                engine.run_prompt("Say hello.".into()).await,
                RunEnd::Answered
            );
            engine.shutdown().await;
            drop(seam);
            // (Its tasks let the session go as they end.)
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2);
        for body in &bodies {
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            let offered: Vec<&str> = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(offered.contains(&"terminal_start"), "{offered:?}");
            for tool in arko_agents::terminal::MonitorTool::NAMES {
                assert!(!offered.contains(&tool), "{tool}");
            }
            assert!(
                !body["instructions"]
                    .as_str()
                    .unwrap()
                    .contains("monitor_start")
            );
        }
        assert!(
            bodies[1].contains("This session's saved conversations and workers started without monitors (monitor_start), so they get them only from their next handover (those started after /model changed the model, or by /new continue, keep theirs). New conversations (/new) and new workers have them from the start."),
            "{}",
            bodies[1]
        );
    }

    /// A session with monitors, continued with -p, which gives none, goes on
    /// with its conversation's tools as they were, byte for byte, the
    /// monitors among them.
    #[tokio::test]
    async fn a_session_continued_with_p_keeps_its_monitors_definitions() {
        let _alone = ALONE_TESTS.lock().await;
        let temp = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = ModelServer::start(Arc::new({
            let bodies = bodies.clone();
            move |_, body: String| {
                bodies.lock().unwrap().push(body);
                Box::pin(async { complete("a1", "Hello.") })
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
        for one_shot in [false, true] {
            let options = Options {
                root: Some(root.clone()),
                session: SessionChoice::Resume(session.id.clone()),
                one_shot,
                interactive: false,
                client: Some(GivenClient(Arc::new(server.client()))),
                ..Options::default()
            };
            let (mut engine, seam) = match Engine::open(options, Arc::new(NoPrompter)).await {
                Ok(open) => open,
                Err(error) => panic!("{error}"),
            };
            engine.front_end(FrontEnd::default());
            assert_eq!(
                engine.run_prompt("Say hello.".into()).await,
                RunEnd::Answered
            );
            engine.shutdown().await;
            drop(seam);
            // (Its tasks let the session go as they end.)
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2);
        let tools = |body: &str| {
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            body["tools"].to_string()
        };
        assert!(tools(&bodies[0]).contains("\"monitor_start\""));
        // A session with workers offers monitors that last while they run.
        assert!(tools(&bodies[0]).contains("while_workers_run"));
        assert_eq!(tools(&bodies[1]), tools(&bodies[0]));
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

    include!("cache_tests.rs");
    include!("mcp_engine_tests.rs");

    /// What an app sends with `Seam::send` comes back as its verdict: a
    /// message as the turn it starts, a command that doesn't exist as
    /// refused, and a quit as read. Once the session closed, a send is
    /// refused as closed.
    #[tokio::test]
    async fn an_apps_sends_get_their_verdicts_until_the_session_closes() {
        use crate::{RefusalKind, Verdict};
        let temp = tempfile::tempdir().unwrap();
        let (server, bodies) = counting_server().await;
        let (root, project) = (temp.path().join("arko"), temp.path().join("project"));
        let (seam, ended) = open_app(&root, &project, &server, SessionChoice::New).await;
        let wait = |verdict: oneshot::Receiver<Verdict>| async move {
            tokio::time::timeout(Duration::from_secs(30), verdict)
                .await
                .expect("a verdict in time")
                .expect("a verdict")
        };
        let started = wait(seam.send(Input::Message(app_message("Count the pears.")))).await;
        assert!(
            matches!(
                started,
                Verdict::Accepted {
                    delivery: crate::remote::Delivery::Turn,
                    item: Some(_),
                    ..
                }
            ),
            "{started:?}"
        );
        answered(&seam, &bodies, 1).await;
        let refused = wait(seam.send(Input::Message(app_message("/nosuch")))).await;
        assert!(
            matches!(&refused, Verdict::Refused(refusal) if refusal.kind == RefusalKind::UnknownCommand),
            "{refused:?}"
        );
        let quit = wait(seam.send(Input::Quit(Some(Vec::new())))).await;
        assert!(matches!(quit, Verdict::Accepted { .. }), "{quit:?}");
        let end = tokio::time::timeout(Duration::from_secs(30), ended).await;
        assert_eq!(end.unwrap().unwrap(), RunEnd::Quit);
        let late = wait(seam.send(Input::Message(app_message("Late.")))).await;
        assert_eq!(late, Verdict::closed());
    }
}

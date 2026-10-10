//! fin: a general-purpose agent for the terminal.
//!
//! The user talks with one agent (`main`), which works with a shell and
//! background terminal jobs in the working directory and can hand tasks to
//! workers that run at the same time. Each message is a turn of one
//! conversation: the first runs the agent, later ones continue it. Ctrl-C
//! stops a turn (the workflow is stopped, then rebuilt from its checkpoint,
//! and the next message reopens the conversation); lines typed while the
//! agent works reach it as notes. Every session is saved and can be resumed.

mod agents;
mod credentials;
mod session;
mod shell;
mod term;
mod ui;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arko_agents::Tool;
use arko_agents::terminal::{TerminalConfig, TerminalManager, TerminalTool};
use arko_ai::{CodexAuthFile, LLMClient, Provider};
use arko_workflows::notes::Note;
use arko_workflows::persistence::{EventLog, PersistenceConfig};
use arko_workflows::prelude::*;
use arko_workflows::services::WorkflowServicesConfig;
use clap::Parser;
use tokio::sync::{mpsc, oneshot};

use crate::agents::{AgentSettings, Effort, MAIN, ModelSettings, WORKER, resolve_model};
use crate::credentials::{ANTHROPIC_KEY, Need};
use crate::session::{Session, Settings};
use crate::shell::{Shell, ShellTool, WindowsShell};
use crate::ui::{Control, Input, Output, Stats};

#[derive(Parser, Debug)]
#[command(
    name = "fin",
    version,
    about = "A general-purpose agent for the terminal: a shell, background jobs and workers that run at the same time.",
    after_help = "Credentials: ANTHROPIC_API_KEY for Claude models; a ChatGPT login for OpenAI models (a Codex auth.json: --codex-auth, else FIN_CODEX_AUTH, else Codex's own from `codex login`; or OPENAI_OAUTH_ACCESS_TOKEN and OPENAI_OAUTH_ACCOUNT_ID). ANTHROPIC_API_KEY, FIN_CODEX_AUTH, FIN_CACHE_1H (1 turns on --cache-1h) and FIN_SHELL (cmd or powershell, on Windows) can also be in a .env file: the nearest one in the current directory or a parent, else the one in fin's folder (FIN_HOME, else fin in your data directory, such as ~/.local/share/fin). When the model has none, or the auth file fails, fin asks for them on a setup screen and saves them in that folder's .env; --setup opens it.\n\nWhile the agent works, type to add to its task, and press Ctrl-C to stop the turn. Commands: /new, /cost, /session, /help, /exit."
)]
struct Args {
    /// Send this message, print the reply and exit. Progress goes to
    /// standard error, the reply to standard output.
    #[arg(short = 'p', long)]
    prompt: Option<String>,

    /// The model: an ID, or opus, fable, sonnet, sol, astra or luna.
    /// Default: opus with ANTHROPIC_API_KEY (or one in a .env file), else
    /// sol with a ChatGPT login.
    #[arg(short, long)]
    model: Option<String>,

    /// Reasoning effort.
    #[arg(short, long, value_enum, default_value_t = Effort::Xhigh)]
    effort: Effort,

    /// Fast mode: Anthropic's on Claude Opus 5.5 (priced at 2x), or Fast
    /// mode over the ChatGPT login (uses more of the plan).
    #[arg(long)]
    fast: bool,

    /// The workers' model. Default: the main model, with its effort and
    /// fast mode.
    #[arg(long)]
    worker_model: Option<String>,

    /// The workers' reasoning effort. Default: --effort.
    #[arg(long, value_enum)]
    worker_effort: Option<Effort>,

    /// Fast mode for workers on another model than the main one.
    #[arg(long)]
    worker_fast: bool,

    /// Anthropic's 1-hour prompt cache for the Claude models, instead of
    /// the 5-minute one: the cache outlasts pauses of up to an hour, and a
    /// cache write costs 2x the input price instead of 1.25x. Default:
    /// FIN_CACHE_1H (1 or 0, in the environment or a .env file).
    #[arg(long = "cache-1h", conflicts_with = "no_cache_1h")]
    cache_1h: bool,

    /// The 5-minute prompt cache, whatever FIN_CACHE_1H says.
    #[arg(long = "no-cache-1h")]
    no_cache_1h: bool,

    /// How many workers may run at once; 0 turns them off.
    #[arg(long, default_value_t = 4)]
    workers: u32,

    /// Input size, in tokens, at which an agent writes a handover report and
    /// continues in a fresh conversation.
    #[arg(long, default_value_t = 200_000)]
    handover_at: usize,

    /// Let commands delete, move and overwrite paths outside the working
    /// directory too (protected paths, such as the home directory itself,
    /// stay refused).
    #[arg(long)]
    allow_outside_workspace: bool,

    /// Show the main agent's reasoning as it works.
    #[arg(long)]
    show_thinking: bool,

    /// The working directory. Default: the current directory.
    #[arg(short = 'C', long)]
    workspace: Option<PathBuf>,

    /// Continue the session worked in last in the working directory
    /// (sessions closed before their first message don't count).
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    continue_latest: bool,

    /// Resume a session by ID (see --sessions).
    #[arg(short, long)]
    resume: Option<String>,

    /// List the saved sessions and exit.
    #[arg(long)]
    sessions: bool,

    /// A Codex auth.json with the ChatGPT login. Default: FIN_CODEX_AUTH,
    /// else $CODEX_HOME/auth.json, else ~/.codex/auth.json.
    #[arg(long)]
    codex_auth: Option<PathBuf>,

    /// The shell commands run in on Windows: cmd (cmd.exe) or powershell
    /// (PowerShell 7 where installed, else Windows PowerShell). Default:
    /// FIN_SHELL, else cmd. Elsewhere commands run in bash (else sh).
    #[arg(long, value_enum)]
    shell: Option<WindowsShell>,

    /// Open the setup screen first: paste an Anthropic API key, or give
    /// the path to a Codex auth.json. It is saved in fin's folder.
    #[arg(long)]
    setup: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("fin: {error}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(run(args));
    // The line editor's thread may still be reading, in raw mode.
    term::restore();
    let code = match code {
        Ok(code) => code,
        Err(message) => {
            eprintln!("fin: {message}");
            ExitCode::FAILURE
        }
    };
    // Background jobs and stray tasks don't hold the process open.
    runtime.shutdown_timeout(Duration::from_secs(2));
    // Again, last: the line editor's thread may have gone back into raw
    // mode while the runtime shut down.
    term::restore();
    code
}

async fn run(args: Args) -> Result<ExitCode, String> {
    let root = session::data_root()?;
    if args.sessions {
        list_sessions(&root);
        return Ok(ExitCode::SUCCESS);
    }
    let can_ask = credentials::can_ask();
    if args.setup {
        if !can_ask {
            return Err("--setup needs a terminal".into());
        }
        credentials::setup(None)?;
    }

    // The session: resumed with the settings it was started with, or new.
    let resumed = if let Some(id) = &args.resume {
        Some(Session::open(&root, id)?)
    } else if args.continue_latest {
        let workspace = workspace(&args)?;
        Some(session::latest_in(&root, &workspace).ok_or_else(|| {
            format!(
                "no session to continue in {}; `fin --sessions` lists them",
                workspace.display()
            )
        })?)
    } else {
        None
    };
    let (session, settings, new) = match resumed {
        Some(session) => {
            let settings = session.load_settings()?;
            if args.model.is_some()
                || args.worker_model.is_some()
                || args.fast
                || args.cache_1h
                || args.no_cache_1h
                || args.shell.is_some()
            {
                eprintln!(
                    "fin: a resumed session keeps the models and shell it was started with ({})",
                    settings.main.describe()
                );
            }
            if !settings.workspace.is_dir() {
                return Err(format!(
                    "the session's working directory {} no longer exists",
                    settings.workspace.display()
                ));
            }
            (Some(session), settings, false)
        }
        None => {
            if args.model.is_none() && default_model(&args).is_none() && can_ask {
                credentials::setup(Some(&Need::Any))?;
            }
            (None, new_settings(&args)?, true)
        }
    };

    // Sessions saved before the shell could be chosen ran PowerShell.
    let shell = Shell::detect(settings.windows_shell.unwrap_or(WindowsShell::Powershell));
    if new && args.shell.is_some() && !cfg!(windows) {
        eprintln!(
            "fin: --shell picks the shell on Windows; here commands run in {}",
            shell.name
        );
    }

    // Missing credentials, or an auth file that fails, are asked for.
    let client = loop {
        match client(&args, &settings) {
            Ok(client) => break Arc::new(client),
            Err(Failure::Need(need)) if can_ask => credentials::setup(Some(&need))?,
            Err(Failure::Need(need)) => return Err(need.message()),
            Err(Failure::Other(message)) => return Err(message),
        }
    };
    let agent_settings = AgentSettings {
        main: &settings.main,
        worker: &settings.worker,
        max_workers: settings.max_workers,
        handover_at: settings.handover_at,
        show_thinking: settings.show_thinking,
        shell_name: shell.name,
        allow_outside_workspace: settings.allow_outside_workspace,
    };
    let main_config = agents::main_config(&agent_settings)?;
    let worker_config = agents::worker_config(&agent_settings)?;

    let session = match session {
        Some(session) => session,
        None => Session::create(&root)
            .map_err(|error| format!("could not create a session: {error}"))?,
    };
    if new {
        session
            .save_settings(&settings)
            .map_err(|error| format!("could not save the session: {error}"))?;
    }
    start_log(&session);

    let managers = Managers::open(&session, &shell, settings.allow_outside_workspace)?;
    let one_shot = args.prompt.is_some();
    let (input_sender, input) = mpsc::unbounded_channel();
    let printer = if one_shot {
        None
    } else {
        term::save();
        ui::spawn_input(root.join("history.txt"), input_sender.clone())
    };
    let interactive = printer.is_some();
    let output = Arc::new(Output::new(printer, one_shot));
    {
        // Ctrl-C when the line editor isn't reading (in raw mode it reads
        // Ctrl-C as a key).
        let sender = input_sender.clone();
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                if sender.send(Input::Interrupt).is_err() {
                    break;
                }
            }
        });
    }

    let mut harness = Harness {
        settings,
        session,
        client,
        main_config,
        worker_config,
        tools: managers.tools(&shell),
        output: output.clone(),
        stats: Arc::new(Mutex::new(Stats::default())),
        workflow: None,
        printer: None,
        conversation: Conversation::New,
        project: None,
        unread: Vec::new(),
        queue_lines: !interactive,
        exiting: false,
    };
    harness.project = project_instructions(&harness.settings.workspace);
    harness.open(!new)?;
    let mut input = Inputs {
        receiver: input,
        pending: None,
    };

    let code = if let Some(prompt) = args.prompt.clone() {
        if !new {
            harness.restart_unfinished(&mut input).await;
        }
        let end = if harness.exiting {
            TurnEnd::Exit
        } else {
            harness.turn(prompt, &mut input).await
        };
        match end {
            TurnEnd::Answered => ExitCode::SUCCESS,
            TurnEnd::Stopped | TurnEnd::Exit => ExitCode::from(130),
            TurnEnd::Failed => ExitCode::FAILURE,
        }
    } else {
        harness.banner(new);
        if !new {
            harness.restart_unfinished(&mut input).await;
        }
        if !harness.exiting {
            harness.repl(&mut input).await;
        }
        ExitCode::SUCCESS
    };
    ui::stop_reading();
    managers.shutdown().await;
    // The prompt is still drawn, and its thread still reading: what is
    // printed from here goes straight to the terminal, below the prompt
    // line cleared and with the terminal's modes restored.
    output.detach();
    if interactive {
        tokio::time::sleep(Duration::from_millis(30)).await;
        output.clear_line();
    }
    term::restore();
    output.line(&output.dim(&format!(
        "Session {} saved. Continue it with: fin --resume {}",
        harness.session.id, harness.session.id
    )));
    Ok(code)
}

/// The working directory the flags name, made absolute.
fn workspace(args: &Args) -> Result<PathBuf, String> {
    let path = match &args.workspace {
        Some(path) => path.clone(),
        None => std::env::current_dir().map_err(|error| error.to_string())?,
    };
    let path = std::path::absolute(&path).map_err(|error| error.to_string())?;
    let path = arko_agents::utils::normalize_path(&path);
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    Ok(path)
}

fn new_settings(args: &Args) -> Result<Settings, String> {
    let workspace = workspace(args)?;
    let main_model = match &args.model {
        Some(name) => resolve_model(name)?.id,
        None => default_model(args)
            .ok_or_else(|| Need::Any.message())?
            .to_string(),
    };
    // The 1-hour cache is Anthropic's; other models ignore it.
    let want_1h = if args.cache_1h || args.no_cache_1h {
        args.cache_1h
    } else {
        credentials::switch(credentials::CACHE_1H)?.unwrap_or(false)
    };
    let cache_1h = |id: &str| {
        want_1h && resolve_model(id).is_ok_and(|model| model.provider == Provider::Anthropic)
    };
    let main = ModelSettings {
        cache_1h: cache_1h(&main_model),
        model: main_model,
        effort: args.effort,
        fast: args.fast,
    };
    let worker = match &args.worker_model {
        Some(name) => {
            let model = resolve_model(name)?.id;
            ModelSettings {
                cache_1h: cache_1h(&model),
                model,
                effort: args.worker_effort.unwrap_or(args.effort),
                fast: args.worker_fast,
            }
        }
        None => ModelSettings {
            effort: args.worker_effort.unwrap_or(main.effort),
            ..main.clone()
        },
    };
    if args.cache_1h && !main.cache_1h && !worker.cache_1h {
        eprintln!("fin: --cache-1h is for Claude models; this session has none, so it is ignored");
    }
    Ok(Settings {
        workspace,
        main,
        worker,
        max_workers: args.workers,
        handover_at: args.handover_at,
        allow_outside_workspace: args.allow_outside_workspace,
        show_thinking: args.show_thinking,
        created: chrono::Utc::now(),
        windows_shell: Some(windows_shell(args)?),
        title: args.prompt.as_deref().map(|prompt| ui::cut(prompt, 80)),
    })
}

/// The shell a new session runs commands in on Windows: --shell, else
/// FIN_SHELL, else cmd.exe.
fn windows_shell(args: &Args) -> Result<WindowsShell, String> {
    if let Some(shell) = args.shell {
        return Ok(shell);
    }
    match credentials::var(credentials::SHELL) {
        None => Ok(WindowsShell::Cmd),
        Some(value) => <WindowsShell as clap::ValueEnum>::from_str(value.trim(), true)
            .map_err(|_| format!("FIN_SHELL is `{value}`; set it to cmd or powershell")),
    }
}

/// Opus with an Anthropic key, else GPT-6.1 Sol with a ChatGPT login;
/// `None` without either.
fn default_model(args: &Args) -> Option<&'static str> {
    if credentials::var(ANTHROPIC_KEY).is_some() {
        Some("claude-opus-5-5")
    } else if credentials::codex_auth(args.codex_auth.as_deref()).is_some()
        || credentials::var("OPENAI_OAUTH_ACCESS_TOKEN").is_some()
        || CodexAuthFile::default_path().is_some_and(|path| path.is_file())
    {
        Some("gpt-6.1-sol")
    } else {
        None
    }
}

/// Why there is no model client.
enum Failure {
    /// Credentials are missing, or the auth file fails.
    Need(Need),
    Other(String),
}

/// The model client: Anthropic with an API key, OpenAI with the ChatGPT
/// login only (never an OpenAI API key).
fn client(args: &Args, settings: &Settings) -> Result<LLMClient, Failure> {
    const NOT_USED: [&str; 4] = [
        "OPENAI_API_KEY",
        "GOOGLE_API_KEY",
        "GEMINI_API_KEY",
        "OPENROUTER_API_KEY",
    ];
    let auth_file = credentials::codex_auth(args.codex_auth.as_deref());
    let from_file = auth_file.is_some();
    let mut client = LLMClient::from_env_with(|name| {
        if NOT_USED.contains(&name) || (from_file && name.starts_with("OPENAI_OAUTH_")) {
            None
        } else {
            credentials::var(name)
        }
    })
    .map_err(|error| Failure::Other(error.to_string()))?;
    let openai_model = [&settings.main, &settings.worker]
        .into_iter()
        .find(|model| model.provider() == Ok(Provider::OpenAI));
    if let Some(model) = openai_model
        && !client.has_provider(&Provider::OpenAI)
    {
        let need = |problem: String| {
            Failure::Need(Need::ChatGptLogin {
                model: model.model.clone(),
                problem,
            })
        };
        let auth = match &auth_file {
            Some(path) => CodexAuthFile::load(path),
            None => CodexAuthFile::load_default(),
        }
        .map_err(|error| need(error.to_string()))?;
        auth.credentials().map_err(|error| {
            need(format!(
                "{} holds no ChatGPT login ({error}); sign in with `codex login`",
                auth.path().display()
            ))
        })?;
        client = client
            .with_codex_auth_file(&auth)
            .map_err(|error| need(error.to_string()))?;
    }
    for model in [&settings.main, &settings.worker] {
        let provider = model.provider().map_err(Failure::Other)?;
        if !client.has_provider(&provider) {
            let model = model.model.clone();
            return Err(Failure::Need(match provider {
                Provider::Anthropic => Need::AnthropicKey { model },
                _ => Need::ChatGptLogin {
                    model,
                    problem: "no ChatGPT login found".into(),
                },
            }));
        }
    }
    Ok(client)
}

/// Logs go to the session's `fin.log`, not the terminal. `RUST_LOG` sets
/// the level (default: info).
fn start_log(session: &Session) {
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(session.log_file())
    else {
        return;
    };
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(file)))
        .try_init();
}

/// `AGENTS.md`, else `CLAUDE.md`, in the working directory: the project's
/// instructions, given with the first message of each conversation.
fn project_instructions(workspace: &Path) -> Option<(String, String)> {
    const MAX_BYTES: usize = 64 * 1024;
    ["AGENTS.md", "CLAUDE.md"].into_iter().find_map(|name| {
        let text = std::fs::read_to_string(workspace.join(name)).ok()?;
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let text = if text.len() > MAX_BYTES {
            let mut end = MAX_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}\n[... the rest of {name} is left out ...]", &text[..end])
        } else {
            text.to_string()
        };
        Some((name.to_string(), text))
    })
}

fn list_sessions(root: &Path) {
    let sessions = session::list(root);
    if sessions.is_empty() {
        println!("No sessions yet ({}).", root.join("sessions").display());
        return;
    }
    for (session, settings) in sessions {
        println!(
            "{}  {}  {}{}",
            session.id,
            settings.main.model,
            settings.workspace.display(),
            settings
                .title
                .map(|title| format!("  \"{title}\""))
                .unwrap_or_default()
        );
    }
}

/// The two terminal managers: one runs the `shell` tool's commands, the
/// other the terminal tools' background jobs. Each keeps its jobs' logs in
/// the session's folder.
struct Managers {
    commands: Arc<TerminalManager>,
    jobs: Arc<TerminalManager>,
}

impl Managers {
    fn open(session: &Session, shell: &Shell, allow_outside: bool) -> Result<Self, String> {
        let open = |directory: PathBuf, args: &[String]| {
            let mut config = TerminalConfig::new(directory);
            config.shell = shell.program.clone();
            config.shell_args = args.to_vec();
            config.command_guard.allow_outside_workspace = allow_outside;
            TerminalManager::new(config).map_err(|error| {
                format!(
                    "could not open the session's terminal ({error}); is it open in another fin?"
                )
            })
        };
        Ok(Self {
            commands: open(session.commands_dir(), &shell.args)?,
            jobs: open(session.jobs_dir(), &shell.job_args)?,
        })
    }

    fn tools(&self, shell: &Shell) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(ShellTool::new(
            self.commands.clone(),
            shell.clone(),
        ))];
        for tool in TerminalTool::all(self.jobs.clone()) {
            tools.push(Arc::new(tool));
        }
        tools
    }

    /// Stops the background jobs still running.
    async fn shutdown(&self) {
        self.jobs.shutdown().await;
        self.commands.shutdown().await;
    }
}

/// The workflow's name in new sessions.
const WORKFLOW_NAME: &str = "fin";

/// What a session's checkpoint was saved with: the workflow's name and the
/// agents' configurations. A resume needs both unchanged, so a session
/// keeps them when fin changes: its agents' instructions, or its name
/// (sessions from before the rename were saved as `arko`).
struct SavedSetup {
    name: String,
    /// The configuration of each agent's latest run.
    configs: std::collections::HashMap<String, arko_agents::AgentConfig>,
}

fn saved_setup(checkpoint: &Path, run_dir: &Path) -> Result<SavedSetup, String> {
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

/// Where the conversation with the main agent stands.
enum Conversation {
    /// The next message starts a conversation.
    New,
    /// The last turn ended with a reply: the next message continues it.
    Answered(AgentResult),
    /// The last turn was stopped: the next message reopens it.
    Stopped(StoppedRun),
}

enum TurnEnd {
    Answered,
    Stopped,
    Failed,
    /// The user closed the input during the turn.
    Exit,
}

struct Harness {
    settings: Settings,
    session: Session,
    client: Arc<LLMClient>,
    main_config: arko_agents::AgentConfig,
    worker_config: arko_agents::AgentConfig,
    tools: Vec<Arc<dyn Tool>>,
    output: Arc<Output>,
    stats: Arc<Mutex<Stats>>,
    workflow: Option<Arc<Workflow>>,
    printer: Option<(mpsc::UnboundedSender<Control>, tokio::task::JoinHandle<()>)>,
    conversation: Conversation,
    /// The project's instructions file, by name, and its text.
    project: Option<(String, String)>,
    /// What the user typed during a turn that the agent didn't read before
    /// the turn ended: it goes with the next message.
    unread: Vec<String>,
    /// Without a terminal, lines read during a turn wait for their own
    /// turns instead of reaching the agent as notes.
    queue_lines: bool,
    /// The user quit while a turn ran before the prompt opened.
    exiting: bool,
}

impl Harness {
    fn workflow(&self) -> &Arc<Workflow> {
        self.workflow.as_ref().expect("the workflow is open")
    }

    /// Builds the workflow, from the session's latest checkpoint when
    /// `resume` is set, and starts printing its events.
    fn open(&mut self, resume: bool) -> Result<(), String> {
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
        let name = saved.as_ref().map_or(WORKFLOW_NAME, |saved| &saved.name);
        let mut builder = Workflow::builder()
            .name(name)
            .workspace(&self.settings.workspace)
            .llm_client(self.client.clone())
            .agent(MAIN, config(MAIN, &self.main_config))
            .persistence(
                PersistenceConfig::new(self.session.run_dir())
                    .event_log(EventLog::LifecycleOnly)
                    .keep_last_checkpoints(3),
            )
            .disable_messaging()
            .cancel_grace_period(Duration::from_secs(10));
        for tool in &self.tools {
            builder = builder.agent_tool(MAIN, tool.clone());
        }
        if self.settings.max_workers > 0 {
            builder = builder
                .agent(WORKER, config(WORKER, &self.worker_config))
                .workflow_services(
                    WorkflowServicesConfig::default()
                        .allow_caller(MAIN)
                        .allow_target(WORKER)
                        .max_active_calls(self.settings.max_workers),
                );
            for tool in &self.tools {
                builder = builder.agent_tool(WORKER, tool.clone());
            }
        }
        if let Some(checkpoint) = checkpoint {
            builder = builder.resume_from(checkpoint);
        }
        let workflow = Arc::new(builder.build().map_err(|error| error.to_string())?);
        let (control, controls) = mpsc::unbounded_channel();
        let task = tokio::spawn(ui::print_events(
            workflow.subscribe_events(),
            controls,
            self.output.clone(),
            self.stats.clone(),
            self.settings.show_thinking,
        ));
        if let Some((_, old)) = self.printer.replace((control, task)) {
            old.abort();
        }
        self.conversation = if resume {
            conversation_of(&workflow)
        } else {
            Conversation::New
        };
        self.workflow = Some(workflow);
        Ok(())
    }

    fn banner(&self, new: bool) {
        let out = &self.output;
        let workers = match self.settings.max_workers {
            0 => "no workers".to_string(),
            n if self.settings.worker == self.settings.main => format!("up to {n} workers"),
            n => format!("up to {n} workers on {}", self.settings.worker.describe()),
        };
        out.line(&format!(
            "{} {} · {}",
            out.bold("fin"),
            self.settings.main.describe(),
            workers
        ));
        out.line(&out.dim(&format!(
            "{} · session {}{}",
            self.settings.workspace.display(),
            self.session.id,
            self.project
                .as_ref()
                .map(|(name, _)| format!(" · {name} loaded"))
                .unwrap_or_default()
        )));
        if new {
            out.line(&out.dim(
                "Type a message (end a line with \\ to continue it). While the agent works, type to add to its task; Ctrl-C stops the turn. /help for commands.",
            ));
        } else if let Conversation::Answered(result) = &self.conversation {
            out.line(&out.dim("Resumed. The last reply:"));
            out.line(&result.summary);
        } else {
            out.line(&out.dim("Resumed."));
        }
    }

    /// After a crash, the turn that was running goes on where it was saved.
    /// Continues a turn fin stopped in the middle of. The user may quit
    /// during it (`exiting`).
    async fn restart_unfinished(&mut self, input: &mut Inputs) {
        if let Conversation::Stopped(run) = &self.conversation
            && matches!(run.restart, Restart::Recover)
        {
            self.output.line(
                &self.output.dim(
                    "The last turn didn't finish (fin stopped while it ran); it continues now.",
                ),
            );
            if let TurnEnd::Exit = self.turn(String::new(), input).await {
                self.exiting = true;
            }
        }
    }

    async fn repl(&mut self, input: &mut Inputs) {
        let mut last_interrupt: Option<Instant> = None;
        while let Some(event) = input.next().await {
            match event {
                Input::Eof => break,
                Input::Interrupt => {
                    if last_interrupt.is_some_and(|at| at.elapsed() < Duration::from_secs(3)) {
                        break;
                    }
                    last_interrupt = Some(Instant::now());
                    self.output
                        .line(&self.output.dim("(Ctrl-C again, Ctrl-D or /exit to quit)"));
                }
                Input::Line(line) => {
                    let line = input.coalesce(line).await;
                    let text = line.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if let Some(command) = command(text) {
                        match command {
                            "/exit" | "/quit" => break,
                            "/new" => {
                                self.conversation = Conversation::New;
                                self.unread.clear();
                                self.output.line(
                                    &self
                                        .output
                                        .dim("The next message starts a new conversation."),
                                );
                            }
                            other => self.command(other),
                        }
                        continue;
                    }
                    if let TurnEnd::Exit = self.turn(text.to_string(), input).await {
                        break;
                    }
                }
            }
        }
    }

    /// Commands that work at any time.
    fn command(&self, command: &str) {
        let out = &self.output;
        match command {
            "/cost" => {
                let workflow = self.workflow();
                let result = workflow.result();
                let (input, output) = result.agent_results.iter().fold((0, 0), |sum, run| {
                    (
                        sum.0 + run.total_input_tokens(),
                        sum.1 + run.total_output_tokens(),
                    )
                });
                out.line(&format!(
                    "${:.2} billed in this session · {}k input and {}k output tokens",
                    workflow.context().budget.total_spent(),
                    input / 1000,
                    output / 1000
                ));
                if let Some(usage) = &self.stats.lock().unwrap().plan_usage {
                    out.line(usage);
                }
            }
            "/session" => {
                out.line(&format!("session {}", self.session.id));
                out.line(&out.dim(&format!("{}", self.session.dir.display())));
                out.line(&out.dim(&format!(
                    "main: {} · workers: {}",
                    self.settings.main.describe(),
                    if self.settings.max_workers == 0 {
                        "off".to_string()
                    } else {
                        format!(
                            "{} (up to {})",
                            self.settings.worker.describe(),
                            self.settings.max_workers
                        )
                    }
                )));
            }
            "/new" => out.line(&out.dim("Wait for the turn to end (or press Ctrl-C) first.")),
            _ => {
                out.line("/new      start a new conversation (this session keeps the old one)");
                out.line("/cost     what this session has cost");
                out.line("/session  this session's ID, folder and models");
                out.line("/exit     quit (Ctrl-D too); the session is saved");
                out.line(
                    &out.dim(
                        "While the agent works: type to add to its task; Ctrl-C stops the turn.",
                    ),
                );
            }
        }
    }

    /// One turn: the message goes to the main agent, and its reply is
    /// printed. An empty message restarts an unfinished turn.
    async fn turn(&mut self, message: String, input: &mut Inputs) -> TurnEnd {
        // A turn fin stopped in the middle of (a crash) goes on where it
        // was saved, without a message: the message follows it.
        if !message.is_empty()
            && let Conversation::Stopped(run) = &self.conversation
            && matches!(run.restart, Restart::Recover)
            && let TurnEnd::Exit = Box::pin(self.turn(String::new(), input)).await
        {
            return TurnEnd::Exit;
        }
        let workflow = self.workflow().clone();
        // A message after a stop goes to the stopped task, which resumes.
        let resumes = !message.is_empty()
            && matches!(&self.conversation, Conversation::Stopped(run) if run.restart != Restart::Recover);
        let message = self.compose(message);
        if resumes {
            self.output.line(
                &self
                    .output
                    .dim("  ✎ added to the stopped task: the agent resumes it where it stopped"),
            );
        }
        let spent_before = workflow.context().budget.total_spent();
        let started = Instant::now();
        let conversation = std::mem::replace(&mut self.conversation, Conversation::New);
        let call = {
            let workflow = workflow.clone();
            tokio::spawn(async move {
                match conversation {
                    Conversation::New => workflow.run_agent(MAIN, &message).await,
                    Conversation::Answered(previous) => {
                        workflow.continue_agent(&previous, &message).await
                    }
                    Conversation::Stopped(run) => workflow.restart(&run, &message).await,
                }
            })
        };
        tokio::pin!(call);
        let mut notes: Vec<String> = Vec::new();
        let mut queued: Vec<String> = Vec::new();
        let mut stopping = false;
        let mut exit = false;
        let outcome = loop {
            tokio::select! {
                outcome = &mut call => break outcome,
                event = input.next() => match event {
                    Some(Input::Line(line)) => {
                        let line = input.coalesce(line).await;
                        let text = line.trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        if let Some(command) = command(&text) {
                            if matches!(command, "/exit" | "/quit") {
                                exit = true;
                                stopping |= self.stop(&workflow);
                            } else {
                                self.command(command);
                            }
                        } else if self.queue_lines {
                            queued.push(text);
                        } else {
                            match workflow.post_note(Note::new(text.clone()).to([MAIN]).once()) {
                                Ok(_) => {
                                    self.output.line(&self.output.dim(
                                        "  ✎ added to the task: the agent reads it at its next step",
                                    ));
                                    notes.push(text);
                                }
                                Err(error) => self.output.line(&self.output.error(&format!(
                                    "  could not pass that on: {error}"
                                ))),
                            }
                        }
                    }
                    Some(Input::Interrupt) => {
                        if stopping {
                            self.output.line(&self.output.dim("  (stopping: the agents get up to 10 s to end)"));
                        } else {
                            stopping = self.stop(&workflow);
                        }
                    }
                    Some(Input::Eof) | None => {
                        exit = true;
                        stopping |= self.stop(&workflow);
                    }
                },
            }
        };
        self.flush_printer().await;
        let outcome = outcome.unwrap_or_else(|error| {
            Err(WorkflowError::Channel(format!(
                "the turn's task failed: {error}"
            )))
        });
        let end = match outcome {
            Ok(result) => {
                self.output.line("");
                self.output.answer(result.summary.trim());
                self.keep_unread(&notes, &result);
                self.conversation = Conversation::Answered(result);
                TurnEnd::Answered
            }
            Err(WorkflowError::Cancelled) if stopping => {
                self.output.line(&self.output.warn("Stopped."));
                self.unread.extend(notes);
                match self.reopen() {
                    Ok(()) => TurnEnd::Stopped,
                    Err(error) => {
                        self.output.line(&self.output.error(&format!(
                            "The session couldn't be reopened after the stop: {error}"
                        )));
                        TurnEnd::Failed
                    }
                }
            }
            Err(error) => {
                self.output
                    .line(&self.output.error(&format!("The turn failed: {error}")));
                self.unread.extend(notes);
                self.recover_after_error(&workflow);
                TurnEnd::Failed
            }
        };
        self.footer(spent_before, started);
        if exit {
            return TurnEnd::Exit;
        }
        // Lines read without a terminal, each its own turn.
        for line in queued {
            if let TurnEnd::Exit = Box::pin(self.turn(line, input)).await {
                return TurnEnd::Exit;
            }
        }
        end
    }

    /// The message as the agent gets it: with the project's instructions
    /// when it starts a conversation, and with what the user typed during
    /// the last turn that the agent didn't read.
    fn compose(&mut self, message: String) -> String {
        let mut parts = Vec::new();
        if matches!(self.conversation, Conversation::New)
            && let Some((name, text)) = &self.project
        {
            parts.push(format!(
                "# Project instructions ({name} in the working directory)\n\n{text}"
            ));
        }
        if !self.unread.is_empty() {
            let unread = std::mem::take(&mut self.unread);
            parts.push(format!(
                "# What I typed during your last turn, which you didn't get to read\n\n{}",
                unread
                    .iter()
                    .map(|text| format!("- {}", text.replace('\n', "\n  ")))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if parts.is_empty() {
            return message;
        }
        if !message.is_empty() {
            parts.push(format!("# My message\n\n{message}"));
        }
        parts.join("\n\n")
    }

    /// Notes the run never read go with the next message.
    fn keep_unread(&mut self, notes: &[String], result: &AgentResult) {
        let read = |text: &str| {
            result.task.conversation.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .filter_map(arko_ai::Content::as_text)
                    .any(|part| part.contains(text))
            })
        };
        let unread: Vec<String> = notes.iter().filter(|text| !read(text)).cloned().collect();
        if !unread.is_empty() {
            self.output.line(&self.output.dim(
                "  (the agent finished before it read what you typed; it goes with your next message)",
            ));
            self.unread.extend(unread);
        }
    }

    fn stop(&self, workflow: &Workflow) -> bool {
        self.output.line(&self.output.warn("  Stopping the turn…"));
        workflow.stop(StopRequest::operator("the user stopped the turn"));
        true
    }

    /// After a stop, the workflow is built again from its checkpoint, and
    /// the stopped conversation waits for the next message.
    fn reopen(&mut self) -> Result<(), String> {
        self.workflow = None;
        self.open(true)
    }

    /// After a failed turn, the conversation goes on if its last reply
    /// stands; otherwise the next message starts a new one.
    fn recover_after_error(&mut self, workflow: &Workflow) {
        self.conversation = conversation_of(workflow);
        if matches!(self.conversation, Conversation::New) {
            self.output.line(&self.output.dim(
                "This conversation can't go on; your next message starts a new one (/new). The session keeps the old one.",
            ));
        }
    }

    async fn flush_printer(&self) {
        if let Some((control, _)) = &self.printer {
            let (done, flushed) = oneshot::channel();
            if control.send(Control::Flush(done)).is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(5), flushed).await;
            }
        }
    }

    fn footer(&self, spent_before: f64, started: Instant) {
        let workflow = self.workflow();
        let spent = workflow.context().budget.total_spent();
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
        if let Some(plan) = stats.plan_usage {
            parts.push(plan);
        }
        self.output
            .line(&self.output.dim(&format!("── {} ──", parts.join(" · "))));
    }
}

/// Where the main agent's conversation stands in a workflow built from a
/// checkpoint: stopped (or interrupted), answered, or neither.
fn conversation_of(workflow: &Workflow) -> Conversation {
    if let Some(run) = workflow
        .stopped_runs()
        .into_iter()
        .rfind(|run| run.result.agent_name == MAIN)
    {
        return Conversation::Stopped(run);
    }
    match workflow
        .result()
        .agent_results
        .into_iter()
        .rfind(|result| result.agent_name == MAIN)
    {
        Some(result) if result.status == AgentRunStatus::Completed => {
            Conversation::Answered(result)
        }
        _ => Conversation::New,
    }
}

/// The commands, typed alone on a line.
fn command(text: &str) -> Option<&str> {
    const COMMANDS: [&str; 6] = ["/exit", "/quit", "/new", "/cost", "/session", "/help"];
    COMMANDS.contains(&text).then_some(text)
}

/// The user's input, read on its own thread.
struct Inputs {
    receiver: mpsc::UnboundedReceiver<Input>,
    /// An input read ahead while gathering a pasted block.
    pending: Option<Input>,
}

impl Inputs {
    /// The next input; cancel-safe.
    async fn next(&mut self) -> Option<Input> {
        match self.pending.take() {
            Some(input) => Some(input),
            None => self.receiver.recv().await,
        }
    }

    /// A pasted block arrives as several lines in quick succession (where
    /// the terminal doesn't mark pastes): they are one message.
    async fn coalesce(&mut self, first: String) -> String {
        let mut text = first;
        while self.pending.is_none() {
            match tokio::time::timeout(Duration::from_millis(40), self.receiver.recv()).await {
                Ok(Some(Input::Line(line))) => {
                    text.push('\n');
                    text.push_str(&line);
                }
                Ok(Some(other)) => self.pending = Some(other),
                Ok(None) => self.pending = Some(Input::Eof),
                Err(_) => break,
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_known_commands_are_commands() {
        assert_eq!(command("/cost"), Some("/cost"));
        assert_eq!(command("/usr/bin is missing"), None);
        assert_eq!(command("/cost please"), None);
    }

    #[test]
    fn the_project_instructions_come_from_agents_md_then_claude_md() {
        let temp = tempfile::tempdir().unwrap();
        assert!(project_instructions(temp.path()).is_none());
        std::fs::write(temp.path().join("CLAUDE.md"), "claude rules").unwrap();
        assert_eq!(
            project_instructions(temp.path()).unwrap(),
            ("CLAUDE.md".to_string(), "claude rules".to_string())
        );
        std::fs::write(temp.path().join("AGENTS.md"), "agent rules\n").unwrap();
        assert_eq!(
            project_instructions(temp.path()).unwrap(),
            ("AGENTS.md".to_string(), "agent rules".to_string())
        );
    }
}

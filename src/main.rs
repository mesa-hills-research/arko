//! fin: a general-purpose agent for the terminal.
//!
//! The user talks with one agent (`main`), which works with a shell,
//! background terminal jobs and file tools in the working directory, and
//! can start workers that run while it goes on (`crew`), each in the
//! working directory or a git worktree of its own. Each message is a turn of one
//! conversation: the first runs the agent, later ones continue it. Ctrl-C
//! stops a turn (the workflow is stopped, then rebuilt from its checkpoint,
//! and the next message reopens the conversation); lines typed while the
//! agent works reach it as notes, and its `ask_user` questions are answered
//! at the prompt. The workers' reports reach it as notes while it works,
//! and start a turn when it is idle. Every session is saved and can be
//! resumed.
//!
//! Besides the shell, the agents get the tools of the MCP servers set up
//! (`mcp`), the skills in the skill folders (`skills`), and semantic search
//! over indexes of codebases and docs that fin keeps current (`index`, also
//! run from the command line as `fin index ...`).

mod agents;
mod credentials;
mod crew;
mod index;
mod mcp;
mod message;
#[cfg(test)]
mod mock_model;
mod session;
mod shell;
mod skills;
mod term;
mod ui;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arko_agents::Tool;
use arko_agents::builtins::ASK_USER_TOOL_NAME;
use arko_agents::terminal::{TerminalConfig, TerminalManager, TerminalTool};
use arko_ai::{CodexAuthFile, LLMClient, Provider};
use arko_workflows::notes::Note;
use arko_workflows::persistence::{EventLog, PersistenceConfig};
use arko_workflows::prelude::*;
use arko_workflows::services::{RUN_WORKFLOW_AGENT_TOOL_NAME, WorkflowServicesConfig};
use arko_workflows::user_input::{UserInputHandler, UserInputRequest};
use clap::Parser;
use tokio::sync::{mpsc, oneshot};

use crate::agents::{AgentSettings, Effort, MAIN, ModelSettings, NO_LIMIT, WORKER, resolve_model};
use crate::credentials::{ANTHROPIC_KEY, Need, VOYAGE_KEY};
use crate::crew::Crew;
use crate::session::{Session, Settings};
use crate::shell::{Shell, ShellTool, WindowsShell};
use crate::ui::{Control, Input, Output, Stats};

#[derive(Parser, Debug)]
#[command(
    name = "fin",
    version,
    about = "A general-purpose agent for the terminal: a shell, background jobs, file tools and workers that run in parallel.",
    after_help = "Credentials: ANTHROPIC_API_KEY for Claude models; a ChatGPT login for OpenAI models (a Codex auth.json: --codex-auth, else FIN_CODEX_AUTH, else Codex's own from `codex login`; or OPENAI_OAUTH_ACCESS_TOKEN and OPENAI_OAUTH_ACCOUNT_ID). ANTHROPIC_API_KEY, FIN_CODEX_AUTH, FIN_CACHE_1H (1 turns on --cache-1h) and FIN_SHELL (cmd or powershell, on Windows) can also be in a .env file: the nearest one in the current directory or a parent, else the one in fin's folder (FIN_HOME, else fin in your data directory, such as ~/.local/share/fin). When the model has none, or the auth file fails, fin asks for them on a setup screen and saves them in that folder's .env; --setup opens it. VOYAGE_API_KEY (Voyage AI, for semantic search) can be in the environment or that .env too.\n\nMCP servers: fin starts those set up in --mcp-config files, in the project's .mcp.json (it asks first, and remembers the answer until the file's servers change) and in mcp.json in fin's folder, in Claude Code's format ({\"mcpServers\": {\"name\": {\"command\": ..., \"args\": [...], \"env\": {...}}}}); servers over standard input and output only. Skills: folders with a SKILL.md, in the project's .agents/skills, .claude/skills or .skills, in skills in fin's folder, or in ~/.agents/skills or ~/.claude/skills.\n\nSearch indexes: `fin index create [PATH]` indexes a codebase or its docs, in any directory (the agents can create indexes too). fin keeps the working directory's indexes current while it runs, and other indexes once the agent searches them. The agents' searches return the top results' whole functions and sections. `fin index --help` lists the commands. Without VOYAGE_API_KEY, search is by keywords only.\n\nWorkers: the agent starts them to work in parallel while it goes on, each in the working directory or in a git worktree of its own (made in the session's folder, on a branch fin/<session>/<worker>); their reports reach it as they finish. /workers lists them, /stop-workers stops them.\n\nWhile the agent works, type to add to its task, and press Ctrl-C to stop the turn (the workers go on). Commands: /new, /cost, /workers, /stop-workers, /mcp, /skills, /session, /help, /exit."
)]
struct Args {
    /// Send this message, print the reply and exit. Progress goes to
    /// standard error, the reply to standard output. When the agent starts
    /// workers, fin waits for their reports, which go to the agent, and
    /// prints its reply to them.
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

    /// How many workers may run at once (default: no limit); 0 turns them
    /// off.
    #[arg(long)]
    workers: Option<u32>,

    /// Input size, in tokens, at which an agent writes a handover report and
    /// continues in a fresh conversation. Default: 500000 on Claude models,
    /// 200000 on others (at most 80% of what the model accepts).
    #[arg(long)]
    handover_at: Option<usize>,

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

    /// Open the setup screen first: paste an Anthropic API key or a Voyage
    /// API key, or give the path to a Codex auth.json. It is saved in fin's
    /// folder.
    #[arg(long)]
    setup: bool,

    /// Start the MCP servers in this file too (repeatable), in Claude
    /// Code's format or arko's. A server named here is taken over one of
    /// the same name in the project's .mcp.json or fin's mcp.json.
    #[arg(long = "mcp-config", value_name = "FILE")]
    mcp_config: Vec<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Search indexes of codebases and docs: create, list, search, update
    /// and watch them.
    Index {
        #[command(subcommand)]
        command: index::IndexCommand,
    },
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
    if let Some(Command::Index { command }) = &args.command {
        return index::run_command(command.clone(), &root, credentials::var(VOYAGE_KEY)).await;
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
                || args.handover_at.is_some()
            {
                eprintln!(
                    "fin: a resumed session keeps the models, shell and handover size it was started with ({})",
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
        mcp_tools: &[],
        mcp_servers: &[],
        skill_folders: &[],
        index_tools: &[],
    };
    // Settings the agents can't be built with leave no session behind.
    agents::main_config(&agent_settings)?;
    agents::worker_config(&agent_settings)?;
    // A project's MCP servers are started once the user approves them.
    let (mcp_servers, mcp_notes) =
        mcp::servers(&root, &settings.workspace, &args.mcp_config, can_ask)?;

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

    // MCP servers, skills and search indexes: what goes wrong with them is
    // said, and the session goes on without.
    let startup = Output::new(None, one_shot);
    for note in &mcp_notes {
        startup.line(&startup.warn(&format!("fin: {note}")));
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
                "fin: the MCP server `{}` (in {}) didn't start: {error}",
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
            startup.line(&startup.warn(&format!("fin: search indexes are off: {error}")));
            None
        }
    };
    let mcp_tools = mcp.tool_names();
    let mcp_started = mcp.started();
    let agent_settings = AgentSettings {
        mcp_tools: &mcp_tools,
        mcp_servers: &mcp_started,
        skill_folders: &skill_folders,
        index_tools: if indexes.is_some() {
            index::TOOL_NAMES
        } else {
            &[]
        },
        ..agent_settings
    };
    let main_config = agents::main_config(&agent_settings)?;
    let worker_config = agents::worker_config(&agent_settings)?;
    let mut tools = managers.tools(&shell);
    tools.extend(mcp.tools().iter().cloned());
    if let Some(indexes) = &indexes {
        tools.extend(indexes.tools());
    }
    let (input_sender, input) = mpsc::unbounded_channel();
    let printer = if one_shot {
        None
    } else {
        term::save();
        ui::spawn_input(root.join("history.txt"), input_sender.clone())
    };
    let interactive = printer.is_some();
    let output = Arc::new(Output::new(printer, one_shot));
    let stats = Arc::new(Mutex::new(Stats::default()));
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
            workspace: settings.workspace.clone(),
            client: client.clone(),
            worker_config: worker_config.clone(),
            tools: tools.clone(),
            output: output.clone(),
            stats: stats.clone(),
            show_thinking: settings.show_thinking,
            max_running: settings.max_workers,
            cache: match settings.worker.provider() {
                Ok(Provider::Anthropic) => {
                    Some(crew::WorkerCache::claude(settings.worker.cache_1h))
                }
                Ok(Provider::OpenAI) => Some(crew::WorkerCache::chatgpt()),
                _ => None,
            },
        });
        if !new {
            for problem in crew.load() {
                output.line(&output.warn(&format!("fin: {problem}")));
            }
        }
        (Some(crew), Some(woken))
    } else {
        (None, None)
    };
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
        tools,
        root: root.clone(),
        mcp,
        mcp_notes,
        skill_folders,
        indexes,
        unoffered: None,
        output: output.clone(),
        stats,
        workflow: None,
        printer: None,
        conversation: Conversation::New,
        project: None,
        unread: Vec::new(),
        queue_lines: !interactive,
        exiting: false,
        crew,
        asker,
        one_shot,
        unread_news: 0,
        exit_warned: false,
        news_hint_shown: false,
    };
    harness.project = project_instructions(&harness.settings.workspace);
    harness.open(!new)?;
    if one_shot && let Some(note) = &harness.unoffered {
        output.line(&output.warn(&format!("fin: {note}")));
    }
    let mut input = Inputs {
        receiver: input,
        pending: None,
        questions,
        woken,
    };

    let code = if let Some(prompt) = args.prompt.clone() {
        if !new {
            harness.restart_unfinished(&mut input).await;
        }
        let end = if harness.exiting {
            TurnEnd::Exit
        } else {
            harness.turn(Prompt::User(prompt), &mut input).await
        };
        // The reply waits for the workers the agent started: their news
        // goes to it as it comes.
        let end = match end {
            TurnEnd::Answered => harness.finish_workers(&mut input).await,
            other => other,
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
    let handover_at = |id: &str| {
        args.handover_at.or_else(|| {
            resolve_model(id)
                .ok()
                .map(|model| agents::default_handover(&model))
        })
    };
    let main = ModelSettings {
        cache_1h: cache_1h(&main_model),
        handover_at: handover_at(&main_model),
        model: main_model,
        effort: args.effort,
        fast: args.fast,
    };
    let worker = match &args.worker_model {
        Some(name) => {
            let model = resolve_model(name)?.id;
            ModelSettings {
                cache_1h: cache_1h(&model),
                handover_at: handover_at(&model),
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
    // What sessions read before the handover was set per model.
    let session_handover = main.handover_at.unwrap_or(200_000);
    if args.cache_1h && !main.cache_1h && !worker.cache_1h {
        eprintln!("fin: --cache-1h is for Claude models; this session has none, so it is ignored");
    }
    Ok(Settings {
        workspace,
        main,
        worker,
        max_workers: args.workers.unwrap_or(NO_LIMIT),
        handover_at: session_handover,
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

/// What a resumed session's agents aren't offered, as they started without
/// it: the tools fin has now that their saved tool lists leave out, and
/// skills. A resumed conversation keeps the configuration it was saved
/// with.
fn unoffered(saved: &SavedSetup, fresh: &[(&str, &arko_agents::AgentConfig)]) -> Option<String> {
    let mut missing: Vec<&str> = Vec::new();
    let mut skills = false;
    for (agent, fresh) in fresh {
        let Some(saved) = saved.configs.get(*agent) else {
            continue;
        };
        let offered = &saved.tool_config.available_tool_names;
        for name in &fresh.tool_config.available_tool_names {
            if !offered.contains(name) && !missing.contains(&name.as_str()) {
                missing.push(name);
            }
        }
        skills |= fresh.skills_config.is_some() && saved.skills_config.is_none();
    }
    let mut parts = Vec::new();
    if missing.contains(&crew::START_WORKER) {
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
        "This session started without {}, so its agents don't get them; a new session does.",
        and_list(&parts)
    ))
}

/// `a`, `a and b`, `a, b, and c`.
fn and_list<T: AsRef<str>>(items: &[T]) -> String {
    let items: Vec<&str> = items.iter().map(AsRef::as_ref).collect();
    match items.as_slice() {
        [] => String::new(),
        [one] => one.to_string(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
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

/// What starts a turn.
enum Prompt {
    /// The user's message.
    User(String),
    /// The workers' news, without a message from the user.
    News,
    /// A turn fin stopped in the middle of goes on, without a message.
    Recover,
}

/// What became of the workers' news.
enum News {
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

struct Harness {
    settings: Settings,
    session: Session,
    client: Arc<LLMClient>,
    main_config: arko_agents::AgentConfig,
    worker_config: arko_agents::AgentConfig,
    tools: Vec<Arc<dyn Tool>>,
    /// fin's folder.
    root: PathBuf,
    /// The MCP servers started, and what was left out and why.
    mcp: mcp::Running,
    mcp_notes: Vec<String>,
    /// The skill folders that exist.
    skill_folders: Vec<PathBuf>,
    /// The search indexes; `None` when they couldn't be opened.
    indexes: Option<Arc<index::Indexes>>,
    /// In a resumed session: the tools (and skills) fin has that the
    /// session's agents aren't offered, as they started without them.
    unoffered: Option<String>,
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
    /// The workers; `None` when they are off, or when the session's main
    /// agent delegates with `run_workflow_agent` (sessions from before
    /// workers ran in parallel), which waits for the worker.
    crew: Option<Arc<Crew>>,
    /// Puts the main agent's questions to the user.
    asker: Arc<Asker>,
    /// fin sends one message and exits (-p).
    one_shot: bool,
    /// News from the workers given to the main agent as notes during its
    /// last turn, which it didn't get to read: a turn continues its
    /// conversation for them.
    unread_news: usize,
    /// `/exit` was typed with workers running: the next one quits.
    exit_warned: bool,
    /// The user was told that the workers' news waits for their message.
    news_hint_shown: bool,
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
        if let Some(saved) = &saved {
            self.unoffered = unoffered(
                saved,
                &[(MAIN, &self.main_config), (WORKER, &self.worker_config)],
            );
        }
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
        builder = builder
            .agent_tool(MAIN, Arc::new(message::MessageUser))
            .user_input_handler(self.asker.clone());
        // Sessions from before workers ran in parallel delegate with
        // `run_workflow_agent`, which waits for the worker: they keep it.
        let legacy = saved
            .as_ref()
            .and_then(|saved| saved.configs.get(MAIN))
            .is_some_and(|config| {
                config
                    .tool_config
                    .available_tool_names
                    .iter()
                    .any(|name| name == RUN_WORKFLOW_AGENT_TOOL_NAME)
            });
        if legacy {
            self.crew = None;
        } else if let Some(crew) = &self.crew {
            for tool in crew.lead_tools() {
                builder = builder.agent_tool(MAIN, tool);
            }
        }
        if legacy && self.settings.max_workers > 0 {
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
            None,
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
        out.line(&format!(
            "{} {} · {}",
            out.bold("fin"),
            self.settings.main.describe(),
            self.workers_summary()
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

    /// The workers, as the banner and `/session` say it.
    /// When the agents hand over, as `/session` says it.
    fn handover_summary(&self) -> String {
        let tokens = |model: &ModelSettings| {
            format!(
                "{}k tokens",
                model.handover_at.unwrap_or(self.settings.handover_at) / 1000
            )
        };
        let main = tokens(&self.settings.main);
        let worker = tokens(&self.settings.worker);
        if self.settings.max_workers == 0 || main == worker {
            format!("handover at {main} of input")
        } else {
            format!("handover at {main} of input (main), {worker} (workers)")
        }
    }

    fn workers_summary(&self) -> String {
        let mut text = match self.settings.max_workers {
            0 => return "no workers".to_string(),
            NO_LIMIT => "parallel workers".to_string(),
            n if self.crew.is_some() => format!("up to {n} parallel workers"),
            n => format!("up to {n} workers"),
        };
        if self.settings.worker != self.settings.main {
            text.push_str(&format!(" on {}", self.settings.worker.describe()));
        }
        text
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
            if let TurnEnd::Exit = self.turn(Prompt::Recover, input).await {
                self.exiting = true;
            }
        }
    }

    async fn repl(&mut self, input: &mut Inputs) {
        let mut last_interrupt: Option<Instant> = None;
        loop {
            let event = match input.next_event(true).await {
                Event::Input(event) => event,
                Event::Woken => {
                    if let News::Exit = self.deliver_news(input).await {
                        break;
                    }
                    continue;
                }
                Event::Question(question) => {
                    // No turn runs: no one waits for the answer.
                    drop(question);
                    continue;
                }
            };
            match event {
                None | Some(Input::Eof) => {
                    // Without a terminal, the workers finish first.
                    if self.queue_lines {
                        self.finish_workers(input).await;
                    }
                    break;
                }
                Some(Input::Interrupt) => {
                    if last_interrupt.is_some_and(|at| at.elapsed() < Duration::from_secs(3)) {
                        break;
                    }
                    last_interrupt = Some(Instant::now());
                    self.output
                        .line(&self.output.dim("(Ctrl-C again, Ctrl-D or /exit to quit)"));
                }
                Some(Input::Line(line)) => {
                    let line = input.coalesce(line).await;
                    let text = line.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if let Some(command) = command(text) {
                        match command {
                            "/exit" | "/quit" => {
                                if self.may_exit() {
                                    break;
                                }
                            }
                            "/new" => {
                                let previous = match &self.conversation {
                                    Conversation::Answered(result) => Some(result.clone()),
                                    _ => None,
                                };
                                if let Some(crew) = &self.crew {
                                    crew.lead_restarted(previous.as_ref());
                                }
                                self.unread_news = 0;
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
                    if let TurnEnd::Exit = self.turn(Prompt::User(text.to_string()), input).await {
                        break;
                    }
                    if let News::Exit = self.deliver_news(input).await {
                        break;
                    }
                }
            }
        }
    }

    /// `/exit` quits; with workers running, the first one only warns.
    fn may_exit(&mut self) -> bool {
        let running = self.crew.as_ref().map_or(0, |crew| crew.running().len());
        if running == 0 || self.exit_warned {
            return true;
        }
        self.exit_warned = true;
        self.output.line(&self.output.warn(&format!(
            "{running} worker{} running: /exit again stops {} and quits (resuming the session lets the agent continue {}).",
            if running == 1 { " is" } else { "s are" },
            if running == 1 { "it" } else { "them" },
            if running == 1 { "it" } else { "them" },
        )));
        false
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
                let workers = self.crew.as_ref().map(|crew| crew.spent());
                let (worker_dollars, worker_input, worker_output) =
                    workers.unwrap_or((0.0, 0, 0));
                out.line(&format!(
                    "${:.2} billed in this session · {}k input and {}k output tokens",
                    workflow.context().budget.total_spent() + worker_dollars,
                    (input + worker_input) / 1000,
                    (output + worker_output) / 1000
                ));
                if worker_input + worker_output > 0 {
                    out.line(&out.dim(&format!(
                        "  of which the workers: ${worker_dollars:.2} · {}k input and {}k output tokens",
                        worker_input / 1000,
                        worker_output / 1000
                    )));
                }
                if let Some(usage) = &self.stats.lock().unwrap().plan_usage {
                    out.line(usage);
                }
                if let Some(indexes) = &self.indexes {
                    let (tokens, dollars) = indexes.spent();
                    if tokens > 0 {
                        out.line(&format!(
                            "${dollars:.3} for embeddings (Voyage) while fin ran · {}k tokens",
                            tokens.div_ceil(1000)
                        ));
                    }
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
                    "This session started before workers ran in parallel: the agent waits for each worker it runs.",
                ),
            },
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
                out.line(&format!("session {}", self.session.id));
                out.line(&out.dim(&format!("{}", self.session.dir.display())));
                out.line(&out.dim(&format!(
                    "main: {} · {}",
                    self.settings.main.describe(),
                    if self.settings.max_workers == 0 {
                        "no workers".to_string()
                    } else {
                        format!(
                            "workers: {} ({})",
                            self.settings.worker.describe(),
                            match self.settings.max_workers {
                                NO_LIMIT => "no limit".to_string(),
                                n => format!("up to {n} at once"),
                            }
                        )
                    }
                )));
                out.line(&out.dim(&self.handover_summary()));
            }
            "/new" => out.line(&out.dim("Wait for the turn to end (or press Ctrl-C) first.")),
            _ => {
                out.line(
                    "/new           start a new conversation (this session keeps the old one)",
                );
                out.line("/cost          what this session has cost");
                out.line("/workers       the workers: what each is doing, or reported");
                out.line("/stop-workers  stop the workers running (the agent can resume them)");
                out.line("/mcp           the MCP servers and their tools");
                out.line("/skills        the skills the agents can load");
                out.line("/session       this session's ID, folder and models");
                out.line("/exit          quit (Ctrl-D too); the session is saved");
                out.line(&out.dim(
                    "While the agent works: type to add to its task; Ctrl-C stops the turn (the workers go on). When it asks you something, type the answer.",
                ));
            }
        }
    }

    /// `/mcp`: each server with its tools, or why it didn't start.
    fn list_mcp(&self) {
        let out = &self.output;
        if self.mcp.servers.is_empty() {
            out.line(&format!(
                "No MCP servers. fin starts those set up in {}, in the project's {} (once you approve them) and in --mcp-config files.",
                self.root.join(mcp::USER_FILE).display(),
                mcp::PROJECT_FILE
            ));
        }
        for started in &self.mcp.servers {
            let server = &started.server;
            out.line(&format!(
                "{}  {}",
                out.bold(&server.name),
                out.dim(&format!(
                    "{} · {}",
                    ui::cut(&server.shown, 100),
                    server.file.display()
                ))
            ));
            match &started.result {
                Ok(tools) if tools.is_empty() => out.line(&out.dim("  no tools")),
                Ok(tools) => out.line(&format!(
                    "  {}",
                    tools
                        .iter()
                        .map(|name| mcp::split_tool_name(name)
                            .map_or(name.as_str(), |(_, tool)| tool))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                Err(error) => out.line(&out.error(&format!("  didn't start: {error}"))),
            }
        }
        for note in &self.mcp_notes {
            out.line(&out.dim(note));
        }
        if let Some(note) = &self.unoffered {
            out.line(&out.warn(note));
        }
    }

    /// `/skills`: the skills in the skill folders now.
    fn list_skills(&self) {
        let out = &self.output;
        let skills = match skills::list(&self.skill_folders, &self.settings.workspace) {
            Ok(skills) => skills,
            Err(error) => {
                out.line(&out.error(&format!("The skills couldn't be read: {error}")));
                return;
            }
        };
        if skills.on_demand_skills.is_empty() && skills.auto_loaded_skills.is_empty() {
            out.line(&format!(
                "No skills. fin reads them (folders with a SKILL.md) from the project's .agents/skills, .claude/skills and .skills, from {}, and from ~/.agents/skills and ~/.claude/skills.",
                self.root.join("skills").display()
            ));
            return;
        }
        for skill in &skills.on_demand_skills {
            out.line(&format!(
                "{}  {}",
                out.bold(&skill.name),
                ui::cut(&skill.description, 160)
            ));
        }
        for skill in &skills.auto_loaded_skills {
            out.line(&format!(
                "{}  {}",
                out.bold(&skill.name),
                out.dim("(always in the agents' instructions)")
            ));
        }
        out.line(&out.dim(&format!(
            "From {}",
            self.skill_folders
                .iter()
                .map(|folder| folder.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    /// One turn: the message goes to the main agent, and its reply is
    /// printed.
    async fn turn(&mut self, prompt: Prompt, input: &mut Inputs) -> TurnEnd {
        // A turn fin stopped in the middle of (a crash) goes on where it
        // was saved, without a message: the message follows it.
        if !matches!(prompt, Prompt::Recover)
            && let Conversation::Stopped(run) = &self.conversation
            && matches!(run.restart, Restart::Recover)
            && let TurnEnd::Exit = Box::pin(self.turn(Prompt::Recover, input)).await
        {
            return TurnEnd::Exit;
        }
        self.exit_warned = false;
        self.news_hint_shown = false;
        let workflow = self.workflow().clone();
        // A message after a stop goes to the stopped task, which resumes.
        let resumes = !matches!(prompt, Prompt::Recover)
            && matches!(&self.conversation, Conversation::Stopped(run) if run.restart != Restart::Recover);
        let news_turn = matches!(prompt, Prompt::News);
        let (message, mail) = self.compose(prompt);
        if news_turn {
            let mut from: Vec<&str> = Vec::new();
            for event in &mail {
                if !from.contains(&event.worker.as_str()) {
                    from.push(&event.worker);
                }
            }
            self.output.line(&self.output.dim(&if from.is_empty() {
                "  ⇠ the workers' news goes to the agent".to_string()
            } else {
                format!("  ⇠ news from {} goes to the agent", and_list(&from))
            }));
        }
        if resumes {
            self.output.line(
                &self
                    .output
                    .dim("  ✎ added to the stopped task: the agent resumes it where it stopped"),
            );
        }
        let spent_before = self.spent();
        let started = Instant::now();
        // Notes of the turn before were given again with this one.
        self.unread_news = 0;
        if let Some(crew) = &self.crew {
            crew.lead_started(workflow.notes_handle());
        }
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
        // The agent's question the next line answers.
        let mut answering: Option<oneshot::Sender<String>> = None;
        let outcome = loop {
            tokio::select! {
                outcome = &mut call => break outcome,
                event = input.next_event(false) => match event {
                    Event::Question(question) => {
                        if question.answer.is_closed() {
                            continue;
                        }
                        self.flush_printer().await;
                        self.output.line(&self.output.bold("  ? The agent asks:"));
                        for line in question.text.trim().lines() {
                            self.output.line(&format!("    {line}"));
                        }
                        self.output.line(&self.output.dim("  (Type your answer and press Enter.)"));
                        answering = Some(question.answer);
                    }
                    Event::Woken => {}
                    Event::Input(Some(Input::Line(line))) => {
                        let line = input.coalesce(line).await;
                        let text = line.trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        if let Some(command) = command(&text) {
                            if matches!(command, "/exit" | "/quit") {
                                if self.may_exit() {
                                    exit = true;
                                    stopping |= self.stop(&workflow);
                                }
                            } else {
                                self.command(command);
                            }
                            continue;
                        }
                        let text = match answering.take() {
                            Some(answer) => match answer.send(text) {
                                Ok(()) => {
                                    self.output.line(&self.output.dim("  ✎ answered"));
                                    continue;
                                }
                                // The question was dropped (a stop).
                                Err(text) => text,
                            },
                            None => text,
                        };
                        if self.queue_lines {
                            queued.push(text);
                        } else {
                            match workflow.post_note(Note::new(text.clone()).to([MAIN]).once()) {
                                Ok(_) => {
                                    self.output.line(&self.output.dim(
                                        "  ✎ added to the task: the agent reads it at its next step",
                                    ));
                                    notes.push(text);
                                    if let Some(crew) = &self.crew {
                                        crew.user_typed();
                                    }
                                }
                                Err(error) => self.output.line(&self.output.error(&format!(
                                    "  could not pass that on: {error}"
                                ))),
                            }
                        }
                    }
                    Event::Input(Some(Input::Interrupt)) => {
                        if stopping {
                            self.output.line(&self.output.dim("  (stopping: the agents get up to 10 s to end)"));
                        } else {
                            stopping = self.stop(&workflow);
                        }
                    }
                    Event::Input(Some(Input::Eof) | None) => {
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
        let unread_news = self
            .crew
            .as_ref()
            .map_or(0, |crew| crew.lead_ended(outcome.as_ref().ok()));
        let end = match outcome {
            Ok(result) => {
                self.unread_news = unread_news;
                self.output.line("");
                // With -p, a reply the workers' news will follow isn't the
                // answer.
                if self.one_shot && self.news_coming() {
                    self.output.line(result.summary.trim());
                } else {
                    self.output.answer(result.summary.trim());
                }
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
                self.recover_after_error(&workflow, mail);
                TurnEnd::Failed
            }
        };
        self.footer(spent_before, started);
        if exit {
            return TurnEnd::Exit;
        }
        // Lines read without a terminal, each its own turn.
        for line in queued {
            if let TurnEnd::Exit = Box::pin(self.turn(Prompt::User(line), input)).await {
                return TurnEnd::Exit;
            }
        }
        end
    }

    /// Whether the workers have news for the main agent, or will have.
    fn news_coming(&self) -> bool {
        self.unread_news > 0
            || self
                .crew
                .as_ref()
                .is_some_and(|crew| crew.has_mail() || !crew.running().is_empty())
    }

    /// While the main agent's conversation waits for a message, the
    /// workers' news (their reports and messages) starts a turn of it.
    async fn deliver_news(&mut self, input: &mut Inputs) -> News {
        input.drain_woken();
        loop {
            let Some(crew) = self.crew.clone() else {
                return News::Done;
            };
            if !crew.has_mail() && self.unread_news == 0 {
                return News::Done;
            }
            if !matches!(self.conversation, Conversation::Answered(_)) {
                if !self.news_hint_shown && !self.one_shot {
                    self.news_hint_shown = true;
                    self.output.line(&self.output.dim(
                        "  (the workers have news for the agent: it goes with your next message)",
                    ));
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

    /// Without a terminal (with -p, or input from a pipe), fin waits for
    /// the workers before it ends: their news goes to the agent as it
    /// comes. Returns how the last turn ended.
    async fn finish_workers(&mut self, input: &mut Inputs) -> TurnEnd {
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
                    crew.stop_all("the user stopped fin");
                    return TurnEnd::Stopped;
                }
                // News that came as the last turn ended.
                News::Done if crew.has_mail() || self.unread_news > 0 => continue,
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
                Event::Question(question) => drop(question),
                Event::Input(Some(Input::Line(_))) => {}
                Event::Input(Some(Input::Interrupt | Input::Eof) | None) => {
                    let stopped = crew.stop_all("the user stopped fin");
                    self.output.line(&self.output.warn(&format!(
                        "Stopping {stopped} worker{}.",
                        if stopped == 1 { "" } else { "s" }
                    )));
                    return TurnEnd::Stopped;
                }
            }
        }
    }

    /// The message as the agent gets it, and the workers' news it carries:
    /// with the project's instructions when it starts a conversation, with
    /// what the user typed during the last turn that the agent didn't read,
    /// and with the news from the workers it hasn't been given. A turn that
    /// recovers gets no message.
    fn compose(&mut self, prompt: Prompt) -> (String, Vec<crew::Event>) {
        let message = match prompt {
            Prompt::Recover => return (String::new(), Vec::new()),
            Prompt::User(message) => Some(message),
            Prompt::News => None,
        };
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
        let mail = self
            .crew
            .as_ref()
            .map(|crew| crew.take_mail())
            .unwrap_or_default();
        if !mail.is_empty() {
            let heading = if message.is_some() {
                "# News from your workers since your last turn"
            } else {
                "# News from your workers (fin started this turn for it; the user hasn't written)"
            };
            parts.push(format!(
                "{heading}\n\n{}",
                mail.iter()
                    .map(crew::Event::text)
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n")
            ));
        }
        let message = match message {
            Some(message) if parts.is_empty() => message,
            Some(message) => {
                parts.push(format!("# My message\n\n{message}"));
                parts.join("\n\n")
            }
            None if parts.is_empty() => {
                "Your workers sent you news as your last turn ended: it follows. (fin started this turn for it; the user hasn't written.)".to_string()
            }
            None => parts.join("\n\n"),
        };
        (message, mail)
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
        let running = self.crew.as_ref().map_or(0, |crew| crew.running().len());
        self.output.line(&self.output.warn(&if running == 0 {
            "  Stopping the turn…".to_string()
        } else {
            format!(
                "  Stopping the turn… (the workers go on: /stop-workers stops {})",
                if running == 1 { "it" } else { "them" }
            )
        }));
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
    /// stands; otherwise the next message starts a new one. The workers'
    /// news the turn carried, and the news it may not have read, go back
    /// to the mailbox.
    fn recover_after_error(&mut self, workflow: &Workflow, mail: Vec<crew::Event>) {
        self.conversation = conversation_of(workflow);
        if let Some(crew) = &self.crew {
            crew.lead_restarted(match &self.conversation {
                Conversation::Answered(result) => Some(result),
                _ => None,
            });
            crew.put_back(mail);
        }
        if matches!(self.conversation, Conversation::New) {
            self.output.line(self.output.dim(
                "This conversation can't go on; your next message starts a new one (/new). The session keeps the old one.",
            ).as_str());
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

    /// What the session has cost: the main agent's runs and the workers'.
    fn spent(&self) -> f64 {
        self.workflow().context().budget.total_spent()
            + self.crew.as_ref().map_or(0.0, |crew| crew.spent().0)
    }

    fn footer(&self, spent_before: f64, started: Instant) {
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
    const COMMANDS: [&str; 10] = [
        "/exit",
        "/quit",
        "/new",
        "/cost",
        "/workers",
        "/stop-workers",
        "/mcp",
        "/skills",
        "/session",
        "/help",
    ];
    COMMANDS.contains(&text).then_some(text)
}

/// The user's input, read on its own thread; the main agent's questions;
/// and the crew's wake-ups.
struct Inputs {
    receiver: mpsc::UnboundedReceiver<Input>,
    /// An input read ahead while gathering a pasted block.
    pending: Option<Input>,
    questions: mpsc::UnboundedReceiver<Question>,
    /// The crew says the main agent has news, or a worker stopped running.
    woken: Option<mpsc::UnboundedReceiver<()>>,
}

/// What [`Inputs::next_event`] returns.
enum Event {
    /// An input; `None` once the input thread is gone.
    Input(Option<Input>),
    /// The main agent asks the user something.
    Question(Question),
    /// The crew woke fin up.
    Woken,
}

impl Inputs {
    /// The next input, question, or (when `woken`) wake-up; cancel-safe.
    async fn next_event(&mut self, woken: bool) -> Event {
        if let Some(input) = self.pending.take() {
            return Event::Input(Some(input));
        }
        tokio::select! {
            input = self.receiver.recv() => Event::Input(input),
            Some(question) = self.questions.recv() => Event::Question(question),
            () = wake(&mut self.woken), if woken => Event::Woken,
        }
    }

    /// Forgets the wake-ups so far: what they were for is being seen to.
    fn drain_woken(&mut self) {
        if let Some(woken) = &mut self.woken {
            while woken.try_recv().is_ok() {}
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

/// The crew's next wake-up; never, without a crew.
async fn wake(woken: &mut Option<mpsc::UnboundedReceiver<()>>) {
    if let Some(receiver) = woken {
        if receiver.recv().await.is_some() {
            return;
        }
        // The crew is gone.
        *woken = None;
    }
    std::future::pending::<()>().await;
}

/// A question the main agent asks with `ask_user`, and where its answer
/// goes.
struct Question {
    text: String,
    answer: oneshot::Sender<String>,
}

/// Answers the main agent's `ask_user` questions: the user types the
/// answer at the prompt. Without a terminal, the agent is told that no one
/// can answer. It always answers: an error would fail the turn.
struct Asker {
    questions: Option<mpsc::UnboundedSender<Question>>,
}

/// The answer when no one can answer.
const CANT_ASK: &str = "No one can answer questions in this session (fin runs without a terminal, from a script or with -p). Decide yourself, go on, and say in your reply what you assumed.";

#[async_trait::async_trait]
impl UserInputHandler for Asker {
    async fn respond(&self, request: UserInputRequest) -> Result<String, String> {
        let Some(questions) = &self.questions else {
            return Ok(CANT_ASK.to_string());
        };
        let (answer, answered) = oneshot::channel();
        let question = Question {
            text: request.questions,
            answer,
        };
        if questions.send(question).is_err() {
            return Ok(CANT_ASK.to_string());
        }
        Ok(answered
            .await
            .unwrap_or_else(|_| "(The user didn't answer.)".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_known_commands_are_commands() {
        assert_eq!(command("/cost"), Some("/cost"));
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
            main: &opus,
            worker: &opus,
            max_workers: NO_LIMIT,
            handover_at: 200_000,
            show_thinking: false,
            shell_name: "bash",
            allow_outside_workspace: false,
            mcp_tools: &[],
            mcp_servers: &[],
            skill_folders: &[],
            index_tools: &[],
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
            "This session started without workers that run in parallel, the file tools, questions to you (ask_user), and answers to you while it works (message_user), so its agents don't get them; a new session does."
        );
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
        let plain = AgentSettings {
            main: &opus,
            worker: &opus,
            max_workers: 4,
            handover_at: 200_000,
            show_thinking: false,
            shell_name: "bash",
            allow_outside_workspace: false,
            mcp_tools: &[],
            mcp_servers: &[],
            skill_folders: &[],
            index_tools: &[],
        };
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
            ..plain
        })
        .unwrap();
        assert_eq!(
            note,
            "This session started without the search index tools, the tools of the MCP servers github and db, and skills, so its agents don't get them; a new session does."
        );
    }

    /// A harness in `temp` on the model server's client, with workers;
    /// with a terminal when `interactive`. Returns it, its inputs, and
    /// what sends the user's input.
    fn harness(
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
        let settings = Settings {
            workspace: workspace.clone(),
            main: luna.clone(),
            worker: luna.clone(),
            max_workers: NO_LIMIT,
            handover_at: 200_000,
            allow_outside_workspace: false,
            show_thinking: false,
            created: chrono::Utc::now(),
            title: None,
            windows_shell: None,
        };
        let agent_settings = AgentSettings {
            main: &luna,
            worker: &luna,
            max_workers: NO_LIMIT,
            handover_at: 200_000,
            show_thinking: false,
            shell_name: "bash",
            allow_outside_workspace: false,
            mcp_tools: &[],
            mcp_servers: &[],
            skill_folders: &[],
            index_tools: &[],
        };
        let main_config = agents::main_config(&agent_settings).unwrap();
        let worker_config = agents::worker_config(&agent_settings).unwrap();
        let root = temp.join("fin");
        let session = Session::create(&root).unwrap();
        let client = Arc::new(client);
        let output = Arc::new(Output::new(None, true));
        let stats = Arc::new(Mutex::new(Stats::default()));
        let (crew, woken) = Crew::new(crew::Setup {
            session_id: session.id.clone(),
            session_dir: session.dir.clone(),
            workspace,
            client: client.clone(),
            worker_config: worker_config.clone(),
            tools: Vec::new(),
            output: output.clone(),
            stats: stats.clone(),
            show_thinking: false,
            max_running: NO_LIMIT,
            cache: None,
        });
        let (questions_sender, questions) = mpsc::unbounded_channel();
        let (input_sender, receiver) = mpsc::unbounded_channel();
        let mut harness = Harness {
            settings,
            session,
            client,
            main_config,
            worker_config,
            tools: Vec::new(),
            root,
            mcp: mcp::Running::default(),
            mcp_notes: Vec::new(),
            skill_folders: Vec::new(),
            indexes: None,
            unoffered: None,
            output,
            stats,
            workflow: None,
            printer: None,
            conversation: Conversation::New,
            project: None,
            unread: Vec::new(),
            queue_lines: !interactive,
            exiting: false,
            crew: Some(crew),
            asker: Arc::new(Asker {
                questions: interactive.then_some(questions_sender),
            }),
            one_shot: !interactive,
            unread_news: 0,
            exit_warned: false,
            news_hint_shown: false,
        };
        harness.open(false).unwrap();
        let inputs = Inputs {
            receiver,
            pending: None,
            questions,
            woken: Some(woken),
        };
        (harness, inputs, input_sender)
    }

    fn summary(harness: &Harness) -> String {
        match &harness.conversation {
            Conversation::Answered(result) => result.summary.clone(),
            _ => panic!("the conversation wasn't answered"),
        }
    }

    /// With -p, the agent starts a worker and replies; fin waits for the
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
                if body.contains("You are worker w1") {
                    hold.notified().await;
                    complete("w", "There are 3 files.")
                } else if body.contains("There are 3 files.") {
                    complete("m3", "Final: 3 files.")
                } else if body.contains("Started worker w1") {
                    complete("m2", "w1 is counting; its report follows.")
                } else {
                    tool_call(
                        "m1",
                        crew::START_WORKER,
                        serde_json::json!({"task": "Count the files."}),
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
        assert_eq!(summary(&harness), "w1 is counting; its report follows.");
        let crew = harness.crew.clone().unwrap();
        assert_eq!(crew.running(), ["w1"]);
        assert!(harness.news_coming());

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
            .find(|body| body.contains("There are 3 files.") && !body.contains("You are worker"))
            .unwrap();
        assert!(
            news.contains(
                "# News from your workers (fin started this turn for it; the user hasn't written)"
            ),
            "{news}"
        );
        crew.shutdown().await;
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

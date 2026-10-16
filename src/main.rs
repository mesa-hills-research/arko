//! Arko: a general-purpose agent for the terminal.
//!
//! The user talks with one agent (`main`), which works with a shell,
//! background terminal jobs and file tools in the working directory, and
//! can start workers that run while it goes on (`crew`), each in the
//! working directory or a git worktree of its own. Each message is a turn of one
//! conversation: the first runs the agent, later ones continue it. Ctrl-C
//! stops only the lead. The next message reopens its conversation. Lines
//! typed while the agent works reach it as notes, and its `ask_user` questions are answered
//! at the prompt. The workers' reports reach it as notes while it works,
//! and start a turn when it is idle. Every session is saved and can be
//! resumed.
//!
//! Besides the shell, the agents get the tools of the MCP servers set up
//! (`mcp`), the skills in the skill folders (`skills`), and semantic search
//! over indexes of codebases and docs that Arko keeps current (`index`, also
//! run from the command line as `arko index ...`).

mod agents;
mod claude_code;
mod commands;
mod conversations;
mod credentials;
mod crew;
mod glyphs;
mod handover;
mod index;
mod keys;
mod mcp;
mod message;
#[cfg(test)]
mod mock_model;
mod model_command;
mod outside;
mod project;
mod providers;
mod remote;
#[cfg(test)]
mod remote_demo;
mod session;
mod shell;
mod skills;
mod term;
mod tui;
mod ui;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
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
use arko_workflows::user_input::{UserInputHandler, UserInputRequest};
use clap::Parser;
use tokio::sync::{mpsc, oneshot};

use crate::agents::{
    AgentSettings, Effort, ExtraRules, MAIN, ModelSettings, NO_LIMIT, WORKER, resolve_model,
};
use crate::credentials::{Need, VOYAGE_KEY};
use crate::crew::Crew;
use crate::providers::{Failure, OpenAiAuth};
use crate::remote::{Action, Delivery, Level, Source};
use crate::session::{Session, Settings};
use crate::shell::{Shell, ShellTool, WindowsShell};
use crate::ui::{Control, Input, Output, Stats};

#[derive(Parser, Debug)]
#[command(
    name = "arko",
    version,
    about = "A general-purpose agent for the terminal: a shell, background jobs, file tools and workers that run in parallel.",
    after_help = "Credentials: ANTHROPIC_API_KEY for Claude models. For OpenAI models, a ChatGPT login or OPENAI_API_KEY, one per session (--openai-auth). The ChatGPT login is a Codex auth.json (--codex-auth, else ARKO_CODEX_AUTH, else Codex's own from `codex login`), or OPENAI_OAUTH_ACCESS_TOKEN with OPENAI_OAUTH_ACCOUNT_ID. GEMINI_API_KEY (or GOOGLE_API_KEY) for Gemini models, and OPENROUTER_API_KEY for openrouter: models. cc: models run in your installed Claude Code (`claude`, signed in with `claude auth login`) on your Claude plan, never on ANTHROPIC_API_KEY. When the model's credential is missing, Arko asks for it on a setup screen and saves it in Arko's .env. --setup opens that screen. While Arko runs, /keys (also in /settings) adds, replaces and removes API keys, and a key added is used at once.\n\nSettings: the API keys above, ARKO_CODEX_AUTH, ARKO_OPENAI_AUTH (chatgpt or api-key), ARKO_CACHE_1H (1 turns on --cache-1h), ARKO_SHELL (cmd or powershell, on Windows), ARKO_SYMBOLS, ARKO_THEME, ARKO_REMOTE_HOST and VOYAGE_API_KEY can be in the environment or in a .env file. Arko reads the nearest .env in the current directory or a parent, then the one in Arko's folder (ARKO_HOME, else arko in your data directory, such as ~/.local/share/arko).\n\nThe screen: in a terminal, Arko shows the session full screen (--repl keeps the line prompt). Enter sends, and Ctrl+J (or Shift+Enter where the terminal reports it) starts a new line. While the agent works, what you send adds to its task. Ctrl+O switches the view between full, quiet (each worker as one line) and messages. Ctrl+T, or ↓ from the input, reaches the agent list, where Enter shows a worker's own work and Esc goes back. Ctrl+C stops the turn, and the workers go on. While a shell command of the agent shown runs, Ctrl+B moves it to the background. Drag over the transcript to select text, and it is copied when you let go. A double click copies a word, and a triple click a line. /help lists the keys, and /settings holds the view, the theme and its previews, the mouse, outside access, the handover size, when commands move to the background, text replies, the extra rules, the project instructions, the web page and the API keys.\n\nThemes: ARKO_THEME picks a theme by name, such as \"macOS Classic Light\" or \"Windows Dark\". The default is macOS Classic Dark, and `terminal` keeps the terminal's own colours. Your own themes go in the themes folder in Arko's folder, in GPUI Kit's theme format. The theme list in /settings shows a theme when you rest on it, and Previews off there shows none until you choose one.\n\nOutside access: the agents change files only inside the working directory. /outside on, or --allow-outside-workspace (also when resuming), lets commands and the file tools change paths outside it too, and /outside off stops that. The agents are told when it changes, and workers follow it. Protected paths, such as the home directory itself, stay refused.\n\nBackground commands: a shell command still running after 10 minutes moves to the background, where it goes on as a job and the agent goes on with its work. The agent hears when it finishes, and a job that finishes while the agent waits for you starts a turn, as a worker's report does. Ctrl+B moves a command to the background at once. /background-after DURATION|off (also in /settings), or --background-after for a new or resumed session, sets the time. With -p commands stay in the foreground unless the flag says otherwise.\n\nModels: --model (-m) and --effort set the main agent's model, and --worker-model and --worker-effort the workers'. Before the conversation starts, /model MODEL [EFFORT] changes the main agent's model. After that the main agent keeps its model for the session, and /model worker MODEL [EFFORT] sets the model of workers started from now on. /model worker main makes them follow the main agent again. A worker keeps the model it started on, also when it is resumed or continued. /model and /model worker without a model open a picker of the models you can use.\n\nReplies: in a session you take part in, the main agent ends each turn with a plain reply. With -p it ends each turn by calling complete_task instead, so a reply that wasn't meant to be its last doesn't end the run. --text-replies and --no-text-replies choose for a new session, and /text-replies (also in /settings) changes it before the conversation starts. A resumed session keeps its own.\n\nExtra rules: agents on small models (Haiku, luna) get a few extra rules in their instructions. Each agent goes by its own model, so a worker on Haiku gets them under a main agent on Opus, which doesn't. --extra-rules gives them to every agent and --no-extra-rules to none, for a new session, and /extra-rules (also in /settings) changes it before the conversation starts. A resumed session keeps its own.\n\nProject instructions: the agents get AGENTS.md from the working directory, or CLAUDE.md when there is no AGENTS.md. The main agent gets it once per conversation, in the first message. Each worker gets the same text once, in its first message, also when it works in a worktree of its own. Both get it again after a handover. --project-instructions both gives both files, each under its own name, and --project-instructions off neither. --no-worker-project-instructions keeps them from the workers. The flags work for a new session or a resumed one, and /project-instructions (also in /settings) changes them as you go. A change reaches the conversations and the workers that start after it.\n\nConversations: /new starts a new conversation in the session, and /resume goes back to an earlier one. The screen opens a list to choose from. At the line prompt and on the web page, /resume lists them and /resume N goes back to number N. A resumed session (-c, --resume) goes on with the conversation it was in.\n\nHandovers: when the main agent's input reaches the handover size (500k tokens on Claude models, 250k on the ChatGPT login, 200k on others), it writes a report and goes on from it in a fresh conversation. On cc: models, Claude Code compacts the conversation at that size instead, and the agent goes on in the same conversation from Claude Code's summary. Workers do the same. /handover-at SIZE lowers the size for the session, and /handover hands over (or compacts) now.\n\nCost: /cost shows what the session billed. For work on a Claude or ChatGPT plan, it also shows the cost at API prices and the plan's usage.\n\nMCP servers: Arko starts those set up in --mcp-config files, in the project's .mcp.json (it asks first, and remembers the answer until the file's servers change) and in mcp.json in Arko's folder. They use Claude Code's format ({\"mcpServers\": {\"name\": {\"command\": ..., \"args\": [...], \"env\": {...}}}}) and talk over standard input and output. Skills: folders with a SKILL.md, in the project's .agents/skills, .claude/skills or .skills, in skills in Arko's folder, or in ~/.agents/skills or ~/.claude/skills.\n\nSearch indexes: `arko index create [PATH]` indexes a codebase or its docs, in any directory (the agents can create indexes too). Arko keeps the working directory's indexes current while it runs, and other indexes once the agent searches them. The agents' searches return the top results' whole functions and sections. `arko index --help` lists the commands. Without VOYAGE_API_KEY, search is by keywords only.\n\nWorkers: the agent starts them to work in parallel while it goes on, each in the working directory or in a git worktree of its own (made in the session's folder, on a branch arko/<session>/<worker>). Their reports reach it as they finish. /workers lists them, /watch shows one's own work, and /stop-workers stops them.\n\nRemote: --remote (or /remote during a session) serves a web page for following the session and taking part in it from a browser, such as a phone's. It has the same views as the screen, and its settings change outside access, text replies, the extra rules, the project instructions, the handover size and when commands move to the background, and show links for other devices. Arko prints the links best first: the local network's, then Tailscale's, then other addresses, with VPN tunnels last. ARKO_REMOTE_HOST (an interface or an address) puts its link first. The QR code is for the first link, and /remote qr [INTERFACE] draws one again. The link holds the session's key (kept in the session's folder), which the page trades for a cookie. Arko listens on 127.0.0.1:8765 by default, and --remote 0.0.0.0 (or 0.0.0.0:PORT) lets in the devices on your network, or on your tailnet with Tailscale. The page is served without TLS. On a network you don't trust, reach it through an SSH tunnel (ssh -L 8765:127.0.0.1:8765 HOST, then open the link on your side) or Tailscale. /remote off stops it and ends its sessions, and /remote new-key makes a new key.\n\nSymbols: the classic Windows console (cmd.exe or PowerShell outside Windows Terminal) shows characters its fonts lack as question marks, so there Arko prints stand-ins those fonts have. ARKO_SYMBOLS sets the symbols: full, basic (those stand-ins) or ascii.\n\nCommands: /new, /resume, /model, /keys, /cost, /workers, /watch, /stop-workers, /view, /copy, /settings, /mcp, /skills, /session, /remote, /outside, /background-after, /text-replies, /extra-rules, /project-instructions, /handover, /handover-at, /help, /exit."
)]
struct Args {
    /// Send this message, print the reply and exit. Progress goes to
    /// standard error, the reply to standard output. When the agent starts
    /// workers, Arko waits for their reports, which go to the agent, and
    /// prints its reply to them.
    #[arg(short = 'p', long)]
    prompt: Option<String>,

    /// The model: an ID or alias (opus, fable, sonnet, sol, astra, luna,
    /// gemini, flash), openrouter:<slug> for a model on OpenRouter
    /// (openrouter:anthropic/claude-opus-5.5), or cc:<model> for a Claude
    /// model in your installed Claude Code, on your Claude plan (cc:opus,
    /// cc:fable). Default: the model of the last session you started
    /// (with its workers' model, when that differed) while its credential
    /// is there, else by the credentials there are: cc:opus with Claude
    /// Code signed in, else opus with ANTHROPIC_API_KEY, else sol with a
    /// ChatGPT login, else openrouter:anthropic/claude-opus-5.5 with
    /// OPENROUTER_API_KEY, else sol with OPENAI_API_KEY, else gemini with
    /// GEMINI_API_KEY. Arko says which it took. Runs with -p don't change
    /// the next default.
    #[arg(short, long)]
    model: Option<String>,

    /// Reasoning effort. Gemini thinks at low, medium or high (higher
    /// efforts are high).
    #[arg(short, long, value_enum, default_value_t = Effort::Xhigh)]
    effort: Effort,

    /// Fast mode: Anthropic's on Claude Opus 5.5, priced at 2x (Claude
    /// Code's own fast mode for cc: models), or OpenAI's, priced at 2x with
    /// an API key and using more of the plan with a ChatGPT login. Gemini
    /// and OpenRouter have none.
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

    /// Anthropic's 1-hour prompt cache for the Claude models (by API key
    /// or on OpenRouter), instead of the 5-minute one: the cache outlasts
    /// pauses of up to an hour, and a cache write costs 2x the input price
    /// instead of 1.25x. Claude Code chooses its own (an hour on a Claude
    /// plan). Default: ARKO_CACHE_1H (1 or 0, in the environment or a .env
    /// file).
    #[arg(long = "cache-1h", conflicts_with = "no_cache_1h")]
    cache_1h: bool,

    /// The 5-minute prompt cache, whatever ARKO_CACHE_1H says.
    #[arg(long = "no-cache-1h")]
    no_cache_1h: bool,

    /// The main agent ends each turn with a plain reply. This is the
    /// default when you take part in the session (the full screen, the line
    /// prompt or --remote).
    #[arg(long = "text-replies", conflicts_with = "no_text_replies")]
    text_replies: bool,

    /// The main agent ends each turn by calling complete_task, with its
    /// reply in the call. This is the default with -p, where no one is
    /// there to say "go on" after a reply that wasn't meant to be its last.
    #[arg(long = "no-text-replies")]
    no_text_replies: bool,

    /// Every agent gets the extra rules for small models in its
    /// instructions, whatever its model. By default only agents on small
    /// models (Haiku, luna) get them.
    #[arg(long = "extra-rules", conflicts_with = "no_extra_rules")]
    extra_rules: bool,

    /// No agent gets the extra rules for small models.
    #[arg(long = "no-extra-rules")]
    no_extra_rules: bool,

    /// Which of the project's instruction files the agents get. The main
    /// agent gets them once per conversation, in its first message, and
    /// each worker once, in its first message. Default: one. When
    /// resuming, it changes the session's choice, as /project-instructions
    /// does.
    #[arg(long, value_enum, value_name = "FILES")]
    project_instructions: Option<project::Files>,

    /// Workers get the project's instructions too, once each, in their
    /// first message. This is the default. When resuming, it gives them to
    /// the workers started from then on.
    #[arg(
        long = "worker-project-instructions",
        conflicts_with = "no_worker_project_instructions"
    )]
    worker_project_instructions: bool,

    /// Workers don't get the project's instructions. Only the main agent
    /// does.
    #[arg(long = "no-worker-project-instructions")]
    no_worker_project_instructions: bool,

    /// How many workers may run at once. Default: no limit. 0 turns them
    /// off.
    #[arg(long)]
    workers: Option<u32>,

    /// Input size, in tokens, at which an agent writes a handover report and
    /// continues in a fresh conversation. On cc: models, Claude Code
    /// compacts the conversation at that size instead, and the agent goes on
    /// in the same conversation. Default: 500000 on Claude models,
    /// 250000 for OpenAI models on the ChatGPT login (whose backend accepts
    /// 272000), and 200000 on others, at most 80% of what the model accepts
    /// (100000 when OpenRouter's catalog can't say). When resuming, it sets
    /// the main agent's size as /handover-at does (up to the size the
    /// session started with).
    #[arg(long)]
    handover_at: Option<usize>,

    /// Let commands and the file tools delete, move and overwrite paths
    /// outside the working directory too (protected paths, such as the
    /// home directory itself, stay refused). It also turns outside access
    /// on in a resumed session, and /outside turns it on and off as you go.
    #[arg(long)]
    allow_outside_workspace: bool,

    /// Move a shell command still running after this long to the
    /// background, where it goes on as a job while the agent goes on with
    /// its work: 90s, 10m, 1h (at most 60m), or off. Default: 10m in a
    /// session you take part in, off with -p. For a new or resumed session,
    /// which keeps it. With -p it is off unless this flag sets it.
    #[arg(long, value_name = "DURATION|off", value_parser = shell::parse_background_after)]
    background_after: Option<u64>,

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

    /// A Codex auth.json with the ChatGPT login. Default: ARKO_CODEX_AUTH,
    /// else $CODEX_HOME/auth.json, else ~/.codex/auth.json.
    #[arg(long)]
    codex_auth: Option<PathBuf>,

    /// How a new session reaches OpenAI models: chatgpt (the ChatGPT login,
    /// on your plan) or api-key (OPENAI_API_KEY, billed per token). The
    /// session keeps it. Default: ARKO_OPENAI_AUTH, else the ChatGPT login
    /// when there is one, else the API key.
    #[arg(long, value_enum)]
    openai_auth: Option<OpenAiAuth>,

    /// The shell commands run in on Windows: cmd (cmd.exe) or powershell
    /// (PowerShell 7 where installed, else Windows PowerShell). Default:
    /// ARKO_SHELL, else cmd. Elsewhere commands run in bash (else sh).
    #[arg(long, value_enum)]
    shell: Option<WindowsShell>,

    /// Open the setup screen first: paste an API key (Anthropic, OpenAI,
    /// Gemini, OpenRouter or Voyage), give the path to a Codex auth.json, or
    /// check Claude Code. What you enter is saved in Arko's folder.
    #[arg(long)]
    setup: bool,

    /// Start the MCP servers in this file too (repeatable), in Claude
    /// Code's format or arko_agents'. A server named here is taken over one of
    /// the same name in the project's .mcp.json or Arko's mcp.json.
    #[arg(long = "mcp-config", value_name = "FILE")]
    mcp_config: Vec<PathBuf>,

    /// Serve a web page for following this session and taking part in it
    /// from a browser, such as a phone's. ADDR is a port, an address, or
    /// both: default 127.0.0.1:8765 (this machine only), and 0.0.0.0 lets
    /// in the other devices on the network. Arko prints the link, which holds
    /// the session's key. No TLS: on a network you don't trust, reach it
    /// through an SSH tunnel or Tailscale.
    #[arg(
        long,
        value_name = "ADDR",
        num_args = 0..=1,
        default_missing_value = "127.0.0.1",
        conflicts_with = "prompt"
    )]
    remote: Option<String>,

    /// Use the line prompt instead of the full-screen view.
    #[arg(long, conflicts_with = "prompt")]
    repl: bool,

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
            eprintln!("arko: {error}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(run(args));
    // The line editor's thread may still be reading, in raw mode.
    term::restore();
    let code = match code {
        Ok(code) => code,
        Err(message) => {
            eprintln!("arko: {message}");
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
    // fin's data folder, made before the rename to Arko, becomes Arko's
    // before anything reads it (keys, sessions, indexes).
    if let Some(note) = session::move_fin_folder() {
        eprintln!("arko: {note}");
    }
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
    let remote_bind = args.remote.as_deref().map(remote::parse_bind).transpose()?;

    // The session: resumed with the settings it was started with, or new.
    let resumed = if let Some(id) = &args.resume {
        Some(Session::open(&root, id)?)
    } else if args.continue_latest {
        let workspace = workspace(&args)?;
        Some(session::latest_in(&root, &workspace).ok_or_else(|| {
            format!(
                "no session to continue in {}. `arko --sessions` lists them",
                workspace.display()
            )
        })?)
    } else {
        None
    };
    let (session, settings, new) = match resumed {
        Some(session) => {
            let mut settings = session.load_settings()?;
            if args.model.is_some()
                || args.worker_model.is_some()
                || args.fast
                || args.cache_1h
                || args.no_cache_1h
                || args.shell.is_some()
                || args.openai_auth.is_some()
            {
                eprintln!(
                    "arko: a resumed session keeps the models and shell it was started with ({})",
                    settings.describe(&settings.main)
                );
            }
            if args.text_replies || args.no_text_replies {
                eprintln!(
                    "arko: a resumed session keeps how its main agent ends its turns: {}",
                    agents::replies_text(settings.text_replies)
                );
            }
            if args.extra_rules || args.no_extra_rules {
                eprintln!(
                    "arko: a resumed session keeps its extra rules for small models: {}",
                    settings.extra_rules.describe()
                );
            }
            if !settings.workspace.is_dir() {
                return Err(format!(
                    "the session's working directory {} no longer exists",
                    settings.workspace.display()
                ));
            }
            // The project's instructions can change at any time, and so can
            // when commands move to the background.
            let background = args.background_after.is_some_and(|after| {
                std::mem::replace(&mut settings.background_after, after) != after
            });
            if project_from_args(&args, &mut settings) | background {
                session
                    .save_settings(&settings)
                    .map_err(|error| format!("could not save the session: {error}"))?;
            }
            (Some(session), settings, false)
        }
        None => {
            if args.model.is_none() && default_model(&args).is_none() && can_ask {
                credentials::setup(Some(&Need::Any))?;
            }
            let mut settings = new_settings(&args)?;
            let notes = providers::check_openrouter(
                &mut settings,
                args.handover_at.is_some(),
                providers::openrouter_url(),
            )
            .await?;
            for note in notes {
                eprintln!("arko: {note}");
            }
            (None, settings, true)
        }
    };

    // Sessions saved before the shell could be chosen ran PowerShell.
    let shell = Shell::detect(settings.windows_shell.unwrap_or(WindowsShell::Powershell));
    if new && args.shell.is_some() && !cfg!(windows) {
        eprintln!(
            "arko: --shell picks the shell on Windows. Here commands run in {}",
            shell.name
        );
    }

    // Missing credentials, or an auth file that fails, are asked for.
    let client = loop {
        match providers::client(args.codex_auth.as_deref(), &settings) {
            Ok(client) => break Arc::new(client),
            Err(Failure::Need(need)) if can_ask => credentials::setup(Some(&need))?,
            Err(Failure::Need(need)) => return Err(need.message()),
            Err(Failure::Other(message)) => return Err(message),
        }
    };
    let provider_setup = providers::Setup::new(settings.openai_auth);
    let agent_settings = AgentSettings {
        main: &settings.main,
        worker: &settings.worker,
        providers: &provider_setup,
        max_workers: settings.max_workers,
        handover_at: settings.handover_at,
        show_thinking: settings.show_thinking,
        mcp_tools: &[],
        mcp_servers: &[],
        skill_folders: &[],
        index_tools: &[],
        indexes: &[],
        text_replies: settings.text_replies,
        extra_rules: settings.extra_rules,
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
        // The next session's default; see `providers::LastModels::remember`.
        if args.prompt.is_none()
            && let Err(error) = providers::LastModels::remember(&root, &settings)
        {
            eprintln!("arko: could not keep this session's models for the next one: {error}");
        }
    }
    start_log(&session);

    let managers = Managers::open(&session, &shell)?;
    // Outside access as the session left it; the flag turns it on below.
    let outside = Arc::new(outside::Outside::new(
        settings.allow_outside_workspace,
        managers.list(),
        settings.workspace.clone(),
    ));
    let one_shot = args.prompt.is_some();
    managers
        .background
        .set_after(background_after_for(&args, settings.background_after));

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
    let keys = keys::Keys::new(
        client.clone(),
        keys::openai_way(&client).await,
        indexes.clone(),
        Some(feed.clone()),
    );
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
    // The full-screen view is the front end with a terminal (not with
    // --repl); it reads the input itself.
    let full_screen = !one_shot && !args.repl && tui::available();
    let printer = if one_shot {
        None
    } else {
        term::save();
        if full_screen {
            None
        } else {
            ui::spawn_input(
                root.join("history.txt"),
                input_sender.clone(),
                Some(keys.clone()),
            )
        }
    };
    let interactive = printer.is_some() || full_screen;
    let output = Arc::new(Output::new(printer, one_shot));
    let stats = Arc::new(Mutex::new(Stats::default()));
    let (remote_actions, remote_input) = mpsc::unbounded_channel();
    let handover = Arc::new(handover::Handover::new(
        runs,
        settings
            .main_handover_at
            .or(settings.main.handover_at)
            .unwrap_or(settings.handover_at),
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
        extra_rules: Default::default(),
        project: Default::default(),
        background: managers.background.clone(),
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
        providers: provider_setup.clone(),
        session,
        client,
        keys,
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
    harness.outside_at_start(new, args.allow_outside_workspace);
    if !new && let Some(tokens) = args.handover_at {
        harness.handover_at_start(tokens);
    }
    if one_shot && let Some(note) = &harness.unoffered {
        output.line(&output.warn(&format!("arko: {note}")));
    }
    let mut input = Inputs {
        receiver: input,
        pending: None,
        questions,
        woken,
        remote: remote_input,
        // With -p, Arko exits when the agent finishes, jobs or not.
        finished: (!one_shot).then(|| managers.jobs.subscribe_finished()),
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
        let screen = full_screen
            .then(|| {
                tui::start(
                    tui::Setup {
                        feed: harness.feed.clone(),
                        meter: harness.gauges.clone(),
                        input: input_sender.clone(),
                        root: root.clone(),
                        session_dir: harness.session.dir.clone(),
                        workspace: harness.settings.workspace.clone(),
                        handover_at: harness
                            .settings
                            .main
                            .handover_at
                            .unwrap_or(harness.settings.handover_at),
                        // (With what Arko said before the screen opened.)
                        banner: harness.header(lock(&MODEL_NOTE).take()),
                        keys: harness.keys.clone(),
                    },
                    &output,
                )
            })
            .flatten();
        harness.banner(new, screen.is_some());
        if let Some(bind) = remote_bind {
            harness.start_remote(bind).await;
        }
        if !new {
            harness.restart_unfinished(&mut input).await;
        }
        if !harness.exiting {
            harness.repl(&mut input).await;
        }
        if let Some(screen) = screen {
            screen.close().await;
        }
        ExitCode::SUCCESS
    };
    ui::stop_reading();
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
        "Session {} saved. Continue it with: arko --resume {}",
        harness.session.id, harness.session.id
    )));
    Ok(code)
}

/// Why the session doesn't run the last session's models, as Arko said
/// before the session started: the full-screen view opens over it, so its
/// banner says it again.
static MODEL_NOTE: Mutex<Option<String>> = Mutex::new(None);

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

/// Whether a new session's main agent ends its turns with a reply in plain
/// text: as `--text-replies` or `--no-text-replies` says, else when the
/// user takes part in the session as it runs, at the full screen or the
/// line prompt (`terminal`: standard input and output are a terminal) or
/// on the web page. Not with -p, nor with lines piped in (each runs as a
/// turn of its own): with no one there to say "go on", a reply that wasn't
/// meant to end the work would end the run.
fn text_replies_for(args: &Args, terminal: bool) -> bool {
    if args.text_replies || args.no_text_replies {
        return args.text_replies;
    }
    args.prompt.is_none() && (args.remote.is_some() || terminal)
}

/// After how many seconds this run's shell commands move to the background
/// (0: never): as the flag says, else never with -p (Arko exits when the
/// agent finishes, jobs or not), else as the session keeps it (`saved`).
fn background_after_for(args: &Args, saved: u64) -> u64 {
    match args.background_after {
        Some(after) => after,
        None if args.prompt.is_some() => 0,
        None => saved,
    }
}

/// Which agents of a new session get the extra rules for small models: as
/// `--extra-rules` (all) or `--no-extra-rules` (none) says, else those on
/// small models.
fn extra_rules_for(args: &Args) -> ExtraRules {
    if args.extra_rules {
        ExtraRules::All
    } else if args.no_extra_rules {
        ExtraRules::Off
    } else {
        ExtraRules::Small
    }
}

/// The project's instructions as the flags choose them, over `settings`'
/// own: a new session's defaults (one file, workers get it), or a resumed
/// session's. Returns whether they changed.
fn project_from_args(args: &Args, settings: &mut Settings) -> bool {
    let before = (
        settings.project_instructions,
        settings.worker_project_instructions,
    );
    if let Some(files) = args.project_instructions {
        settings.project_instructions = files;
    }
    if args.worker_project_instructions {
        settings.worker_project_instructions = true;
    } else if args.no_worker_project_instructions {
        settings.worker_project_instructions = false;
    }
    (
        settings.project_instructions,
        settings.worker_project_instructions,
    ) != before
}

fn new_settings(args: &Args) -> Result<Settings, String> {
    let workspace = workspace(args)?;
    let name = |name: &str| Ok::<_, String>(providers::model_name(&resolve_model(name)?));
    let default = match &args.model {
        Some(_) => None,
        None => Some(default_model(args).ok_or_else(|| Need::Any.message())?),
    };
    let main_model = match (&args.model, &default) {
        (Some(model), _) => name(model)?,
        (None, Some(default)) => name(&default.main)?,
        (None, None) => unreachable!("a default without --model"),
    };
    // The 1-hour cache is Anthropic's, by API key or on OpenRouter; other
    // models ignore it.
    let want_1h = if args.cache_1h || args.no_cache_1h {
        args.cache_1h
    } else {
        credentials::switch(credentials::CACHE_1H)?.unwrap_or(false)
    };
    let cache_1h = |id: &str| {
        want_1h && resolve_model(id).is_ok_and(|model| providers::takes_cache_ttl(&model))
    };
    // The last session's workers' model goes with its main model.
    let worker_model = match &args.worker_model {
        Some(model) => Some(name(model)?),
        None => default
            .as_ref()
            .and_then(|default| default.worker.as_deref())
            .map(name)
            .transpose()?,
    };
    // OpenAI models are all reached one way: the session keeps it.
    let uses_openai = std::iter::once(&main_model)
        .chain(&worker_model)
        .any(|id| resolve_model(id).is_ok_and(|model| model.provider == arko_ai::Provider::OpenAI));
    let openai_auth = if uses_openai {
        let (auth, both) =
            providers::choose_openai_auth(args.openai_auth, args.codex_auth.as_deref())?;
        if both {
            eprintln!(
                "arko: this session reaches OpenAI models over your ChatGPT login (your plan), which comes before OPENAI_API_KEY. --openai-auth api-key (or ARKO_OPENAI_AUTH=api-key in a .env file) starts sessions on the API key instead, billed per token"
            );
        }
        Some(auth)
    } else {
        if args.openai_auth.is_some() {
            eprintln!("arko: --openai-auth is for OpenAI models, and this session has none");
        }
        None
    };
    // On the ChatGPT login, OpenAI models hand over below its backend's
    // input limit.
    let handover_at = |id: &str| {
        args.handover_at.or_else(|| {
            resolve_model(id)
                .ok()
                .map(|model| agents::default_handover(&model, openai_auth))
        })
    };
    let main = ModelSettings {
        cache_1h: cache_1h(&main_model),
        handover_at: handover_at(&main_model),
        model: main_model,
        effort: args.effort,
        fast: args.fast,
    };
    // A workers' model of their own (`--worker-model`, or the last
    // session's) doesn't follow the main agent's.
    let own_worker_model = worker_model.is_some();
    let worker = match worker_model {
        Some(model) => ModelSettings {
            cache_1h: cache_1h(&model),
            handover_at: handover_at(&model),
            model,
            effort: args.worker_effort.unwrap_or(args.effort),
            fast: args.worker_fast,
        },
        None => ModelSettings {
            effort: args.worker_effort.unwrap_or(main.effort),
            ..main.clone()
        },
    };
    // What sessions read before the handover was set per model.
    let session_handover = main.handover_at.unwrap_or(200_000);
    if args.cache_1h && !main.cache_1h && !worker.cache_1h {
        eprintln!(
            "arko: --cache-1h is for Claude models by API key or on OpenRouter (Claude Code chooses its own). This session has none, so it is ignored"
        );
    }
    let text_replies = {
        use std::io::IsTerminal;
        text_replies_for(
            args,
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        )
    };
    let mut settings = Settings {
        text_replies,
        extra_rules: extra_rules_for(args),
        project_instructions: Default::default(),
        worker_project_instructions: true,
        workspace,
        main,
        worker,
        max_workers: args.workers.unwrap_or(NO_LIMIT),
        handover_at: session_handover,
        main_handover_at: None,
        allow_outside_workspace: args.allow_outside_workspace,
        show_thinking: args.show_thinking,
        created: chrono::Utc::now(),
        windows_shell: Some(windows_shell(args)?),
        title: args.prompt.as_deref().map(|prompt| ui::cut(prompt, 80)),
        openai_auth,
        own_worker_model,
        background_after: background_after_for(args, shell::DEFAULT_BACKGROUND_AFTER),
    };
    project_from_args(args, &mut settings);
    providers::check_openai_choices(&settings)?;
    // The model Arko chose: the last session's, else one the credentials
    // allow.
    if let Some(default) = &default {
        let mut line = format!("This session runs {}.", settings.describe(&settings.main));
        if let Some(reason) = &default.reason {
            line = format!("{line} {reason}");
        }
        eprintln!("arko: {line}");
        *lock(&MODEL_NOTE) = default.reason.clone();
    }
    Ok(settings)
}

/// The shell a new session runs commands in on Windows: --shell, else
/// ARKO_SHELL, else cmd.exe.
fn windows_shell(args: &Args) -> Result<WindowsShell, String> {
    if let Some(shell) = args.shell {
        return Ok(shell);
    }
    match credentials::var(credentials::SHELL) {
        None => Ok(WindowsShell::Cmd),
        Some(value) => <WindowsShell as clap::ValueEnum>::from_str(value.trim(), true)
            .map_err(|_| format!("ARKO_SHELL is `{value}`. Set it to cmd or powershell")),
    }
}

/// The models of a new session without --model; `None` without
/// credentials.
fn default_model(args: &Args) -> Option<providers::DefaultModels> {
    providers::default_models(args.codex_auth.as_deref(), &session::data_root().ok()?)
}

/// Logs go to the session's `arko.log`, not the terminal. `RUST_LOG` sets
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

/// The session's terminal: its manager runs the terminal tools' background
/// jobs and the `shell` tool's commands (held, with the shell's own
/// arguments, until one moves to the background) and keeps their logs in
/// the session's folder.
struct Managers {
    jobs: Arc<TerminalManager>,
    /// When the shell's commands move to the background, and those running.
    background: Arc<shell::Background>,
}

impl Managers {
    /// Opens it with outside access off ([`outside::Outside`] sets it) and
    /// the shell's commands kept in the foreground: set
    /// [`background`](Self::background) before making the tools.
    fn open(session: &Session, shell: &Shell) -> Result<Self, String> {
        let mut config = TerminalConfig::new(session.jobs_dir());
        config.shell = shell.program.clone();
        config.shell_args = shell.job_args.clone();
        let jobs = TerminalManager::new(config).map_err(|error| {
            format!("could not open the session's terminal ({error}). Is it open in another Arko?")
        })?;
        Ok(Self {
            jobs,
            background: shell::Background::new(0),
        })
    }

    fn list(&self) -> Vec<Arc<TerminalManager>> {
        vec![self.jobs.clone()]
    }

    /// The shell tool, which keeps the command channels of the runs it is
    /// bound to in `runs`, and the terminal tools.
    fn tools(&self, shell: &Shell, runs: Arc<handover::Runs>) -> Vec<Arc<dyn Tool>> {
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
    async fn shutdown(&self) {
        self.jobs.shutdown().await;
    }
}

/// The workflow's name in new sessions.
const WORKFLOW_NAME: &str = "arko";

/// What a session's checkpoint was saved with: the workflow's name and the
/// agents' configurations. A resume needs both unchanged, so a session
/// keeps them when Arko changes: its agents' instructions, or its name
/// (sessions saved before the rename to Arko have `fin`).
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
/// it: the tools Arko has now that their saved tool lists leave out, and
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
    /// A background job of the agent's finished, without a message from
    /// the user: its terminal update follows.
    Jobs,
    /// A turn Arko stopped in the middle of goes on, without a message.
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
    /// What the agents' configurations take from the providers (how the
    /// session reaches OpenAI models, how Claude Code runs): `/model`
    /// builds them again with it.
    providers: providers::Setup,
    session: Session,
    client: Arc<LLMClient>,
    /// The API keys: what the client and the search indexes use, and which
    /// providers the session reaches (`/keys`).
    keys: Arc<keys::Keys>,
    main_config: arko_agents::AgentConfig,
    worker_config: arko_agents::AgentConfig,
    tools: Vec<Arc<dyn Tool>>,
    /// Arko's folder.
    root: PathBuf,
    /// The MCP servers started, and what was left out and why.
    mcp: mcp::Running,
    mcp_notes: Vec<String>,
    /// The skill folders that exist.
    skill_folders: Vec<PathBuf>,
    /// The search indexes; `None` when they couldn't be opened.
    indexes: Option<Arc<index::Indexes>>,
    /// In a resumed session: the tools (and skills) Arko has that the
    /// session's agents aren't offered, as they started without them.
    unoffered: Option<String>,
    output: Arc<Output>,
    stats: Arc<Mutex<Stats>>,
    workflow: Option<Arc<Workflow>>,
    printer: Option<(mpsc::UnboundedSender<Control>, tokio::task::JoinHandle<()>)>,
    conversation: Conversation,
    /// The session's conversations, and which one the next message goes
    /// to (`/new`, `/resume`).
    conversations: conversations::Conversations,
    /// The project's instruction files, as Arko found them (see `project`).
    project: project::Found,
    /// What the user typed during a turn that the agent didn't read before
    /// the turn ended: it goes with the next message.
    unread: Vec<String>,
    /// The conversation changed (`/new`, `/resume`): the next message names
    /// the workers still running, whose reports now come to it.
    switched: bool,
    /// Without a terminal, lines read during a turn wait for their own
    /// turns instead of reaching the agent as notes.
    queue_lines: bool,
    /// The user quit while a turn ran before the prompt opened.
    exiting: bool,
    /// Native workers. None when disabled or when resuming a legacy session.
    crew: Option<Arc<Crew>>,
    /// Puts the main agent's questions to the user.
    asker: Arc<Asker>,
    /// Arko sends one message and exits (-p).
    one_shot: bool,
    /// `/exit` was typed with workers running: the next one quits.
    exit_warned: bool,
    /// The user was told that the workers' news waits for their message.
    news_hint_shown: bool,
    /// What the remote page shows: the workflows' events, and what Arko
    /// says of the turns.
    feed: remote::Feed,
    /// The remote page's server, while it runs.
    remote: Option<remote::Server>,
    /// Where the page's actions go; [`Inputs`] reads them.
    remote_actions: mpsc::UnboundedSender<Action>,
    /// Where the page was last asked for, and the port it was served on:
    /// `/remote` serves it there again.
    remote_bind: Option<(remote::Bind, u16)>,
    /// The figures of the page's status.
    gauges: Arc<Gauges>,
    /// Outside access, which the user turns on and off (`/outside`).
    outside: Arc<outside::Outside>,
    /// The main agent's handover size, and the watch that asks for its
    /// handovers (`/handover`, `/handover-at`).
    handover: Arc<handover::Handover>,
    /// When the shell's commands move to the background, and those running
    /// (`/background-after`, Ctrl+B).
    background: Arc<shell::Background>,
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
        let main_config = config(MAIN, &self.main_config);
        // A main agent that ends its turns with a reply in text answers in
        // text beside its calls: it has no `message_user`. One that calls
        // `complete_task` (as every session saved before text replies
        // does) keeps it.
        let answers_with_tool = !main_config.tool_config.ends_task_with_text();
        let mut builder = Workflow::builder()
            .name(name)
            .workspace(&self.settings.workspace)
            .llm_client(self.client.clone())
            .agent(MAIN, main_config)
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
            builder = builder.agent(WORKER, config(WORKER, &self.worker_config));
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
        if let Some(checkpoint) = checkpoint {
            builder = builder.resume_from(checkpoint);
            // The saved workers' runs are checked as the workflow is built.
            if let Some(crew) = &self.crew {
                crew.detach();
            }
        }
        let workflow = Arc::new(builder.build().map_err(|error| error.to_string())?);
        let mut configs = vec![config(MAIN, &self.main_config)];
        if self.settings.max_workers > 0 {
            configs.push(config(WORKER, &self.worker_config));
        }
        self.outside.attach(&workflow, &configs);
        self.handover.watch(&workflow, &configs[0]);
        let (control, controls) = mpsc::unbounded_channel();
        let task = tokio::spawn(ui::print_events(
            workflow.subscribe_events(),
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
        if let Some(crew) = &self.crew {
            crew.attach(&workflow, lead);
            if resume {
                workflow.resume_subagents();
            }
        }
        self.workflow = Some(workflow);
        self.publish_conversations();
        Ok(())
    }

    /// What the full-screen view's banner shows: the session's top-level
    /// facts, and its notes (`notes` first: what Arko said before the
    /// session started).
    fn header(&self, model_note: Option<String>) -> tui::Banner {
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
        tui::Banner {
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
    fn banner(&self, new: bool, boxed: bool) {
        if !boxed {
            self.loose_banner();
        }
        self.banner_end(new);
    }

    /// The facts as lines, as the line prompt shows them.
    fn loose_banner(&self) {
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
    fn banner_end(&self, new: bool) {
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

    /// The workers, as the banner and `/session` say it.
    /// When the agents hand over, as `/session` says it.
    fn handover_summary(&self) -> String {
        let what = |compacts: bool| if compacts { "compaction" } else { "handover" };
        let main = format!(
            "{} at {}k tokens",
            what(self.handover.compacts()),
            self.handover.at() / 1000
        );
        let worker = format!(
            "{} at {}k tokens",
            what(agents::compacts(&self.worker_config)),
            self.settings
                .worker
                .handover_at
                .unwrap_or(self.settings.handover_at)
                / 1000
        );
        if self.settings.max_workers == 0 || main == worker {
            format!("{main} of input")
        } else {
            format!("{main} of input (main), {worker} (workers)")
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
    async fn restart_unfinished(&mut self, input: &mut Inputs) {
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

    async fn repl(&mut self, input: &mut Inputs) {
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
                Event::Input(None | Some(Input::Eof)) => {
                    // Without a terminal, the workers finish first.
                    if self.queue_lines {
                        self.finish_workers(input).await;
                    }
                    break;
                }
                Event::Input(Some(Input::Interrupt)) => {
                    if last_interrupt.is_some_and(|at| at.elapsed() < Duration::from_secs(3)) {
                        break;
                    }
                    last_interrupt = Some(Instant::now());
                    self.output
                        .line(&self.output.dim("(Ctrl-C again, Ctrl-D or /exit to quit)"));
                    continue;
                }
                Event::Input(Some(Input::Line(line))) => {
                    (input.coalesce(line).await, Source::Terminal)
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
            if text.is_empty() {
                continue;
            }
            if let Some(command) = command(text) {
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
                    "/exit" | "/quit" => harness.may_exit(),
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
            self.feed.user(text, source, Delivery::Turn);
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
    fn say(&self, level: Level, text: &str) {
        let out = &self.output;
        out.echo(&match level {
            Level::Info => out.dim(text),
            Level::Warn => out.warn(text),
            Level::Error => out.error(text),
        });
        self.feed.notice(level, text);
    }

    /// Shows in the terminal what was typed on the remote page.
    fn echo_web(&self, text: &str) {
        let out = &self.output;
        out.echo(&format!(
            "{} {}",
            out.accent(&format!("web {}", ui::PROMPT.trim_end())),
            text.trim().replace('\n', "\n  ")
        ));
    }

    /// Whether the command can't be run from where it was typed: the page
    /// can't quit Arko or turn itself off.
    fn refused(&self, command: &str, source: Source) -> bool {
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
    fn mirrored<R>(&mut self, run: impl FnOnce(&mut Self) -> R) -> R {
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
    async fn remote_command(&mut self, command: &str) {
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
    fn remote_key(&self, new: bool) -> Result<String, String> {
        let path = self.session.dir.join(remote::KEY_FILE);
        if new {
            remote::new_key(&path)
        } else {
            remote::session_key(&path)
        }
    }

    /// Serves the web page where `bind` says, in place of the one running,
    /// and prints its links.
    async fn start_remote(&mut self, bind: remote::Bind) {
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
                self.announce_remote(server.addr(), &key);
                self.remote_bind = Some((bind, server.addr().port()));
                tui::page_at(Some(server.addr()));
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
    async fn stop_remote(&mut self) {
        if let Some(server) = self.remote.take() {
            tui::page_at(None);
            server.stop().await;
        }
    }

    /// Prints the links to the page (with the key: never logged), best
    /// first, and a QR code of the first that other devices can open.
    fn announce_remote(&self, addr: std::net::SocketAddr, key: &str) {
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
    fn remote_qr(&self, interface: &str) {
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
    fn live_command(&mut self, command: &str, source: Source) -> bool {
        let Some((name, argument)) = split_live_command(command) else {
            return false;
        };
        self.feed.user(command, source, Delivery::Command);
        match name {
            "/outside" => self.outside_command(argument, source),
            "/model" => self.model_command(argument, source),
            "/keys" => self.keys_command(argument, source),
            "/handover" => self.handover_command(argument, source),
            "/text-replies" => self.text_replies_command(argument, source),
            "/extra-rules" => self.extra_rules_command(argument, source),
            "/project-instructions" => self.project_command(argument, source),
            "/background-after" => self.background_after_command(argument, source),
            _ => self.handover_at_command(argument, source),
        }
        true
    }

    /// `/handover`: the main agent hands over at its next step, or when
    /// its next turn starts. In Claude Code, Claude Code compacts its
    /// conversation then instead.
    fn handover_command(&mut self, argument: &str, source: Source) {
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

    /// `/handover-at`: the size the main agent hands over at, and with a
    /// size, sets it.
    fn handover_at_command(&mut self, argument: &str, source: Source) {
        if argument.is_empty() {
            let compacts = self.handover.compacts();
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
            text.push_str(if compacts {
                ". /handover-at 200k sets another size, and /handover compacts now."
            } else {
                ". /handover-at 200k sets another size, and /handover hands over now."
            });
            self.mirrored(|harness| harness.output.line(&text));
            return;
        }
        let tokens = match handover::parse_tokens(argument) {
            Ok(tokens) => tokens,
            Err(error) => {
                self.mirrored(|harness| harness.output.line(&harness.output.error(&error)));
                return;
            }
        };
        let set = self.set_handover_at(tokens);
        if source == Source::Web {
            self.say(
                Level::Info,
                &format!(
                    "The handover size was set to {} from the web page.",
                    handover::shown(set)
                ),
            );
        }
        let lines = self.handover_set_lines(tokens, set);
        self.mirrored(|harness| {
            for line in &lines {
                harness.output.line(line);
            }
        });
    }

    /// Asks the main agent for a handover: at its next step while it
    /// works, else as its next turn starts. `None` when there is no
    /// conversation to hand over.
    fn ask_handover(&self) -> Option<handover::Asked> {
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

    /// `--handover-at` when resuming: the size the main agent hands over
    /// at from now on.
    fn handover_at_start(&mut self, tokens: usize) {
        let set = self.set_handover_at(tokens);
        for line in self.handover_set_lines(tokens, set) {
            self.say(Level::Info, &line);
        }
    }

    /// Sets the size the main agent hands over at, up to the one its
    /// configuration hands over at by itself, and keeps it in the session.
    /// Returns the size set.
    fn set_handover_at(&mut self, tokens: usize) -> usize {
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
    fn handover_set_lines(&self, tokens: usize, set: usize) -> Vec<String> {
        let compacts = self.handover.compacts();
        let configured = handover::shown(self.handover.configured());
        let shown = handover::shown(set);
        let mut lines = vec![match (set < tokens, compacts) {
            (true, false) => format!(
                "This session's main agent hands over at {configured} at the latest: its conversation keeps the size it was started with. It now hands over at {shown}."
            ),
            (true, true) => format!(
                "This session's main agent compacts at {configured} at the latest: its conversation keeps the size it was started with. It now compacts at {shown}."
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
    fn outside_command(&mut self, argument: &str, source: Source) {
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
    fn outside_at_start(&mut self, new: bool, flag: bool) {
        if new && self.outside.is_on() {
            self.tell_agents(&self.outside.note());
        } else if !new && flag {
            self.set_outside(true);
        }
    }

    /// Turns outside access on or off: the command guard, the file tools
    /// and new workers follow at once, the session keeps it, and the agents
    /// get a note. Returns whether it changed.
    fn set_outside(&mut self, on: bool) -> bool {
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
    fn tell_agents(&self, text: &str) {
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
    fn background_after_command(&mut self, argument: &str, source: Source) {
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
    fn set_background_after(&mut self, after: u64) {
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
                let plans = self.stats.lock().unwrap().plans.clone();
                let ledger = self.workflow().cost_ledger();
                for line in providers::cost_report(&ledger, &self.settings, &plans) {
                    match line.detail {
                        true => out.line(&out.dim(&format!("  {}", line.text))),
                        false => out.line(&line.text),
                    }
                }
                if let Some(indexes) = &self.indexes {
                    let (tokens, dollars) = indexes.spent();
                    if tokens > 0 {
                        out.line(&format!(
                            "${dollars:.3} for embeddings (Voyage) while Arko ran · {}k tokens",
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
                    "Old workers are archived. Start a new Arko session for native workers. /new keeps this session's old tools.",
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
                    self.settings.describe(&self.settings.main),
                    if self.settings.max_workers == 0 {
                        "no workers".to_string()
                    } else {
                        format!(
                            "workers: {} ({})",
                            self.settings.describe(&self.settings.worker),
                            match self.settings.max_workers {
                                NO_LIMIT => "no limit".to_string(),
                                n => format!("up to {n} at once"),
                            }
                        )
                    }
                )));
                out.line(&out.dim(&self.handover_summary()));
                out.line(&out.dim(&format!(
                    "text replies {}: {}",
                    if self.settings.text_replies {
                        "on"
                    } else {
                        "off"
                    },
                    agents::replies_text(self.settings.text_replies)
                )));
                out.line(&out.dim(&format!(
                    "extra rules {}: {}",
                    self.settings.extra_rules.name(),
                    self.settings.extra_rules.describe()
                )));
                out.line(&out.dim(&self.project_summary()));
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
    fn list_mcp(&self) {
        let out = &self.output;
        if self.mcp.servers.is_empty() {
            out.line(&format!(
                "No MCP servers. Arko starts those set up in {}, in the project's {} (once you approve them) and in --mcp-config files.",
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
                "No skills. Arko reads them (folders with a SKILL.md) from the project's .agents/skills, .claude/skills and .skills, from {}, and from ~/.agents/skills and ~/.claude/skills.",
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
        // A turn Arko stopped in the middle of (a crash) goes on where it
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
                            (input.coalesce(line).await, Source::Terminal)
                        }
                        Event::Remote(Action::Message(text)) => {
                            self.echo_web(&text);
                            (text, Source::Web)
                        }
                        Event::Remote(Action::Answer { question, text }) => {
                            match answering.take() {
                                Some((id, answer)) if id == question => {
                                    self.echo_web(&text);
                                    let given = text.trim().to_string();
                                    if answer.send(given.clone()).is_ok() {
                                        self.output.echo(&self.output.dim("  ✎ answered"));
                                        self.feed.answered(id, &given, Source::Web);
                                    } else {
                                        self.feed.question_closed(id);
                                        self.feed.notice(Level::Warn, "That question is no longer asked.");
                                    }
                                }
                                other => {
                                    answering = other;
                                    self.feed.notice(Level::Warn, "That question is no longer asked.");
                                }
                            }
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
                    };
                    let text = line.trim().to_string();
                    if text.is_empty() {
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
                                harness.may_exit()
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
                        match workflow.post_note(Note::new(text.clone()).to([MAIN]).once()) {
                            Ok(seq) => {
                                self.output.echo(&self.output.dim(
                                    "  ✎ added to the task: the agent reads it at its next step",
                                ));
                                self.feed.user(&text, source, Delivery::Note);
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
        // The jobs that finished meanwhile told the agent as it worked.
        input.drain_finished();
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
                // With -p, a reply the workers' news will follow isn't the
                // answer.
                if self.one_shot && self.news_coming() {
                    self.output.line(result.summary.trim());
                } else {
                    self.output.answer(result.summary.trim());
                }
                self.feed.reply(&result.summary);
                self.keep_unread(&notes, &workflow);
                self.conversation = Conversation::Answered(result);
                self.track_conversations();
                TurnEnd::Answered
            }
            Err(WorkflowError::AgentStopped { .. } | WorkflowError::Cancelled) if stopping => {
                self.say(Level::Warn, "Stopped.");
                self.keep_unread(&notes, &workflow);
                self.track_conversations();
                self.conversation = conversation_of(&workflow, self.conversations.current_run());
                TurnEnd::Stopped
            }
            Err(error) => {
                self.say(Level::Error, &format!("The turn failed: {error}"));
                self.keep_unread(&notes, &workflow);
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
            if let TurnEnd::Exit = Box::pin(self.turn(Prompt::User(line), input)).await {
                return TurnEnd::Exit;
            }
        }
        end
    }

    /// Whether the workers have news for the main agent, or will have.
    fn news_coming(&self) -> bool {
        self.crew
            .as_ref()
            .is_some_and(|crew| !crew.running().is_empty() || crew.has_mail())
    }

    /// While the main agent's conversation waits for a message, the
    /// workers' news (their reports and messages) starts a turn of it.
    async fn deliver_news(&mut self, input: &mut Inputs) -> News {
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
    async fn deliver_finished(
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
    fn move_to_background(&self, agent: &str) {
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
                Event::Input(Some(Input::Line(_))) => {}
                Event::Input(Some(Input::Background(agent))) => self.move_to_background(&agent),
                Event::Remote(Action::Message(_) | Action::Answer { .. }) => {
                    self.feed.notice(
                        Level::Warn,
                        "Arko is only waiting for the workers now, so messages aren't taken.",
                    );
                }
                Event::Input(Some(Input::Interrupt | Input::Eof) | None)
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
    fn compose(&mut self, prompt: Prompt) -> String {
        let jobs = matches!(prompt, Prompt::Jobs);
        let message = match prompt {
            Prompt::Recover => return String::new(),
            Prompt::User(message) => Some(message),
            Prompt::News | Prompt::Jobs => None,
        };
        let mut parts = Vec::new();
        if matches!(self.conversation, Conversation::New)
            && let Some(project) = self.project.for_main(self.settings.project_instructions)
        {
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

    /// Notes the run never read go with the next message.
    fn keep_unread(&mut self, notes: &[(u64, String)], workflow: &Workflow) {
        let given = workflow
            .result()
            .agent_results
            .iter()
            .filter(|run| run.agent_name == MAIN)
            .map(|run| workflow.notes_given(run.agent_run_id))
            .max()
            .unwrap_or(0);
        let unread: Vec<String> = notes
            .iter()
            .filter(|(seq, _)| *seq > given)
            .map(|(_, text)| text.clone())
            .collect();
        if !unread.is_empty() {
            self.say(
                Level::Info,
                "  (the agent finished before it read what you typed, so it goes with your next message)",
            );
            self.unread.extend(unread);
        }
    }

    fn stop(&self, workflow: &Workflow) -> bool {
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
    fn recover_after_error(&mut self, workflow: &Workflow) {
        self.track_conversations();
        self.conversation = conversation_of(workflow, self.conversations.current_run());
        if matches!(self.conversation, Conversation::New) {
            self.say(
                Level::Info,
                "This conversation can't go on, so your next message starts it over, without what was said before.",
            );
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
        let parts = parts.join(" · ");
        self.output
            .echo(&self.output.dim(&format!("── {parts} ──")));
        self.feed.turn_ended(&parts);
    }
}

/// Where the conversation whose latest run is `run` stands: stopped (or
/// interrupted), answered, or neither (no run yet, or one that can't go
/// on: the next message starts one).
fn conversation_of(workflow: &Workflow, run: Option<uuid::Uuid>) -> Conversation {
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
fn command(text: &str) -> Option<&str> {
    (commands::find(text).is_some() || split_live_command(text).is_some()).then_some(text)
}

/// `/remote`, alone or with what follows it.
fn is_remote_command(text: &str) -> bool {
    text == "/remote" || text.starts_with("/remote ")
}

/// The link `/remote qr` shows: the first other devices can open, or the
/// one on `interface` (a name or an address). `Err` with what to tell the
/// user when there is none, of a server on `addr`.
fn qr_link<'a>(
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
fn place(link: &remote::Link) -> String {
    link.interface
        .as_ref()
        .map(|name| format!(" ({name}, {})", link.kind.label()))
        .unwrap_or_default()
}

/// The commands that change a setting while the session runs, which take
/// an argument: `/outside`, `/model`, `/handover`, `/handover-at`, `/keys`.
const LIVE_COMMANDS: [&str; 9] = [
    "/outside",
    "/model",
    "/handover",
    "/handover-at",
    "/keys",
    "/text-replies",
    "/extra-rules",
    "/project-instructions",
    "/background-after",
];

/// What the user is told of when shell commands move to the background,
/// after `after` seconds (0: never).
fn background_after_text(after: u64) -> String {
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
fn handover_text(asked: handover::Asked, compacts: bool) -> &'static str {
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
fn split_live_command(text: &str) -> Option<(&str, &str)> {
    let (name, argument) = text.split_once(' ').unwrap_or((text, ""));
    LIVE_COMMANDS
        .contains(&name)
        .then_some((name, argument.trim()))
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
    /// What the remote page sends.
    remote: mpsc::UnboundedReceiver<Action>,
    /// The terminal's jobs that finish in the background; `None` with -p,
    /// where they wake no one.
    finished: Option<tokio::sync::broadcast::Receiver<arko_agents::terminal::JobFinished>>,
}

/// What [`Inputs::next_event`] returns.
enum Event {
    /// An input; `None` once the input thread is gone.
    Input(Option<Input>),
    /// The main agent asks the user something.
    Question(Question),
    /// The crew woke Arko up.
    Woken,
    /// A job finished in the background.
    Finished(arko_agents::terminal::JobFinished),
    /// The remote page sent something.
    Remote(Action),
}

impl Inputs {
    /// The next input, question, action from the remote page, or (when
    /// `woken`) wake-up or job finished; cancel-safe.
    async fn next_event(&mut self, woken: bool) -> Event {
        if let Some(input) = self.pending.take() {
            return Event::Input(Some(input));
        }
        tokio::select! {
            input = self.receiver.recv() => Event::Input(input),
            Some(question) = self.questions.recv() => Event::Question(question),
            Some(action) = self.remote.recv() => Event::Remote(action),
            () = wake(&mut self.woken), if woken => Event::Woken,
            finished = job_finished(&mut self.finished), if woken => Event::Finished(finished),
        }
    }

    /// Forgets the wake-ups so far: what they were for is being seen to.
    fn drain_woken(&mut self) {
        if let Some(woken) = &mut self.woken {
            while woken.try_recv().is_ok() {}
        }
    }

    /// Forgets the jobs that finished so far: their updates reached the
    /// agent, or go with the turn starting.
    fn drain_finished(&mut self) {
        if let Some(finished) = &mut self.finished {
            while !matches!(
                finished.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty
                    | tokio::sync::broadcast::error::TryRecvError::Closed)
            ) {}
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

/// The next job that finishes in the background; never, without a
/// terminal to hear from. (Missed ones are told by the next: a turn they
/// start gets all their updates.)
async fn job_finished(
    finished: &mut Option<tokio::sync::broadcast::Receiver<arko_agents::terminal::JobFinished>>,
) -> arko_agents::terminal::JobFinished {
    if let Some(receiver) = finished {
        loop {
            match receiver.recv().await {
                Ok(job) => return job,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        *finished = None;
    }
    std::future::pending().await
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The session's figures for the remote page's status, read as the page
/// asks. The workflow and its worker view are refreshed on session resume.
struct Gauges {
    /// The agents' models, as the harness last published them.
    models: Mutex<remote::Models>,
    /// The conversations, as the harness last published them.
    conversations: Mutex<Vec<remote::ConversationEntry>>,
    workflow: Mutex<Option<Arc<Workflow>>>,
    crew: Mutex<Option<Arc<Crew>>>,
    stats: Arc<Mutex<Stats>>,
    outside: Arc<outside::Outside>,
    handover: Arc<handover::Handover>,
    /// The providers the session reaches, which a key added or removed
    /// changes.
    keys: Arc<keys::Keys>,
    /// The main agent ends its turns with a reply in plain text, as the
    /// harness last published it.
    text_replies: std::sync::atomic::AtomicBool,
    /// Which agents get the extra rules for small models, as the harness
    /// last published it.
    extra_rules: Mutex<ExtraRules>,
    /// Which of the project's instruction files the agents get, and whether
    /// workers do, as the harness last published them.
    project: Mutex<(project::Files, bool)>,
    /// When the shell's commands move to the background, and those running.
    background: Arc<shell::Background>,
}

impl Gauges {
    /// The agents running a shell command, as the clients name them:
    /// [`remote::MAIN_AGENT`], or a worker's ID.
    fn commands_running(&self) -> Vec<String> {
        let running = self.background.running();
        if running.is_empty() {
            return Vec::new();
        }
        let workers = lock(&self.workflow)
            .as_ref()
            .map(|workflow| workflow.subagents())
            .unwrap_or_default();
        // The runs that aren't workers' are the main agent's.
        let mut agents: Vec<String> = running
            .into_iter()
            .map(|run| {
                workers
                    .iter()
                    .find(|worker| worker.run_id == run)
                    .map_or_else(
                        || remote::MAIN_AGENT.to_string(),
                        |worker| worker.handle.clone(),
                    )
            })
            .collect();
        agents.sort();
        agents.dedup();
        agents
    }
}

impl remote::Meter for Gauges {
    fn read(&self) -> remote::Reading {
        let workflow = lock(&self.workflow).clone();
        let crew = lock(&self.crew).clone();
        let stats = lock(&self.stats).clone();
        let (project_instructions, worker_project_instructions) = *lock(&self.project);
        remote::Reading {
            cost: workflow
                .as_ref()
                .map_or(0.0, |workflow| workflow.context().budget.total_spent()),
            api_price: workflow.and_then(|workflow| providers::api_price(&workflow.cost_ledger())),
            context_tokens: stats.context_tokens,
            plan_usage: stats.plan_usage,
            workers_running: crew.as_ref().map(|crew| crew.running()).unwrap_or_default(),
            workers: crew
                .filter(|crew| !crew.is_empty())
                .map(|crew| crew.describe()),
            settings: remote::LiveSettings {
                outside: self.outside.is_on(),
                handover_at: self.handover.at(),
                text_replies: self.text_replies.load(std::sync::atomic::Ordering::Relaxed),
                extra_rules: *lock(&self.extra_rules),
                project_instructions,
                worker_project_instructions,
                background_after: self.background.after().map_or(0, |after| after.as_secs()),
            },
            commands: self.commands_running(),
            models: {
                let mut models = lock(&self.models).clone();
                models.reachable = self.keys.reachable();
                if models.openai_auth.is_none() {
                    models.openai_auth = self
                        .keys
                        .openai_auth()
                        .map(|auth| auth.describe().to_string());
                }
                models
            },
            conversations: lock(&self.conversations).clone(),
        }
    }
}

/// Prints through `to`, and keeps a copy of the lines: a command's output,
/// for the remote page.
struct Tee {
    to: Arc<Output>,
    /// Each line, with its level.
    lines: Arc<Mutex<Vec<(Option<Level>, String)>>>,
}

impl rustyline::ExternalPrinter for Tee {
    fn print(&mut self, text: String) -> rustyline::Result<()> {
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
const CANT_ASK: &str = "No one can answer questions in this session (Arko runs without a terminal, from a script or with -p). Decide yourself, go on, and say in your reply what you assumed.";

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

    #[test]
    fn commands_move_to_the_background_unless_with_p() {
        let args = |line: &[&str]| {
            Args::try_parse_from(std::iter::once("arko").chain(line.iter().copied())).unwrap()
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
            main: &opus,
            worker: &opus,
            providers: &providers::Setup::new(None),
            max_workers: NO_LIMIT,
            handover_at: 200_000,
            show_thinking: false,
            mcp_tools: &[],
            mcp_servers: &[],
            skill_folders: &[],
            index_tools: &[],
            indexes: &[],
            text_replies: false,
            extra_rules: Default::default(),
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
        let plain = AgentSettings {
            main: &opus,
            worker: &opus,
            providers: &providers::Setup::new(None),
            max_workers: 4,
            handover_at: 200_000,
            show_thinking: false,
            mcp_tools: &[],
            mcp_servers: &[],
            skill_folders: &[],
            index_tools: &[],
            indexes: &[],
            text_replies: false,
            extra_rules: Default::default(),
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
            indexes: &[],
            ..plain
        })
        .unwrap();
        assert_eq!(
            note,
            "This session started without the search index tools, the tools of the MCP servers github and db, and skills, so its agents don't get them. A new session does."
        );
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
        let settings = Settings {
            workspace: workspace.clone(),
            main: luna.clone(),
            worker: luna.clone(),
            max_workers: NO_LIMIT,
            handover_at: 200_000,
            main_handover_at: None,
            allow_outside_workspace: false,
            show_thinking: false,
            created: chrono::Utc::now(),
            title: None,
            windows_shell: None,
            openai_auth: None,
            own_worker_model: false,
            text_replies: false,
            extra_rules: Default::default(),
            project_instructions: Default::default(),
            worker_project_instructions: false,
            background_after: 0,
        };
        let agent_settings = AgentSettings {
            main: &luna,
            worker: &luna,
            providers: &providers::Setup::new(None),
            max_workers: NO_LIMIT,
            handover_at: 200_000,
            show_thinking: false,
            mcp_tools: &[],
            mcp_servers: &[],
            skill_folders: &[],
            index_tools: &[],
            indexes: &[],
            text_replies: false,
            extra_rules: Default::default(),
        };
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
        let background = shell::Background::new(0);
        let handover = Arc::new(handover::Handover::new(
            Arc::default(),
            200_000,
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
            extra_rules: Default::default(),
            project: Default::default(),
            background: background.clone(),
        });
        let mut harness = Harness {
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
            mcp_notes: Vec::new(),
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
            crew: Some(crew),
            asker: Arc::new(Asker {
                questions: interactive.then_some(questions_sender),
            }),
            one_shot: !interactive,
            exit_warned: false,
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
            finished: None,
        };
        (harness, inputs, input_sender)
    }

    fn summary(harness: &Harness) -> String {
        match &harness.conversation {
            Conversation::Answered(result) => result.summary.clone(),
            _ => panic!("the conversation wasn't answered"),
        }
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
        let server = ModelServer::start(Arc::new(move |_, body: String| {
            let seen = seen.clone();
            let ready = ready.clone();
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
            main,
            worker,
            providers: setup,
            max_workers: NO_LIMIT,
            handover_at: 200_000,
            show_thinking: false,
            mcp_tools: &[],
            mcp_servers: &[],
            skill_folders: &[],
            index_tools: &[],
            indexes: &[],
            text_replies: false,
            extra_rules: Default::default(),
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
        let server = ModelServer::start(Arc::new(move |_, _| {
            Box::pin(async move { complete("w", "There are 3 files.") })
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

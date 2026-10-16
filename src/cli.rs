//! The command line: its flags (`Args`), and `run`, which opens the session
//! they ask for and runs it at the full screen, at the line prompt or with -p.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use tokio::sync::mpsc;

use crate::agents::{
    AgentSettings, Effort, ExtraRules, MAIN, ModelSettings, NO_LIMIT, resolve_model,
};
use crate::credentials::{Need, VOYAGE_KEY};
use crate::crew::Crew;
use crate::harness::{Conversation, Harness, Managers, Prompt, TurnEnd};
use crate::inputs::{Asker, Inputs};
use crate::lock;
use crate::providers::{Failure, OpenAiAuth};
use crate::remote::Level;
use crate::session::{Session, Settings};
use crate::shell::{Shell, WindowsShell};
use crate::status::Gauges;
use crate::ui::{Input, Output, Stats};
use crate::{
    agents, credentials, crew, handover, index, keys, mcp, outside, project, providers, remote,
    session, shell, skills, term, tui, ui,
};

#[derive(Parser, Debug)]
#[command(
    name = "arko",
    version,
    about = "A general-purpose agent for the terminal: a shell, background jobs, file tools and workers that run in parallel.",
    after_help = "Credentials: ANTHROPIC_API_KEY for Claude models. For OpenAI models, a ChatGPT login or OPENAI_API_KEY, one per session (--openai-auth). The ChatGPT login is a Codex auth.json (--codex-auth, else ARKO_CODEX_AUTH, else Codex's own from `codex login`), or OPENAI_OAUTH_ACCESS_TOKEN with OPENAI_OAUTH_ACCOUNT_ID. GEMINI_API_KEY (or GOOGLE_API_KEY) for Gemini models, and OPENROUTER_API_KEY for openrouter: models. cc: models run in your installed Claude Code (`claude`, signed in with `claude auth login`) on your Claude plan, never on ANTHROPIC_API_KEY. When the model's credential is missing, Arko asks for it on a setup screen and saves it in Arko's .env. --setup opens that screen. While Arko runs, /keys (also in /settings) adds, replaces and removes API keys, and a key added is used at once.\n\nSettings: the API keys above, ARKO_CODEX_AUTH, ARKO_OPENAI_AUTH (chatgpt or api-key), ARKO_CACHE_1H (1 turns on --cache-1h), ARKO_SHELL (cmd or powershell, on Windows), ARKO_SYMBOLS, ARKO_THEME, ARKO_REMOTE_HOST and VOYAGE_API_KEY can be in the environment or in a .env file. Arko reads the nearest .env in the current directory or a parent, then the one in Arko's folder (ARKO_HOME, else arko in your data directory, such as ~/.local/share/arko).\n\nThe screen: in a terminal, Arko shows the session full screen (--repl keeps the line prompt). Enter sends, and Ctrl+J (or Shift+Enter where the terminal reports it) starts a new line. While the agent works, what you send adds to its task. Ctrl+O switches the view between full, quiet (each worker as one line) and messages. Ctrl+T, or ↓ from the input, reaches the agent list (the main agent and the workers running), where Enter shows a worker's own work and Esc goes back. Ctrl+C stops the turn, and the workers go on. While a shell command of the agent shown runs, Ctrl+B moves it to the background. Drag over the transcript to select text, and it is copied when you let go. A double click copies a word, and a triple click a line. A click on a command's \"more lines\" shows its whole output, and a click on the line at its end folds it again. /help lists the keys, and /settings holds the view, the theme and its previews, the mouse, whether quitting with background jobs running asks first, outside access, the handover size, when commands move to the background, text replies, the extra rules, Claude Code compaction (in a session on cc: models), the project instructions, the web page and the API keys.\n\nThemes: ARKO_THEME picks a theme by name, such as \"macOS Classic Light\" or \"Windows Dark\". The default is macOS Classic Dark, and `terminal` keeps the terminal's own colours. Your own themes go in the themes folder in Arko's folder, in GPUI Kit's theme format. The theme list in /settings shows a theme when you rest on it, and Previews off there shows none until you choose one.\n\nOutside access: the agents change files only inside the working directory. /outside on, or --allow-outside-workspace (also when resuming), lets commands and the file tools change paths outside it too, and /outside off stops that. The agents are told when it changes, and workers follow it. Protected paths, such as the home directory itself, stay refused.\n\nBackground commands: a shell command still running after 10 minutes moves to the background, where it goes on as a job and the agent goes on with its work. The agent hears when it finishes, and a job that finishes while the agent waits for you starts a turn, as a worker's report does. Ctrl+B moves a command to the background at once. /background-after DURATION|off (also in /settings), or --background-after for a new or resumed session, sets the time. With -p commands stay in the foreground unless the flag says otherwise. Background jobs stop when Arko exits, so quitting while one runs asks first (/settings turns that off), and asks again about a job that started since. At the line prompt, /exit (or Ctrl+D) warns and a second one quits.\n\nModels: --model (-m) and --effort set the main agent's model, and --worker-model and --worker-effort the workers'. Before the conversation starts, /model MODEL [EFFORT] changes the main agent's model. After that the main agent keeps its model for the session, and /model worker MODEL [EFFORT] sets the model of workers started from now on. /model worker main makes them follow the main agent again. A worker keeps the model it started on, also when it is resumed or continued. /model and /model worker without a model open a picker of the models you can use.\n\nReplies: in a session you take part in, the main agent ends each turn with a plain reply. With -p it ends each turn by calling complete_task instead, so a reply that wasn't meant to be its last doesn't end the run. --text-replies and --no-text-replies choose for a new session, and /text-replies (also in /settings) changes it before the conversation starts. A resumed session keeps its own.\n\nExtra rules: agents on small models (Haiku, luna) get a few extra rules in their instructions. Each agent goes by its own model, so a worker on Haiku gets them under a main agent on Opus, which doesn't. --extra-rules gives them to every agent and --no-extra-rules to none, for a new session, and /extra-rules (also in /settings) changes it before the conversation starts. A resumed session keeps its own.\n\nProject instructions: the agents get AGENTS.md from the working directory, or CLAUDE.md when there is no AGENTS.md. The main agent gets it once per conversation, in the first message. Each worker gets the same text once, in its first message, also when it works in a worktree of its own. Both get it again after a handover. --project-instructions both gives both files, each under its own name, and --project-instructions off neither. --no-worker-project-instructions keeps them from the workers. The flags work for a new session or a resumed one, and /project-instructions (also in /settings) changes them as you go. A change reaches the conversations and the workers that start after it.\n\nConversations: /new starts a new conversation in the session, and /resume goes back to an earlier one. The screen opens a list to choose from. At the line prompt and on the web page, /resume lists them and /resume N goes back to number N. A resumed session (-c, --resume) goes on with the conversation it was in.\n\nHandovers: when the main agent's input reaches the handover size (500k tokens on Claude models, 250k on the ChatGPT login, 200k on others), it writes a report and goes on from it in a fresh conversation. On cc: models, Claude Code compacts the conversation at that size instead, and the agent goes on in the same conversation from Claude Code's summary. Workers do the same. --no-cc-compact has the agents on cc: models hand over as on the other providers, and --cc-compact has them compact, for a new session. /cc-compact (also in /settings) changes it before the conversation starts, and a resumed session keeps its own. /handover-at SIZE changes the size for the session, larger or smaller, for the main agent and the workers (/handover-at main SIZE or /handover-at workers SIZE changes one of them), each up to 95% of what its model accepts. The running conversations, the workers' too, go by the new size from their next step. /handover has the main agent hand over (or compact) now.\n\nCost: /cost shows what the session billed. For work on a Claude or ChatGPT plan, it also shows the cost at API prices and the plan's usage.\n\nMCP servers: Arko starts those set up in --mcp-config files, in the project's .mcp.json (it asks first, and remembers the answer until the file's servers change) and in mcp.json in Arko's folder. They use Claude Code's format ({\"mcpServers\": {\"name\": {\"command\": ..., \"args\": [...], \"env\": {...}}}}) and talk over standard input and output. Skills: folders with a SKILL.md, in the project's .agents/skills, .claude/skills or .skills, in skills in Arko's folder, or in ~/.agents/skills or ~/.claude/skills.\n\nSearch indexes: `arko index create [PATH]` indexes a codebase or its docs, in any directory (the agents can create indexes too). Arko keeps the working directory's indexes current while it runs, and other indexes once the agent searches them. The agents' searches return the top results' whole functions and sections. `arko index --help` lists the commands. Without VOYAGE_API_KEY, search is by keywords only.\n\nWorkers: the agent starts them to work in parallel while it goes on, each in the working directory or in a git worktree of its own (made in the session's folder, on a branch arko/<session>/<worker>). Their reports reach it as they finish. /workers lists them, /watch shows one's own work, and /stop-workers stops them.\n\nRemote: --remote (or /remote during a session) serves a web page for following the session and taking part in it from a browser, such as a phone's. It has the same views as the screen, and its settings change outside access, text replies, the extra rules, Claude Code compaction, the project instructions, the handover sizes and when commands move to the background, and show links for other devices. Arko prints the links best first: the local network's, then Tailscale's, then other addresses, with VPN tunnels last. ARKO_REMOTE_HOST (an interface or an address) puts its link first. The QR code is for the first link, and /remote qr [INTERFACE] draws one again. The link holds the session's key (kept in the session's folder), which the page trades for a cookie. Arko listens on 127.0.0.1:8765 by default, and --remote 0.0.0.0 (or 0.0.0.0:PORT) lets in the devices on your network, or on your tailnet with Tailscale. The page is served without TLS. On a network you don't trust, reach it through an SSH tunnel (ssh -L 8765:127.0.0.1:8765 HOST, then open the link on your side) or Tailscale. /remote off stops it and ends its sessions, and /remote new-key makes a new key.\n\nSymbols: the classic Windows console (cmd.exe or PowerShell outside Windows Terminal) shows characters its fonts lack as question marks, so there Arko prints stand-ins those fonts have. ARKO_SYMBOLS sets the symbols: full, basic (those stand-ins) or ascii.\n\nCommands: /new, /resume, /model, /keys, /cost, /workers, /watch, /stop-workers, /view, /copy, /settings, /mcp, /skills, /session, /remote, /outside, /background-after, /text-replies, /cc-compact, /extra-rules, /project-instructions, /handover, /handover-at, /help, /exit."
)]
pub(crate) struct Args {
    /// Send this message, print the reply and exit. Progress goes to
    /// standard error, the reply to standard output. When the agent starts
    /// workers, Arko waits for their reports, which go to the agent, and
    /// prints its reply to them.
    #[arg(short = 'p', long)]
    pub(crate) prompt: Option<String>,

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
    pub(crate) model: Option<String>,

    /// Reasoning effort. Gemini thinks at low, medium or high (higher
    /// efforts are high).
    #[arg(short, long, value_enum, default_value_t = Effort::Xhigh)]
    pub(crate) effort: Effort,

    /// Fast mode: Anthropic's on Claude Opus 5.5, priced at 2x (Claude
    /// Code's own fast mode for cc: models), or OpenAI's, priced at 2x with
    /// an API key and using more of the plan with a ChatGPT login. Gemini
    /// and OpenRouter have none.
    #[arg(long)]
    pub(crate) fast: bool,

    /// The workers' model. Default: the main model, with its effort and
    /// fast mode.
    #[arg(long)]
    pub(crate) worker_model: Option<String>,

    /// The workers' reasoning effort. Default: --effort.
    #[arg(long, value_enum)]
    pub(crate) worker_effort: Option<Effort>,

    /// Fast mode for workers on another model than the main one.
    #[arg(long)]
    pub(crate) worker_fast: bool,

    /// Anthropic's 1-hour prompt cache for the Claude models (by API key
    /// or on OpenRouter), instead of the 5-minute one: the cache outlasts
    /// pauses of up to an hour, and a cache write costs 2x the input price
    /// instead of 1.25x. Claude Code chooses its own (an hour on a Claude
    /// plan). Default: ARKO_CACHE_1H (1 or 0, in the environment or a .env
    /// file).
    #[arg(long = "cache-1h", conflicts_with = "no_cache_1h")]
    pub(crate) cache_1h: bool,

    /// The 5-minute prompt cache, whatever ARKO_CACHE_1H says.
    #[arg(long = "no-cache-1h")]
    pub(crate) no_cache_1h: bool,

    /// The main agent ends each turn with a plain reply. This is the
    /// default when you take part in the session (the full screen, the line
    /// prompt or --remote).
    #[arg(long = "text-replies", conflicts_with = "no_text_replies")]
    pub(crate) text_replies: bool,

    /// The main agent ends each turn by calling complete_task, with its
    /// reply in the call. This is the default with -p, where no one is
    /// there to say "go on" after a reply that wasn't meant to be its last.
    #[arg(long = "no-text-replies")]
    pub(crate) no_text_replies: bool,

    /// Agents on cc: models have Claude Code compact their conversation at
    /// the handover size (its /compact), and go on in the same
    /// conversation. This is the default.
    #[arg(long = "cc-compact", conflicts_with = "no_cc_compact")]
    pub(crate) cc_compact: bool,

    /// Agents on cc: models hand over at the handover size, as on the other
    /// providers: they write a report and go on from it in a fresh
    /// conversation.
    #[arg(long = "no-cc-compact")]
    pub(crate) no_cc_compact: bool,

    /// Every agent gets the extra rules for small models in its
    /// instructions, whatever its model. By default only agents on small
    /// models (Haiku, luna) get them.
    #[arg(long = "extra-rules", conflicts_with = "no_extra_rules")]
    pub(crate) extra_rules: bool,

    /// No agent gets the extra rules for small models.
    #[arg(long = "no-extra-rules")]
    pub(crate) no_extra_rules: bool,

    /// Which of the project's instruction files the agents get. The main
    /// agent gets them once per conversation, in its first message, and
    /// each worker once, in its first message. Default: one. When
    /// resuming, it changes the session's choice, as /project-instructions
    /// does.
    #[arg(long, value_enum, value_name = "FILES")]
    pub(crate) project_instructions: Option<project::Files>,

    /// Workers get the project's instructions too, once each, in their
    /// first message. This is the default. When resuming, it gives them to
    /// the workers started from then on.
    #[arg(
        long = "worker-project-instructions",
        conflicts_with = "no_worker_project_instructions"
    )]
    pub(crate) worker_project_instructions: bool,

    /// Workers don't get the project's instructions. Only the main agent
    /// does.
    #[arg(long = "no-worker-project-instructions")]
    pub(crate) no_worker_project_instructions: bool,

    /// How many workers may run at once. Default: no limit. 0 turns them
    /// off.
    #[arg(long)]
    pub(crate) workers: Option<u32>,

    /// Input size, in tokens, at which the agents (the main agent and the
    /// workers) write a handover report and continue in a fresh
    /// conversation. On cc: models, Claude Code compacts the conversation at
    /// that size instead, and the agent goes on in the same conversation
    /// (--no-cc-compact has them hand over too). It takes what /handover-at
    /// takes: 950k, 950K, 950000, 950,000 or 950_000, at least 20k, and each
    /// agent goes by at most 95% of what its model accepts. Default: 500k
    /// on Claude models, 250k for OpenAI models on the ChatGPT login (whose
    /// backend accepts 272k), and 200k on others, at most 80% of what the
    /// model accepts (100k when OpenRouter's catalog can't say). When
    /// resuming, it sets both sizes as /handover-at does, larger or smaller.
    #[arg(long, value_name = "SIZE", value_parser = handover::parse_tokens)]
    pub(crate) handover_at: Option<usize>,

    /// Let commands and the file tools delete, move and overwrite paths
    /// outside the working directory too (protected paths, such as the
    /// home directory itself, stay refused). It also turns outside access
    /// on in a resumed session, and /outside turns it on and off as you go.
    #[arg(long)]
    pub(crate) allow_outside_workspace: bool,

    /// Move a shell command still running after this long to the
    /// background, where it goes on as a job while the agent goes on with
    /// its work: 90s, 10m, 1h (at most 60m), or off. Default: 10m in a
    /// session you take part in, off with -p. For a new or resumed session,
    /// which keeps it. With -p it is off unless this flag sets it.
    #[arg(long, value_name = "DURATION|off", value_parser = shell::parse_background_after)]
    pub(crate) background_after: Option<u64>,

    /// Show the main agent's reasoning as it works.
    #[arg(long)]
    pub(crate) show_thinking: bool,

    /// The working directory. Default: the current directory.
    #[arg(short = 'C', long)]
    pub(crate) workspace: Option<PathBuf>,

    /// Continue the session worked in last in the working directory
    /// (sessions closed before their first message don't count).
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    pub(crate) continue_latest: bool,

    /// Resume a session by ID (see --sessions).
    #[arg(short, long)]
    pub(crate) resume: Option<String>,

    /// List the saved sessions and exit.
    #[arg(long)]
    pub(crate) sessions: bool,

    /// A Codex auth.json with the ChatGPT login. Default: ARKO_CODEX_AUTH,
    /// else $CODEX_HOME/auth.json, else ~/.codex/auth.json.
    #[arg(long)]
    pub(crate) codex_auth: Option<PathBuf>,

    /// How a new session reaches OpenAI models: chatgpt (the ChatGPT login,
    /// on your plan) or api-key (OPENAI_API_KEY, billed per token). The
    /// session keeps it. Default: ARKO_OPENAI_AUTH, else the ChatGPT login
    /// when there is one, else the API key.
    #[arg(long, value_enum)]
    pub(crate) openai_auth: Option<OpenAiAuth>,

    /// The shell commands run in on Windows: cmd (cmd.exe) or powershell
    /// (PowerShell 7 where installed, else Windows PowerShell). Default:
    /// ARKO_SHELL, else cmd. Elsewhere commands run in bash (else sh).
    #[arg(long, value_enum)]
    pub(crate) shell: Option<WindowsShell>,

    /// Open the setup screen first: paste an API key (Anthropic, OpenAI,
    /// Gemini, OpenRouter or Voyage), give the path to a Codex auth.json, or
    /// check Claude Code. What you enter is saved in Arko's folder.
    #[arg(long)]
    pub(crate) setup: bool,

    /// Start the MCP servers in this file too (repeatable), in Claude
    /// Code's format or arko_agents'. A server named here is taken over one of
    /// the same name in the project's .mcp.json or Arko's mcp.json.
    #[arg(long = "mcp-config", value_name = "FILE")]
    pub(crate) mcp_config: Vec<PathBuf>,

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
    pub(crate) remote: Option<String>,

    /// Use the line prompt instead of the full-screen view.
    #[arg(long, conflicts_with = "prompt")]
    pub(crate) repl: bool,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
pub(crate) enum Command {
    /// Search indexes of codebases and docs: create, list, search, update
    /// and watch them.
    Index {
        #[command(subcommand)]
        command: index::IndexCommand,
    },
}

/// The `arko` command: reads the command line, runs what it asks for, and
/// says how it ended.
pub fn cli_main() -> ExitCode {
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

pub(crate) async fn run(args: Args) -> Result<ExitCode, String> {
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
            if args.cc_compact || args.no_cc_compact {
                eprintln!(
                    "arko: a resumed session keeps what its agents in Claude Code do at the handover size: {}",
                    agents::cc_text(!settings.cc_handover)
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
    let agent_settings = AgentSettings::for_session(&settings, &provider_setup);
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
        late: None,
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
        let (quit_popup, quit_asks) = mpsc::unbounded_channel();
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
                        quit_asks,
                    },
                    &output,
                )
            })
            .flatten();
        harness.quit_popup = screen.is_some().then_some(quit_popup);
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
pub(crate) static MODEL_NOTE: Mutex<Option<String>> = Mutex::new(None);

/// The working directory the flags name, made absolute.
pub(crate) fn workspace(args: &Args) -> Result<PathBuf, String> {
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
pub(crate) fn text_replies_for(args: &Args, terminal: bool) -> bool {
    if args.text_replies || args.no_text_replies {
        return args.text_replies;
    }
    args.prompt.is_none() && (args.remote.is_some() || terminal)
}

/// After how many seconds this run's shell commands move to the background
/// (0: never): as the flag says, else never with -p (Arko exits when the
/// agent finishes, jobs or not), else as the session keeps it (`saved`).
pub(crate) fn background_after_for(args: &Args, saved: u64) -> u64 {
    match args.background_after {
        Some(after) => after,
        None if args.prompt.is_some() => 0,
        None => saved,
    }
}

/// Which agents of a new session get the extra rules for small models: as
/// `--extra-rules` (all) or `--no-extra-rules` (none) says, else those on
/// small models.
pub(crate) fn extra_rules_for(args: &Args) -> ExtraRules {
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
pub(crate) fn project_from_args(args: &Args, settings: &mut Settings) -> bool {
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

pub(crate) fn new_settings(args: &Args) -> Result<Settings, String> {
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
    // input limit. A size given is held under what the model leaves room
    // for, as /handover-at holds it.
    let handover_at = |id: &str| {
        let model = resolve_model(id).ok();
        match args.handover_at {
            Some(tokens) => Some(
                model
                    .as_ref()
                    .and_then(|model| agents::max_handover(model, openai_auth))
                    .map_or(tokens, |most| tokens.min(most)),
            ),
            None => model.map(|model| agents::default_handover(&model, openai_auth)),
        }
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
    // A size given that a model holds lower is said.
    if let Some(tokens) = args.handover_at {
        let mut said = Vec::new();
        for model in [&main, &worker] {
            if let Some(at) = model.handover_at.filter(|at| *at < tokens)
                && !said.contains(&model.model)
            {
                eprintln!(
                    "arko: {} hands over at {}, the largest size Arko allows for its model",
                    model.model,
                    handover::shown(at)
                );
                said.push(model.model.clone());
            }
        }
    }
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
        cc_handover: args.no_cc_compact,
        extra_rules: extra_rules_for(args),
        project_instructions: Default::default(),
        worker_project_instructions: true,
        workspace,
        main,
        worker,
        max_workers: args.workers.unwrap_or(NO_LIMIT),
        handover_at: session_handover,
        main_handover_at: None,
        workers_handover_at: None,
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
pub(crate) fn windows_shell(args: &Args) -> Result<WindowsShell, String> {
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
pub(crate) fn default_model(args: &Args) -> Option<providers::DefaultModels> {
    providers::default_models(args.codex_auth.as_deref(), &session::data_root().ok()?)
}

/// Logs go to the session's `arko.log`, not the terminal. `RUST_LOG` sets
/// the level (default: info).
pub(crate) fn start_log(session: &Session) {
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

pub(crate) fn list_sessions(root: &Path) {
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

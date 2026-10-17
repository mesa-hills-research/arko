//! The command line: its flags (`Args`), and `run`, which opens the session
//! they ask for and runs it at the full screen, at the line prompt or with -p.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use crate::agents::{Effort, ExtraRules};
use crate::credentials::VOYAGE_KEY;
use crate::engine::{Engine, FrontEnd, RunEnd};
use crate::options::{Options, SessionChoice};
use crate::providers::OpenAiAuth;
use crate::shell::WindowsShell;
use crate::terminal::TerminalPrompter;
use crate::ui::Input;
use crate::{credentials, handover, index, project, remote, session, shell, term, tui, ui};

#[derive(Parser, Debug)]
#[command(
    name = "arko",
    version,
    about = "A general-purpose agent for the terminal, with a shell, background jobs, file tools and workers that run in parallel.",
    after_help = "Credentials\nClaude models use ANTHROPIC_API_KEY. OpenAI models use a ChatGPT login or OPENAI_API_KEY, one per session (--openai-auth). The ChatGPT login is a Codex auth.json (--codex-auth, else ARKO_CODEX_AUTH, else Codex's own from `codex login`), or OPENAI_OAUTH_ACCESS_TOKEN with OPENAI_OAUTH_ACCOUNT_ID. Gemini models use GEMINI_API_KEY (or GOOGLE_API_KEY), and openrouter: models OPENROUTER_API_KEY. cc: models run in your installed Claude Code (`claude`, signed in with `claude auth login`) on your Claude plan, never on ANTHROPIC_API_KEY. When the model's credential is missing, Arko asks for it on a setup screen and saves it in Arko's .env. --setup opens that screen. While Arko runs, /keys (also in /settings) adds, replaces and removes API keys, and a key added is used at once.\n\nSettings\nThe API keys above, ARKO_CODEX_AUTH, ARKO_OPENAI_AUTH (chatgpt or api-key), ARKO_CACHE_1H (1 turns on --cache-1h), ARKO_SHELL (cmd or powershell, on Windows), ARKO_SYMBOLS, ARKO_THEME, ARKO_REMOTE_HOST and VOYAGE_API_KEY can be in the environment or in a .env file. Arko reads the nearest .env in the session's working directory or a parent, then one beside the arko program, then the one in Arko's folder (ARKO_HOME, else arko in your data directory, such as ~/.local/share/arko).\n\nThe screen\nIn a terminal, Arko shows the session full screen (--repl keeps the line prompt). Enter sends, and Ctrl+J (or Shift+Enter where the terminal reports it) starts a new line. While the agent works, what you send adds to its task. Ctrl+O switches the view between full, quiet (each worker as one line) and messages. Ctrl+T, or ↓ from the input, reaches the agent list (the main agent and the workers running), where Enter shows a worker's own work and Esc goes back. Ctrl+C stops the turn, and the workers go on. While a shell command of the agent shown runs, Ctrl+B moves it to the background. Drag over the transcript to select text, and it is copied when you let go. A double click copies a word, and a triple click a line. A click on a command's \"more lines\" shows its whole output, and a click on the line at its end folds it again. The agents' thinking shows as a Thinking line with its first line, which a click opens the same way (the web page shows it folded too). --show-thinking shows it whole from the start, and --hide-thinking neither asks the models for it nor shows it. /help lists the keys, and /settings holds the view, the theme and its previews, the mouse, whether quitting with background jobs running asks first, outside access, the handover size, when commands move to the background, text replies, the extra rules, Claude Code compaction (in a session on cc: models), the project instructions, the web page and the API keys.\n\nThemes\nARKO_THEME picks a theme by name, such as \"macOS Classic Light\" or \"Windows Dark\". The default is macOS Classic Dark, and `terminal` keeps the terminal's own colours. Your own themes go in the themes folder in Arko's folder, in GPUI Kit's theme format. The theme list in /settings shows a theme when you rest on it, and Previews off there shows none until you choose one.\n\nOutside access\nThe agents change files only inside the working directory. /outside on, or --allow-outside-workspace (also when resuming), lets commands and the file tools change paths outside it too, and /outside off stops that. The agents are told when it changes, and workers follow it. Protected paths, such as the home directory itself, stay refused.\n\nBackground commands\nA shell command still running after 10 minutes moves to the background, where it goes on as a job and the agent goes on with its work. The agent hears when it finishes, and a job that finishes while the agent waits for you starts a turn, as a worker's report does. Ctrl+B moves a command to the background at once. /background-after DURATION|off (also in /settings), or --background-after for a new or resumed session, sets the time. With -p commands stay in the foreground unless the flag says otherwise. Background jobs stop when Arko exits, so quitting while one runs asks first (/settings turns that off), and asks again about a job that started since. At the line prompt, /exit (or Ctrl+D) warns and a second one quits.\n\nModels\n--model (-m) and --effort set the main agent's model, and --worker-model and --worker-effort the workers'. Before the conversation starts, /model MODEL [EFFORT] changes the main agent's model. After that the main agent keeps its model for the session, and /model worker MODEL [EFFORT] sets the model of workers started from now on. /model worker main makes them follow the main agent again. A worker keeps the model it started on, also when it is resumed or continued. /model and /model worker without a model open a picker of the models you can use.\n\nReplies\nIn a session you take part in, the main agent ends each turn with a plain reply. With -p it ends each turn by calling complete_task instead, so a reply that wasn't meant to be its last doesn't end the run. --text-replies and --no-text-replies choose for a new session, and /text-replies (also in /settings) changes it before the conversation starts. A resumed session keeps its own.\n\nExtra rules\nAgents on small models (Haiku, luna) get a few extra rules in their instructions. Each agent goes by its own model, so a worker on Haiku gets them under a main agent on Opus, which doesn't. --extra-rules gives them to every agent and --no-extra-rules to none, for a new session, and /extra-rules (also in /settings) changes it before the conversation starts. A resumed session keeps its own.\n\nProject instructions\nThe agents get AGENTS.md from the working directory, or CLAUDE.md when there is no AGENTS.md. The main agent gets it once per conversation, in the first message. Each worker gets the same text once, in its first message, also when it works in a worktree of its own. Both get it again after a handover. --project-instructions both gives both files, each under its own name, and --project-instructions off neither. --no-worker-project-instructions keeps them from the workers. The flags work for a new session or a resumed one, and /project-instructions (also in /settings) changes them as you go. A change reaches the conversations and the workers that start after it.\n\nConversations\n/new starts a new conversation in the session, and /resume goes back to an earlier one. The screen opens a list to choose from. At the line prompt and on the web page, /resume lists them and /resume N goes back to number N. A resumed session (-c, --resume) goes on with the conversation it was in.\n\nHandovers\nWhen the main agent's input reaches the handover size (500k tokens on Claude models, 250k on the ChatGPT login, 200k on others), it writes a report and goes on from it in a fresh conversation. On cc: models, Claude Code compacts the conversation at that size instead, and the agent goes on in the same conversation from Claude Code's summary. Workers do the same. --no-cc-compact has the agents on cc: models hand over as on the other providers, and --cc-compact has them compact, for a new session. /cc-compact (also in /settings) changes it before the conversation starts, and a resumed session keeps its own. /handover-at SIZE changes the size for the session, larger or smaller, for the main agent and the workers (/handover-at main SIZE or /handover-at workers SIZE changes one of them), each up to 95% of what its model accepts. The running conversations, the workers' too, go by the new size from their next step. /handover has the main agent hand over (or compact) now.\n\nCost\n/cost shows what the session billed. For work on a Claude or ChatGPT plan, it also shows the cost at API prices and the plan's usage.\n\nMCP servers\nArko starts the MCP servers set up in --mcp-config files, in the project's .mcp.json (it asks first, and remembers the answer until the file's servers change) and in mcp.json in Arko's folder. They use Claude Code's format ({\"mcpServers\": {\"name\": {\"command\": ..., \"args\": [...], \"env\": {...}}}}) and talk over standard input and output.\n\nSkills\nSkills are folders with a SKILL.md, in the project's .agents/skills, .claude/skills or .skills, in skills in Arko's folder, or in ~/.agents/skills or ~/.claude/skills.\n\nSearch indexes\n`arko index create [PATH]` indexes a codebase or its docs, in any directory (the agents can create indexes too). Arko keeps the working directory's indexes current while it runs, and other indexes once the agent searches them. The agents' searches return the top results' whole functions and sections. `arko index --help` lists the commands. Without VOYAGE_API_KEY, search is by keywords only.\n\nWorkers\nThe agent starts workers to work in parallel while it goes on, each in the working directory or in a git worktree of its own (made in the session's folder, on a branch arko/<session>/<worker>). Their reports reach it as they finish. /workers lists them, /watch shows one's own work, and /stop-workers stops them (/stop-workers r1 stops one).\n\nRemote\n--remote (or /remote during a session) serves a web page for following the session and taking part in it from a browser, such as a phone's. It has the same views as the screen, and its settings change outside access, text replies, the extra rules, Claude Code compaction, the project instructions, the handover sizes and when commands move to the background, and show links for other devices. Arko prints the links best first, the local network's, then Tailscale's, then other addresses, and VPN tunnels last. ARKO_REMOTE_HOST (an interface or an address) puts its link first. The QR code is for the first link, and /remote qr [INTERFACE] draws one again. The link holds the session's key (kept in the session's folder), which the page trades for a cookie. Arko listens on 127.0.0.1:8765 by default, and --remote 0.0.0.0 (or 0.0.0.0:PORT) lets in the devices on your network, or on your tailnet with Tailscale. The page is served without TLS. On a network you don't trust, reach it through an SSH tunnel (ssh -L 8765:127.0.0.1:8765 HOST, then open the link on your side) or Tailscale. /remote off stops it and ends its sessions, and /remote new-key makes a new key.\n\nSymbols\nThe classic Windows console (cmd.exe or PowerShell outside Windows Terminal) shows characters its fonts lack as question marks, so there Arko prints stand-ins those fonts have. ARKO_SYMBOLS sets the symbols to full, basic (those stand-ins) or ascii.\n\nCommands\n/new, /resume, /model, /keys, /cost, /workers, /watch, /stop-workers, /view, /copy, /settings, /mcp, /skills, /session, /remote, /outside, /background-after, /text-replies, /cc-compact, /extra-rules, /project-instructions, /handover, /handover-at, /help, /exit."
)]
pub(crate) struct Args {
    /// Send this message, print the reply and exit. Progress goes to
    /// standard error, the reply to standard output. When the agent starts
    /// workers, Arko waits for their reports, which go to the agent, and
    /// prints its reply to them.
    #[arg(short = 'p', long)]
    pub(crate) prompt: Option<String>,

    /// The model, as an ID or alias (opus, fable, sonnet, sol, astra, luna,
    /// gemini, flash), openrouter:<slug> for a model on OpenRouter
    /// (openrouter:anthropic/claude-opus-5.5), or cc:<model> for a Claude
    /// model in your installed Claude Code, on your Claude plan (cc:opus,
    /// cc:fable). By default, the model of the last session you started
    /// (with its workers' model, when that differed) while its credential
    /// is there, else one by the credentials there are, cc:opus with Claude
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

    /// Use fast mode. Anthropic's, on Claude Opus 5.5, is priced at 2x
    /// (Claude Code's own fast mode for cc: models). OpenAI's is priced at
    /// 2x with an API key and uses more of the plan with a ChatGPT login.
    /// Gemini and OpenRouter have none.
    #[arg(long)]
    pub(crate) fast: bool,

    /// The workers' model. By default they take the main model, with its
    /// effort and fast mode.
    #[arg(long)]
    pub(crate) worker_model: Option<String>,

    /// The workers' reasoning effort, by default --effort's.
    #[arg(long, value_enum)]
    pub(crate) worker_effort: Option<Effort>,

    /// Fast mode for workers on another model than the main one.
    #[arg(long)]
    pub(crate) worker_fast: bool,

    /// Anthropic's 1-hour prompt cache for the Claude models (by API key
    /// or on OpenRouter), instead of the 5-minute one. The cache outlasts
    /// pauses of up to an hour, and a cache write costs 2x the input price
    /// instead of 1.25x. Claude Code chooses its own (an hour on a Claude
    /// plan). ARKO_CACHE_1H (1 or 0, in the environment or a .env file) sets
    /// the default.
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
    /// providers, writing a report and going on from it in a fresh
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
    /// each worker once, in its first message. The default is one. When
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

    /// How many workers may run at once, with no limit by default. 0 turns
    /// them off.
    #[arg(long)]
    pub(crate) workers: Option<u32>,

    /// Input size, in tokens, at which the agents (the main agent and the
    /// workers) write a handover report and continue in a fresh
    /// conversation. On cc: models, Claude Code compacts the conversation at
    /// that size instead, and the agent goes on in the same conversation
    /// (--no-cc-compact has them hand over too). It takes what /handover-at
    /// takes (950k, 950K, 950000, 950,000, 950_000, 1M or 1.2M), at least
    /// 20k, and each agent goes by at most 95% of what its model accepts.
    /// The default is 500k on Claude models, 250k for OpenAI models on the
    /// ChatGPT login (whose backend accepts 272k), and 200k on others, at most 80%
    /// of what the model accepts (100k when OpenRouter's catalog can't say).
    /// When resuming, it sets both sizes as /handover-at does, larger or
    /// smaller.
    #[arg(long, value_name = "SIZE", value_parser = handover::parse_tokens)]
    pub(crate) handover_at: Option<usize>,

    /// Let commands and the file tools delete, move and overwrite paths
    /// outside the working directory too (protected paths, such as the
    /// home directory itself, stay refused). It also turns outside access
    /// on in a resumed session, and /outside turns it on and off as you go.
    #[arg(long)]
    pub(crate) allow_outside_workspace: bool,

    /// Move a shell command still running after this long (90s, 10m or 1h,
    /// at most 60m, or off) to the background, where it goes on as a job
    /// while the agent goes on with its work. The default is 10m in a
    /// session you take part in, and off with -p. For a new or resumed
    /// session, which keeps it.
    #[arg(long, value_name = "DURATION|off", value_parser = shell::parse_background_after)]
    pub(crate) background_after: Option<u64>,

    /// Show the agents' thinking whole. The line prompt (--repl) prints the
    /// main agent's as it comes, and the full-screen view opens it. By
    /// default the full-screen view and the web page show it folded, and
    /// the line prompt leaves it out. For a new session.
    #[arg(long, conflicts_with = "hide_thinking")]
    pub(crate) show_thinking: bool,

    /// Neither ask the models for the text of their thinking (they still
    /// think) nor show it. For a provider that refuses that request, such
    /// as OpenAI's reasoning summaries for an API organization it hasn't
    /// verified. For a new session.
    #[arg(long)]
    pub(crate) hide_thinking: bool,

    /// The working directory, by default the current directory.
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

    /// A Codex auth.json with the ChatGPT login. By default ARKO_CODEX_AUTH,
    /// else $CODEX_HOME/auth.json, else ~/.codex/auth.json.
    #[arg(long)]
    pub(crate) codex_auth: Option<PathBuf>,

    /// How a new session reaches OpenAI models, chatgpt (the ChatGPT login,
    /// on your plan) or api-key (OPENAI_API_KEY, billed per token). The
    /// session keeps it. By default ARKO_OPENAI_AUTH, else the ChatGPT login
    /// when there is one, else the API key.
    #[arg(long, value_enum)]
    pub(crate) openai_auth: Option<OpenAiAuth>,

    /// The shell commands run in on Windows, cmd (cmd.exe) or powershell
    /// (PowerShell 7 where installed, else Windows PowerShell). By default
    /// ARKO_SHELL, else cmd. Elsewhere commands run in bash (else sh).
    #[arg(long, value_enum)]
    pub(crate) shell: Option<WindowsShell>,

    /// Open the setup screen first, to paste an API key (Anthropic, OpenAI,
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
    /// both, by default 127.0.0.1:8765 (this machine only), and 0.0.0.0 lets
    /// in the other devices on the network. Arko prints the link, which holds
    /// the session's key. The page isn't encrypted, so on a network you don't
    /// trust, reach it through an SSH tunnel or Tailscale.
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
    /// Create, list, search, update and watch search indexes of codebases
    /// and docs.
    Index {
        #[command(subcommand)]
        command: index::IndexCommand,
    },
}

impl Args {
    /// The session these flags ask for.
    pub(crate) fn options(&self) -> Result<Options, String> {
        use std::io::IsTerminal;
        self.options_at(std::io::stdin().is_terminal() && std::io::stdout().is_terminal())
    }

    /// The session these flags ask for, with standard input and output a
    /// terminal or not (`terminal`).
    pub(crate) fn options_at(&self, terminal: bool) -> Result<Options, String> {
        let one_shot = self.prompt.is_some();
        Ok(Options {
            root: None,
            session: match (&self.resume, self.continue_latest) {
                (Some(id), _) => SessionChoice::Resume(id.clone()),
                (None, true) => SessionChoice::ContinueLatest,
                (None, false) => SessionChoice::New,
            },
            workspace: self.workspace.clone(),
            model: self.model.clone(),
            effort: self.effort,
            fast: self.fast,
            worker_model: self.worker_model.clone(),
            worker_effort: self.worker_effort,
            worker_fast: self.worker_fast,
            cache_1h: flag(self.cache_1h, self.no_cache_1h),
            text_replies: flag(self.text_replies, self.no_text_replies),
            cc_compact: flag(self.cc_compact, self.no_cc_compact),
            extra_rules: flag(self.extra_rules, self.no_extra_rules)
                .map(|on| if on { ExtraRules::All } else { ExtraRules::Off }),
            project_instructions: self.project_instructions,
            worker_project_instructions: flag(
                self.worker_project_instructions,
                self.no_worker_project_instructions,
            ),
            workers: self.workers,
            handover_at: self.handover_at,
            workers_handover_at: None,
            allow_outside_workspace: self.allow_outside_workspace,
            background_after: self.background_after,
            show_thinking: self.show_thinking,
            hide_thinking: self.hide_thinking,
            codex_auth: self.codex_auth.clone(),
            openai_auth: self.openai_auth,
            shell: self.shell,
            mcp_config: self.mcp_config.clone(),
            mcp_config_required: true,
            remote: self.remote.as_deref().map(remote::parse_bind).transpose()?,
            one_shot,
            // At the full screen or the line prompt.
            interactive: !one_shot && terminal,
            title: self.prompt.as_deref().map(|prompt| ui::cut(prompt, 80)),
            app: "arko",
            app_name: "Arko in a terminal",
            shows_page_links: false,
            source: remote::Source::Terminal,
            mode: crate::session::Mode::Agent,
            chat_instructions: None,
            #[cfg(any(test, feature = "testing"))]
            client: None,
        })
    }
}

/// What a flag and its `--no-` form say: `None` when neither is given.
fn flag(on: bool, off: bool) -> Option<bool> {
    if on {
        Some(true)
    } else if off {
        Some(false)
    } else {
        None
    }
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
    let options = args.options()?;
    let one_shot = options.one_shot;
    let prompter = Arc::new(TerminalPrompter::new(can_ask));
    let (mut engine, seam) = Engine::open(options, prompter.clone())
        .await
        .map_err(|error| error.to_string())?;
    let output = engine.output();
    // A Ctrl-C since the workflow started loading (when the line editor
    // isn't reading: in raw mode it reads Ctrl-C as a key) is the session's.
    if let Some(mut interrupts) = prompter.take_interrupts() {
        let sender = seam.input.clone();
        tokio::spawn(async move {
            while interrupts.recv().await.is_some() {
                if sender.send(Input::Interrupt).is_err() {
                    break;
                }
            }
        });
    }
    // The full-screen view is the front end with a terminal (not with
    // --repl), and it reads the input itself. Else the line prompt reads it.
    let full_screen = !one_shot && !args.repl && tui::available();
    let mut prompt_shown = false;
    if !one_shot {
        term::save();
        if !full_screen
            && let Some(printer) = ui::spawn_input(
                seam.session.root.join("history.txt"),
                seam.input.clone(),
                Some(seam.keys.clone()),
            )
        {
            output.attach(printer);
            prompt_shown = true;
        }
    }
    let session_id = seam.session.id.clone();
    let code = if let Some(prompt) = args.prompt.clone() {
        engine.front_end(FrontEnd::default());
        match engine.run_prompt(prompt).await {
            RunEnd::Answered | RunEnd::Quit | RunEnd::Reopen => ExitCode::SUCCESS,
            RunEnd::Stopped => ExitCode::from(130),
            RunEnd::Failed => ExitCode::FAILURE,
        }
    } else {
        let screen = full_screen
            .then(|| {
                tui::start(
                    tui::Setup {
                        feed: seam.feed.clone(),
                        meter: seam.meter.clone(),
                        input: seam.input.clone(),
                        root: seam.session.root.clone(),
                        session_dir: seam.session.dir.clone(),
                        workspace: seam.session.workspace.clone(),
                        handover_at: seam.handover_at,
                        show_thinking: seam.show_thinking,
                        banner: seam.banner,
                        keys: seam.keys.clone(),
                        quit_asks: seam.quit_asks,
                    },
                    &output,
                )
            })
            .flatten();
        prompt_shown |= full_screen;
        engine.front_end(FrontEnd {
            boxed_banner: screen.is_some(),
            asks_to_quit: screen.is_some(),
            asks_about_workers: false,
        });
        engine.run().await;
        if let Some(screen) = screen {
            screen.close().await;
        }
        ExitCode::SUCCESS
    };
    ui::stop_reading();
    engine.shutdown().await;
    // The prompt is still drawn, and its thread still reading: what is
    // printed from here goes straight to the terminal, below the prompt
    // line cleared and with the terminal's modes restored.
    output.detach();
    if prompt_shown {
        tokio::time::sleep(Duration::from_millis(30)).await;
        output.clear_line();
    }
    term::restore();
    output.line(&output.dim(&format!(
        "Session {session_id} saved. arko --resume {session_id} continues it."
    )));
    Ok(code)
}

/// Logs go to the `arko.log` of the session in `dir`, not the terminal.
/// `RUST_LOG` sets the level (default: info).
pub(crate) fn start_log(dir: &Path) {
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(session::LOG_FILE))
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

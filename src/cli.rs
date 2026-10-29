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
    after_help = "Credentials\nClaude models use ANTHROPIC_API_KEY. OpenAI models use a ChatGPT login or OPENAI_API_KEY, one per session (--openai-auth). The ChatGPT login is a Codex auth.json (--codex-auth, else ARKO_CODEX_AUTH, else Codex's own from `codex login`), or OPENAI_OAUTH_ACCESS_TOKEN with OPENAI_OAUTH_ACCOUNT_ID. ARKO_CODEX_AUTH can name several files, separated as PATH is (`:`, and `;` on Windows). Arko uses those logins in order, and the next takes over when one reaches its usage limit. /chatgpt lists, adds, removes and orders them while Arko runs, and a login you sign in to another account with `codex login` is used from the next request. Gemini models use GEMINI_API_KEY (or GOOGLE_API_KEY), and openrouter: models OPENROUTER_API_KEY. cc: models run in your installed Claude Code (`claude`, signed in with `claude auth login`) on your Claude plan, never on ANTHROPIC_API_KEY. When the model's credential is missing, Arko asks for it on a setup screen and saves it in Arko's .env. --setup opens that screen. While Arko runs, /keys (also in /settings) adds, replaces and removes API keys, and a key added is used at once.\n\nSettings\nThe API keys above, ARKO_CODEX_AUTH, ARKO_OPENAI_AUTH (chatgpt or api-key), ARKO_CACHE_1H (1 turns on --long-cache), ARKO_BUILTIN_SKILLS (0 turns off the built-in skills), ARKO_BUILTIN_SKILLS_OFF (the built-in skills to leave out, separated by commas), ARKO_SHELL (cmd or powershell, on Windows), ARKO_WORKER_BILLING (plans, ask or api), ARKO_API_BUDGET (dollars a day, or off), ARKO_SYMBOLS, ARKO_THEME, ARKO_REMOTE_HOST and VOYAGE_API_KEY can be in the environment or in a .env file. Arko reads the nearest .env in the session's working directory or a parent, then one beside the arko program, then the one in Arko's folder (ARKO_HOME, else arko in your data directory, such as ~/.local/share/arko).\n\nThe screen\nIn a terminal, Arko shows the session full screen (--repl keeps the line prompt). Enter sends, and Ctrl+J (or Shift+Enter where the terminal reports it) starts a new line. While the agent works, what you send adds to its task. Ctrl+O switches the view between full, quiet (each worker as one line) and messages. Ctrl+T, or Tab or ↓ from the input, reaches the agent list (the main agent, the workers running and those that ended in the last five minutes). There Tab and Shift+Tab move through it and on to the input, Enter shows a worker's own work, and Esc goes back. Ctrl+C stops the turn, and the workers go on. While the agent shown runs a long call (a shell command, an MCP tool's call, index_create), Ctrl+B moves it to the background. While the agent waits for you, the status line counts down the prompt cache's time when the provider gives one (\"cache 3:12\"), warns in its last minute and then says \"cache expired\". A resumed session counts from its last request. The agent waits for answers to its questions unless it has work no answer could change. When you aren't typing they open an answer panel above the message box, else a chip says the agent asks, and Alt+Q opens it. The panel shows one question at a time, then a summary. Number keys pick a choice or \"You decide\", ↑↓ and Space select choices, and typing gives your own answer. Enter goes on, then sends from the summary and leaves unanswered questions open. Esc answers in a message instead (Backspace in the empty box takes the questions off it), and Ctrl+N dismisses them. Requests for your approval, such as an MCP server an agent adds, a skill or worker profile it saves for every project, or a worker billed per token, wait in the approval tray above the message box. Alt+A gives it the keys. There ↑↓ move, Space selects, Enter approves those selected and rejects the rest, R rejects them all, V opens a row's details and Esc goes back. What the message box holds goes to the agent with your answer. At the line prompt, /approve and /reject answer them. Drag over the transcript, or over the answer panel, to select text, and it is copied when you let go. A double click copies a word, and a triple click a line. A click on a command's \"more lines\" shows its whole output, and a click on the line at its end folds it again. The agents' thinking shows as a Thinking line with its first line, which a click opens the same way (the web page shows it folded too). --show-thinking shows it whole from the start, and --hide-thinking neither asks the models for it nor shows it. /help lists the keys, and /settings holds the view, the theme and its previews, the mouse, whether quitting with background jobs running asks first, outside access, the handover size, when commands move to the background, monitor wakes (how long a monitor's warning wakes the agent), who chooses the workers' models, worker billing, the daily API budget, worker depth, worker retry cap, message copies, whether agents ask before adding MCP servers, whether they ask before saving project skills, whether agents' project files stay out of git, whether the repository root's profiles are read, text replies, the extra rules, Claude Code compaction (in a session on cc: models), the project instructions, the built-in skills, the web page, the API keys and the ChatGPT logins.\n\nThemes\nARKO_THEME picks a theme by name, such as \"macOS Classic Light\" or \"Windows Dark\". The default is macOS Classic Dark, and `terminal` keeps the terminal's own colours. Your own themes go in the themes folder in Arko's folder, in GPUI Kit's theme format. The theme list in /settings shows a theme when you rest on it, and Previews off there shows none until you choose one.\n\nOutside access\nThe agents change files only inside the working directory. /outside on, or --allow-outside-workspace (also when resuming), lets commands and the file tools change paths outside it too, and /outside off stops that. The agents are told when it changes, and workers follow it. Protected paths, such as the home directory itself, stay refused.\n\nBackground jobs\nA shell command or another long call (an MCP tool's call, index_create) still running after 10 minutes moves to the background, where it goes on as a job and the agent goes on with its work. The agent hears when it finishes, with a moved call's result, and a job that finishes while the agent waits for you starts a turn, as a worker's report does. Ctrl+B moves such a call to the background at once, and so does Move to background on the web page. /jobs lists the background jobs, and /jobs stop N or /jobs stop all stops them (the agent reads that you did with your next message). /background-after DURATION|off (also in /settings), or --background-after for a new or resumed session, sets the time. With -p, calls stay in the foreground, and only --background-after moves commands. Background jobs stop when Arko exits, so quitting while one runs asks first (/settings turns that off), and asks again about a job that started since. A session continued after that tells the agent which of its jobs stopped. At the line prompt, /exit (or Ctrl+D) warns and a second one quits.\n\nMonitors\nThe agents can watch a command with a monitor, which alerts them of the output lines that match a pattern (a dev server's errors), or of a check run again and again whose result changes (a health URL starting to answer). Alerts reach the agent as notes while it works, at most once a minute, and an alert while it waits for you starts a turn. A monitor lasts up to 30 minutes, and before it ends the agent is asked whether to keep it. That warning wakes the agent while the prompt cache from your last message lasts (auto, which is 90% of it, and an hour where the provider gives no cache time). /monitor-wakes DURATION|auto|always|off (also in /settings), or --monitor-wakes for a new or resumed session, changes how long. /jobs lists the monitors with the other background jobs, and stops them. A monitor stops when Arko exits, and a session continued after that tells the agent which stopped.\n\nModels\n--model (-m) and --effort set the main agent's model, and --worker-model and --worker-effort the workers'. Before the conversation starts, /model MODEL [EFFORT] changes the main agent's model. After that the main agent keeps its model for the session, and /model worker MODEL [EFFORT] sets the model of workers started from now on. /model worker main makes them follow the main agent again. A worker keeps the model it started on, also when it is resumed or continued. The main agent can choose another model and effort for a worker (see Worker profiles), so --worker-model and /model worker set the model of the workers it gives no choice. /model and /model worker without a model open a picker of the models you can use. The picker's second step starts on the effort the model ran at last.\n\nOpenRouter\nopenrouter:<slug> names any model OpenRouter serves. The picker's OpenRouter group lists the models you picked, with their context size and prices, and ends with a row for all the models OpenRouter lists that take tool calls, which you filter by typing. /model openrouter: and a few letters completes slugs from that list. /openrouter (a menu in the full-screen view, words elsewhere) sets zero data retention, no data collection, a region (any, us, eu or us-or-eu), how providers are ordered (price, throughput or latency), how precise they must be, and the providers to use or to leave out. They are saved in openrouter.json in Arko's folder, which also takes OpenRouter's other provider fields written by hand. Open sessions use a change from their next request. A region needs OpenRouter's Business or Enterprise plan, fewer models are offered in one, and the privacy settings of your OpenRouter account apply too. The list of models is read from OpenRouter without a key and kept for ten minutes.\n\nReplies\nIn a session you take part in, the main agent ends each turn with a plain reply. With -p it ends each turn by calling complete_task instead, so a reply that wasn't meant to be its last doesn't end the run. --text-replies and --no-text-replies choose for a new session, and /text-replies (also in /settings) changes it before the conversation starts. A resumed session keeps its own.\n\nExtra rules\nAgents on small models (Haiku, luna) get a few extra rules in their instructions. Each agent goes by its own model, so a worker on Haiku gets them under a main agent on Opus, which doesn't. --extra-rules gives them to every agent and --no-extra-rules to none, for a new session, and /extra-rules (also in /settings) changes it before the conversation starts. A resumed session keeps its own.\n\nProject instructions\nThe agents get AGENTS.md from the working directory, or CLAUDE.md when there is none. The main agent gets it once per conversation, in the first message. Each worker gets the same text once, in its first message, also when it works in a worktree of its own. Both get it again after a handover. --project-instructions both gives both files, each under its own name, and --project-instructions off neither. --no-worker-project-instructions keeps them from the workers. The flags work for a new session or a resumed one, and /project-instructions (also in /settings) changes them as you go. A change reaches the conversations and the workers that start after it.\n\nKept notes\nThe main agent can keep notes for itself and its workers (keep_note), such as something you asked it to do later. They come back on their own after a handover or a compaction and when /resume or -c goes back to a conversation, and a worker gets its notes in its first message and after its own handover. /notes lists them and /notes remove N removes one. They hold 8 KB in all.\n\nConversations\n/new starts a new conversation in the session, and /resume goes back to an earlier one. The screen opens a list to choose from. At the line prompt and on the web page, /resume lists them and /resume N goes back to number N. A resumed session (-c, --resume) goes on with the conversation it was in.\n\nHandovers\nWhen the main agent's input reaches the handover size (950k tokens on Claude models, 500k on Claude models through OpenRouter, 250k on the ChatGPT login, 200k on others), it writes a report and goes on from it in a fresh conversation. On cc: models, Claude Code compacts the conversation at that size instead, and the agent goes on in the same conversation from Claude Code's summary. Workers do the same. --no-cc-compact has the agents on cc: models hand over as on the other providers, and --cc-compact has them compact, for a new session. /cc-compact (also in /settings) changes it before the conversation starts, and a resumed session keeps its own. /handover-at SIZE changes the size for the session, larger or smaller, for the main agent and the workers (/handover-at main SIZE or /handover-at workers SIZE changes one of them), each up to 95% of what its model accepts. The running conversations, the workers' too, go by the new size from their next step. /handover has the main agent hand over (or compact) now.\n\nCost\n/cost shows what the session billed. For work on a Claude or ChatGPT plan, it also shows the cost at API prices and the plan's usage, with a ChatGPT account's credits.\n\nMCP servers\nArko starts the MCP servers set up in --mcp-config files, in the project's .arko/mcp.json and .mcp.json (it asks first, and remembers the answer until the file's servers change) and in mcp.json in Arko's folder. They use Claude Code's format ({\"mcpServers\": {\"name\": {\"command\": ..., \"args\": [...], \"env\": {...}}}}) and talk over standard input and output. The agents reach them through one tool, mcp, so starting, stopping or adding a server keeps the prompt cache. The main agent can add a server for the project (in .arko/mcp.json) or for every project, and asks you first unless you turned that off (see Approvals). An entry with \"workers\": false keeps a server to the main agent. Each open agent session runs its own copy of each server, unless its entry says \"shared\": true, and a chat runs none. /mcp lists the servers, /mcp NAME shows one, and /mcp remove NAME removes one an agent added.\n\nSkills\nSkills are folders with a SKILL.md, in the project's .agents/skills, .claude/skills, .skills or .arko/skills, in skills in Arko's folder, or in ~/.agents/skills or ~/.claude/skills. A skill's models field lists the models whose agents see it (an ID, a short name such as haiku, or a pattern such as gpt-6*), and workers: false keeps it to the main agent. The main agent saves skills with add_skill, in the project's .arko/skills, or in Arko's folder for every project once you approve. A downloaded skill always asks, and so does every save with review-skills on (see Approvals). /skills lists the skills, /skills NAME shows one, and /skills remove NAME removes one an agent saved. Arko has three built-in skills of its own, user-facing-prose (for any text a user reads), repo-docs-style (for project docs) and ponytail (for coding tasks and running workers). The agents see their descriptions and load them when a task calls for it, and a skill of the same name in those folders replaces one. --no-builtin-skills leaves them out of a new session, and --no-builtin-skill NAME leaves one out. When resuming, they change the conversations that start after. ARKO_BUILTIN_SKILLS=0 and ARKO_BUILTIN_SKILLS_OFF=NAME do the same for every new session. /builtin-skills on|off and /builtin-skills NAME on|off (also in /settings) change the choice as you go. Before the conversation starts they change the agents. After it the conversation going on keeps its skills, and new workers and new conversations get the change. A name that isn't a built-in skill is skipped with a warning.\n\nApprovals\nAgents ask you before they add an MCP server, for a project or for every project, and before they save a skill for every project or from a download. /extensions (also in /settings and on the web page) says three switches and changes one. ask-mcp is on by default. Off lets agents add servers without asking, and a server runs a program with your permissions, outside the command guard, so /settings and the web page ask before they turn it off. review-skills is off by default, and on has agents ask before they save a skill in the project too. keep-out-of-git is on by default and keeps what agents save in the project's .arko folder out of git, with a .gitignore Arko writes there. The switches are kept in Arko's folder, and a running session follows a change at the agent's next add.\n\nSearch indexes\n`arko index create [PATH]` indexes a codebase or its docs, in any directory (the agents can create indexes too). Arko keeps the working directory's indexes current while it runs, and other indexes once the agent searches them. The agents' searches return the top results' whole functions and sections. `arko index --help` lists the commands. Without VOYAGE_API_KEY, search is by keywords only.\n\nWorkers\nThe agent starts workers to work in parallel while it goes on, each in the working directory or in a git worktree of its own (made in the session's folder, on a branch arko/<session>/<worker>). Their reports reach it as they finish, and each ends with what the worker used, in tokens and at API prices. A worker it starts with `nested: true`, or with a profile that says `nested: true`, can start workers of its own, and their reports reach that worker. /worker-depth sets how deep this goes. /worker-retry-cap off|on|N (also --worker-retry-cap [N]) stops a worker, and the workers it started, after N failed model requests in a row. It is off by default, on and a flag without a number mean 10, and a change applies at once, also to the workers running. /workers lists the workers, a worker's own under it, /watch shows one's own work, and /stop-workers stops them (/stop-workers r1 stops one and the workers it started). /message r1 TEXT sends a worker a message. /message-copies on (also --message-copies) has the agent that started it get a copy with its next news.\n\nWorker billing\ncc: models run on your Claude plan, and OpenAI models on your ChatGPT plan when the session uses the ChatGPT login. Models on an API key, Gemini and OpenRouter are billed per token. /worker-billing plans|ask|api (also --worker-billing, and ARKO_WORKER_BILLING for new sessions) says whether workers may run on those. With plans they can't, with ask (the default) Arko asks you before each one starts, and with api they start without asking. It asks in the approval tray. Approve starts that worker, and with Allow … for this session turned on in its details, that model's next workers too. Reject refuses it. The main agent is told to prefer your plans' models. /api-budget DOLLARS|on|off (also --api-budget, and ARKO_API_BUDGET for new sessions) sets a daily API budget, off by default and $10 when on. Once what your sessions were billed per token since midnight reaches it, new workers on API models don't start (with ask, Arko still asks), and you're told once. Running workers go on, and the main agent's own calls count toward it.\n\nWorker profiles\nA profile is a named setup for workers, a model, an effort and instructions. It is a Markdown file in profiles in Arko's folder, in .arko/profiles (or the older .agents/profiles) in the project or in the root of its git repository, or in the session's folder, and the nearest one wins. The agent lists the profiles and the models it can use (worker_options), starts a worker with a profile, or with a model and effort of its own choice, and saves profiles for the project unless it picks the session or every project. A profile for the session needs no approval, and neither does one for the project when git ignores its folder. Arko makes .arko with a .gitignore in it, and deleting the file shares what Arko writes there. Any other save asks you first. /profiles lists the profiles, /profiles NAME shows one, /profiles add NAME MODEL [EFFORT] [DESCRIPTION] makes one for every project, and /profiles remove NAME deletes one. /worker-models agent, profiles or session (also --worker-models) says whether the agent chooses, uses profiles only, or leaves every worker on the session's worker model. /worker-depth 1 to 4 (also --worker-depth) sets how many levels of workers the session allows, 2 by default, and 1 keeps workers from starting workers.\n\nRemote\n--remote (or /remote during a session) serves a web page for following the session and taking part in it from a browser, such as a phone's. It has the same views as the screen, and its settings change outside access, text replies, the extra rules, Claude Code compaction, the project instructions, the built-in skills, the handover sizes, when commands move to the background and monitor wakes, who chooses the workers' models, worker billing, the daily API budget, worker depth, worker retry cap, message copies, whether agents ask before adding MCP servers, whether they ask before saving project skills and whether agents' project files stay out of git, and show links for other devices. Typing / in its input lists the commands it can run, as the screen's input does. Arko prints the links best first, the local network's, then Tailscale's, then other addresses, and VPN tunnels last. ARKO_REMOTE_HOST (an interface or an address) puts its link first. The QR code is for the first link, and /remote qr [INTERFACE] draws one again. The link holds the session's key (kept in the session's folder), which the page trades for a cookie. The cookie lasts 12 hours after the page last opened or reconnected, for up to 7 days, also when Arko serves the page again, so a browser that forgets the link's key keeps the page without a new scan. Arko listens on 127.0.0.1:8765 by default, and --remote 0.0.0.0 (or 0.0.0.0:PORT) lets in the devices on your network, or on your tailnet with Tailscale. The page is served without TLS. On a network you don't trust, reach it through an SSH tunnel (ssh -L 8765:127.0.0.1:8765 HOST, then open the link on your side) or Tailscale. /remote off stops it and ends its sessions, and /remote new-key makes a new key.\n\nSymbols\nThe classic Windows console (cmd.exe or PowerShell outside Windows Terminal) shows characters its fonts lack as question marks, so there Arko prints stand-ins those fonts have. ARKO_SYMBOLS sets the symbols to full, basic (those stand-ins) or ascii.\n\nCommands\n/new, /resume, /model, /keys, /openrouter, /chatgpt, /cost, /workers, /watch, /stop-workers, /view, /copy, /settings, /mcp, /skills, /session, /remote, /outside, /jobs, /notes, /approve, /reject, /background-after, /monitor-wakes, /profiles, /worker-models, /worker-billing, /api-budget, /worker-depth, /worker-retry-cap, /message, /message-copies, /extensions, /text-replies, /cc-compact, /extra-rules, /project-instructions, /builtin-skills, /handover, /handover-at, /help, /exit."
)]
pub(crate) struct Args {
    /// Send this message, print the reply and exit. Progress goes to
    /// standard error, the reply to standard output. When the agent starts
    /// workers, Arko waits for their reports, which go to the agent, and
    /// prints its reply to them.
    #[arg(short = 'p', long)]
    pub(crate) prompt: Option<String>,

    /// The model, as an ID or alias (opus, fable, sonnet, haiku, mythos, sol,
    /// astra, terra, luna, gemini, flash), openrouter:<slug> for a model on
    /// OpenRouter (openrouter:anthropic/claude-opus-5.5), or cc:<model> for
    /// a Claude model in your installed Claude Code, on your Claude plan
    /// (cc:opus, cc:fable, cc:haiku). Claude Mythos 5.1 (mythos) needs access
    /// through Anthropic's Cyber Verification Program. By default, the model of the last session you started
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

    /// Who chooses the workers' models. It applies to a new session and to
    /// one you resume, and /worker-models changes it at any time.
    #[arg(long, value_enum, value_name = "agent|profiles|session")]
    pub(crate) worker_models: Option<crate::agents::ModelChoice>,

    /// How many levels of workers the session allows, from 1 to 4. The
    /// default is 2, so a worker the main agent starts can start workers of
    /// its own when the main agent says so. It applies to a new session and
    /// to one you resume.
    #[arg(long, value_name = "1-4", value_parser = clap::value_parser!(u32).range(1..=4))]
    pub(crate) worker_depth: Option<u32>,

    /// A worker fails after this many failed model requests in a row, and
    /// the workers it started stop with it. Without a number the cap is 10.
    /// There is no cap by default. It applies to a new session and to one
    /// you resume, and /worker-retry-cap changes it.
    #[arg(
        long,
        value_name = "N",
        num_args = 0..=1,
        default_missing_value = "10",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub(crate) worker_retry_cap: Option<u32>,

    /// Whether workers may run on models billed per token (API keys,
    /// Gemini, OpenRouter): plans keeps them on your Claude and ChatGPT
    /// plans, ask (the default) asks you before each, and api allows them.
    /// It applies to a new session and to one you resume, and
    /// /worker-billing changes it at any time. ARKO_WORKER_BILLING sets the
    /// default for new sessions.
    #[arg(long, value_enum, value_name = "plans|ask|api")]
    pub(crate) worker_billing: Option<crate::billing::WorkerBilling>,

    /// A daily API budget in dollars, or off. Once the day's spend billed
    /// per token reaches it, across your sessions, new workers on API models
    /// don't start. It applies to a new session and to one you resume, and
    /// /api-budget changes it. ARKO_API_BUDGET sets the default for new
    /// sessions.
    #[arg(long, value_name = "DOLLARS|off", value_parser = parse_api_budget)]
    pub(crate) api_budget: Option<ApiBudget>,

    /// The agent that started a worker gets a copy of each message you send
    /// the worker. It applies to a new session and to one you resume, and
    /// /message-copies changes it at any time.
    #[arg(long = "message-copies", conflicts_with = "no_message_copies")]
    pub(crate) message_copies: bool,

    /// The agent that started a worker doesn't see the messages you send
    /// the worker, which is the default for a new session.
    #[arg(long = "no-message-copies")]
    pub(crate) no_message_copies: bool,

    /// Fast mode for workers on another model than the main one.
    #[arg(long)]
    pub(crate) worker_fast: bool,

    /// The long prompt cache, on the models that have one. Claude models (by
    /// API key, on OpenRouter or in Claude Code) keep it an hour instead of 5
    /// minutes, and a cache write costs 2x the input price instead of 1.25x.
    /// OpenAI GPT-5.6 and later on an API key keep it 12 hours instead of 30
    /// minutes, and a write costs 2.5x instead of 1.25x. Reads cost the same.
    /// Without it Claude Code chooses its own (an hour on a Claude plan).
    /// ARKO_CACHE_1H (1 or 0, in the environment or a .env file) sets the
    /// default, and --cache-1h is its earlier name.
    #[arg(
        long = "long-cache",
        alias = "cache-1h",
        conflicts_with = "no_cache_1h"
    )]
    pub(crate) cache_1h: bool,

    /// The short prompt cache, whatever ARKO_CACHE_1H says (also
    /// --no-cache-1h).
    #[arg(long = "no-long-cache", alias = "no-cache-1h")]
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

    /// The agents get none of Arko's built-in skills. ARKO_BUILTIN_SKILLS=0
    /// (in the environment or a .env file) does it for every new session.
    /// When resuming, it changes the conversations that start after.
    #[arg(long = "no-builtin-skills")]
    pub(crate) no_builtin_skills: bool,

    /// The agents don't get this built-in skill. Repeat the flag or separate
    /// names with commas. ARKO_BUILTIN_SKILLS_OFF (names separated by
    /// commas, in the environment or a .env file) does it for every new
    /// session. When resuming, it changes the conversations that start
    /// after.
    #[arg(
        long = "no-builtin-skill",
        value_name = "NAME",
        value_delimiter = ',',
        value_parser = clap::builder::PossibleValuesParser::new(crate::builtin_skills::names()),
    )]
    pub(crate) no_builtin_skill: Vec<String>,

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
    /// The default is 950k on Claude models by API key and on cc: models
    /// (95% of what a smaller one accepts), 500k on Claude models on
    /// OpenRouter, 250k for OpenAI models on the ChatGPT login (whose
    /// backend accepts 272k), and 200k on others. Those last three are at
    /// most 80% of what the model accepts (100k when OpenRouter's catalog
    /// can't say).
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

    /// Move a shell command or another long call (an MCP tool's,
    /// index_create) still running after this long (90s, 10m or 1h, at
    /// most 60m, or off) to the background, where it goes on as a job while
    /// the agent goes on with its work. The default is 10m in a session you
    /// take part in, and off with -p, where it moves shell commands only.
    /// For a new or resumed session, which keeps it.
    #[arg(long, value_name = "DURATION|off", value_parser = shell::parse_background_after)]
    pub(crate) background_after: Option<u64>,

    /// How long after your last message a monitor's warning still wakes the
    /// agent, so it can keep the monitor (30m or 2h, at most 24h, auto,
    /// always, or off). The default is auto, which lasts 90% of the main
    /// agent's prompt cache (an hour where the provider gives none). Later
    /// the warning goes with your next message. For a new or resumed
    /// session, which keeps it.
    #[arg(long, value_name = "DURATION|auto|always|off", value_parser = crate::monitors::parse_wakes)]
    pub(crate) monitor_wakes: Option<crate::monitors::Wakes>,

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
    /// ARKO_SHELL, else powershell. Elsewhere commands run in bash (else sh).
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

/// `--api-budget`'s value: dollars, or none for `off`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ApiBudget(Option<f64>);

fn parse_api_budget(text: &str) -> Result<ApiBudget, String> {
    crate::billing::parse_budget(text).map(ApiBudget)
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
            worker_models: self.worker_models,
            worker_depth: self.worker_depth,
            worker_retry_cap: self.worker_retry_cap,
            message_copies: flag(self.message_copies, self.no_message_copies),
            worker_billing: self.worker_billing,
            api_budget: self.api_budget.map(|ApiBudget(budget)| budget),
            cache_1h: flag(self.cache_1h, self.no_cache_1h),
            text_replies: flag(self.text_replies, self.no_text_replies),
            cc_compact: flag(self.cc_compact, self.no_cc_compact),
            extra_rules: flag(self.extra_rules, self.no_extra_rules)
                .map(|on| if on { ExtraRules::All } else { ExtraRules::Off }),
            builtin_skills: self.no_builtin_skills.then_some(false),
            builtin_skills_off: Some(self.no_builtin_skill.clone())
                .filter(|names| !names.is_empty()),
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
            monitor_wakes: self.monitor_wakes,
            show_thinking: self.show_thinking,
            hide_thinking: self.hide_thinking,
            codex_auth: self.codex_auth.clone(),
            openai_auth: self.openai_auth,
            shell: self.shell,
            mcp_config: self.mcp_config.clone(),
            mcp_config_required: true,
            remote: self.remote.as_deref().map(remote::parse_bind).transpose()?,
            remote_port_fallback: false,
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

/// The `arko` command. It reads the command line, runs what it asks for,
/// and says how it ended.
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
    // A name a model is still being asked for ends with the process, and so
    // does the Claude Code it runs.
    crate::session_names::stop_all();
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
    let lines = session_lines(root);
    if lines.is_empty() {
        println!("No sessions yet ({}).", root.join("sessions").display());
        return;
    }
    for line in lines {
        println!("{line}");
    }
}

/// The sessions in Arko's folder `root`, a line each: its ID, model and
/// folder, and its name (the one the user gave it, a model's, or its first
/// words, as everything else shows it).
fn session_lines(root: &Path) -> Vec<String> {
    session::list(root)
        .into_iter()
        .map(|(session, settings)| {
            let conversations =
                crate::conversations::Conversations::load(&session.dir).unwrap_or_default();
            format!(
                "{}  {}  {}  \"{}\"",
                session.id,
                settings.main.model,
                settings.workspace.display(),
                crate::sessions::title(&session, &conversations, &settings)
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{Effort, ModelSettings, resolve_model};

    /// The listing names each session as the sidebar of an app does, the
    /// short name from its first words, a model's, or the user's.
    #[test]
    fn the_listing_shows_each_sessions_name() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let luna = ModelSettings {
            model: resolve_model("luna").unwrap().id,
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        assert!(session_lines(&root).is_empty());
        let session = session::Session::create(&root).unwrap();
        let first = "Fix the flaky scroll test in the sidebar before the release";
        session
            .save_settings(&session::Settings {
                title: Some(crate::ui::cut(first, 80)),
                ..session::Settings::for_test(temp.path(), &luna)
            })
            .unwrap();
        let line = || session_lines(&root).remove(0);
        let expect = |name: &str| {
            assert!(
                line().ends_with(&format!("  \"{name}\"")),
                "{name}: {}",
                line()
            );
        };
        assert!(line().starts_with(&format!("{}  gpt-6-luna  ", session.id)));
        expect("Fix the flaky scroll test in…");
        session.set_generated_name("Flaky scroll test").unwrap();
        expect("Flaky scroll test");
        session.rename("Pears").unwrap();
        expect("Pears");
    }
}

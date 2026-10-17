//! What a front end asks of a session as it opens it: which session, and,
//! for a new one, its models and settings. The command line's flags become
//! [`Options`] (`cli`), and an app fills them in itself.

use std::path::PathBuf;

use crate::agents::{Effort, ExtraRules};
use crate::project;
use crate::providers::OpenAiAuth;
use crate::remote;
use crate::shell::WindowsShell;

/// Which session to open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SessionChoice {
    /// A new session.
    #[default]
    New,
    /// The session with this ID.
    Resume(String),
    /// The latest session in the workspace.
    ContinueLatest,
}

/// What a session opens with. A resumed session keeps its models and the
/// settings it was started with. The options it can change as it resumes
/// are the project instructions, when commands move to the background,
/// outside access, the handover size and the MCP config files.
#[derive(Debug, Clone)]
pub struct Options {
    /// Arko's folder, by default `ARKO_HOME`, else `arko` in the data
    /// directory.
    pub root: Option<PathBuf>,
    pub session: SessionChoice,
    /// The working directory of a new session (also where `ContinueLatest`
    /// looks), by default the current directory.
    pub workspace: Option<PathBuf>,
    /// The main agent's model, by default the last session's, else one the
    /// credentials allow.
    pub model: Option<String>,
    pub effort: Effort,
    pub fast: bool,
    /// The workers' model, by default the main agent's (or the last
    /// session's workers').
    pub worker_model: Option<String>,
    /// The workers' effort, by default the main agent's.
    pub worker_effort: Option<Effort>,
    pub worker_fast: bool,
    /// The 1-hour prompt cache, by default as `ARKO_CACHE_1H` says.
    pub cache_1h: Option<bool>,
    /// The main agent ends its turns with a plain reply. By default it does
    /// when someone takes part in the session as it runs (it is
    /// [`interactive`](Self::interactive), or the web page serves it) and it
    /// isn't one-shot.
    pub text_replies: Option<bool>,
    /// Agents on `cc:` models compact at the handover size, which they do by
    /// default. With `Some(false)` they hand over.
    pub cc_compact: Option<bool>,
    /// Who gets the extra rules, by default the agents on small models.
    pub extra_rules: Option<ExtraRules>,
    /// The project instructions, by default a new session's default, or a
    /// resumed session's own.
    pub project_instructions: Option<project::Files>,
    /// Whether workers get the project instructions, by default as above.
    pub worker_project_instructions: Option<bool>,
    /// How many workers may run at once, with no limit by default.
    pub workers: Option<u32>,
    /// The handover size. For a new session it's each agent's (held under
    /// what its model leaves room for). When resuming, it's the size set for
    /// the main agent and the workers, as `/handover-at` sets it.
    pub handover_at: Option<usize>,
    /// The workers' handover size. For a new session it's each worker's
    /// (held under what its model leaves room for). When resuming, it's the
    /// size set for the workers. By default it's `handover_at`.
    pub workers_handover_at: Option<usize>,
    /// Outside access on from the start (also when resuming).
    pub allow_outside_workspace: bool,
    /// After how many seconds shell commands move to the background (0 for
    /// never), by default the session's own, or never when one-shot.
    pub background_after: Option<u64>,
    /// Show the agents' thinking whole from the start (a new session).
    pub show_thinking: bool,
    /// Neither ask the models for the text of their thinking nor show it
    /// (a new session).
    pub hide_thinking: bool,
    /// The Codex auth file of the ChatGPT login.
    pub codex_auth: Option<PathBuf>,
    pub openai_auth: Option<OpenAiAuth>,
    /// The shell on Windows, by default `ARKO_SHELL`, else cmd.exe.
    pub shell: Option<WindowsShell>,
    /// MCP config files besides the project's and Arko's.
    pub mcp_config: Vec<PathBuf>,
    /// A file of `mcp_config` that can't be read keeps the session from
    /// opening (`--mcp-config`, given for one run). Without it (an app's
    /// files, kept as a setting) its servers aren't started, and a note says
    /// why. In an app's session the transcript gets every MCP note as a
    /// warning as the session opens, such as a server left out of a file,
    /// or a project's servers that wait for the user's answer.
    pub mcp_config_required: bool,
    /// Serve the web page from the start, there.
    pub remote: Option<remote::Bind>,
    /// The session runs one message and ends (`-p`). The main agent ends its
    /// turns with `complete_task`, commands stay in the foreground, and the
    /// next session's default models stay as they are.
    pub one_shot: bool,
    /// Someone at the front end takes part as the session runs (at a
    /// terminal, in an app). The agent's questions are put to them, and what
    /// they type while it works reaches it as notes. Else the agent is told
    /// no one can answer, and each line is a turn of its own.
    pub interactive: bool,
    /// A new session's title, or none until it has one.
    pub title: Option<String>,
    /// Who opens the session, as the app's identity, kept in the session
    /// while it is open (`arko` for the command line).
    pub app: &'static str,
    /// The app's name as people read it. Others trying to open the session
    /// are told it has it open there, and the web page tags what the user
    /// typed in it ("typed in …").
    pub app_name: &'static str,
    /// The front end shows the web page's links itself. The transcript
    /// (which the page's clients get too) then says where the page is
    /// served, without the session's key, and where the links and their QR
    /// codes are.
    pub shows_page_links: bool,
    /// Where the user's input comes from (the terminal, or an app), as the
    /// feed tags it.
    pub source: remote::Source,
    /// A new session's kind, an agent working in `workspace` with its tools
    /// and workers, or a chat with none (which works in no folder).
    pub mode: crate::session::Mode,
    /// A new chat's instructions from the user.
    pub chat_instructions: Option<String>,
    /// A client to use in place of the one the session's credentials make
    /// (tests: the mock model server's).
    #[cfg(any(test, feature = "testing"))]
    pub client: Option<GivenClient>,
}

/// A client made beforehand: see [`Options::client`].
#[cfg(any(test, feature = "testing"))]
#[derive(Clone)]
pub struct GivenClient(pub std::sync::Arc<arko_ai::LLMClient>);

#[cfg(any(test, feature = "testing"))]
impl std::fmt::Debug for GivenClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GivenClient")
    }
}

impl Options {
    /// The client given in place of the session's own, if any.
    pub(crate) fn given_client(&self) -> Option<std::sync::Arc<arko_ai::LLMClient>> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(GivenClient(client)) = &self.client {
            return Some(client.clone());
        }
        None
    }
}

impl Default for Options {
    /// A new session in the current directory, with someone taking part.
    fn default() -> Self {
        Self {
            root: None,
            session: SessionChoice::New,
            workspace: None,
            model: None,
            effort: Effort::Xhigh,
            fast: false,
            worker_model: None,
            worker_effort: None,
            worker_fast: false,
            cache_1h: None,
            text_replies: None,
            cc_compact: None,
            extra_rules: None,
            project_instructions: None,
            worker_project_instructions: None,
            workers: None,
            handover_at: None,
            workers_handover_at: None,
            allow_outside_workspace: false,
            background_after: None,
            show_thinking: false,
            hide_thinking: false,
            codex_auth: None,
            openai_auth: None,
            shell: None,
            mcp_config: Vec::new(),
            mcp_config_required: false,
            remote: None,
            one_shot: false,
            interactive: true,
            title: None,
            app: "app",
            app_name: "an app",
            shows_page_links: false,
            source: remote::Source::App,
            mode: crate::session::Mode::Agent,
            chat_instructions: None,
            #[cfg(any(test, feature = "testing"))]
            client: None,
        }
    }
}

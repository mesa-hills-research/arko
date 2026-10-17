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
mod chat;
mod claude_code;
#[cfg(feature = "tui")]
mod cli;
pub mod commands;
mod context;
mod conversations;
mod credentials;
mod crew;
pub mod details;
mod engine;
pub mod extensions;
mod glyphs;
mod handover;
mod harness;
pub mod history;
pub mod images;
mod index;
mod inputs;
mod keys;
pub mod logging;
mod mcp;
mod message;
#[cfg(any(test, feature = "testing"))]
mod mock_model;
mod model_command;
pub mod models;
mod options;
mod outside;
mod project;
mod prompter;
mod providers;
pub mod remote;
#[cfg(test)]
mod remote_demo;
mod session;
pub mod sessions;
mod shell;
mod skills;
mod start;
mod status;
#[cfg(feature = "tui")]
mod term;
#[cfg(feature = "tui")]
mod terminal;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
#[cfg(feature = "tui")]
mod tui;
mod ui;

use std::sync::Mutex;

pub use crate::agents::{Effort, ExtraRules};
#[cfg(feature = "tui")]
pub use crate::cli::cli_main;
pub use crate::credentials::{Env, Need};
pub use crate::keys::Keys;
pub use crate::project::Files as ProjectFiles;
pub use crate::providers::OpenAiAuth;
pub use crate::shell::WindowsShell;
pub use crate::ui::{Attachment, Banner, Input, Message, QuitAsk};

/// API keys and logins, for an app's accounts page (no session needed).
pub mod accounts {
    pub use crate::claude_code::{
        ClaudeCodeStatus, Problem, Ready, add_to_path, find_installed, install_dirs,
        status as claude_code,
    };
    pub use crate::credentials::{
        ChatGptLogin, KEYS, Key, KeySource, KeyState, LoginSource, LoginState, chatgpt_login,
        chatgpt_login_in, clear_chatgpt_login, key_states, key_states_in, set_chatgpt_login,
        use_env,
    };
    pub use crate::keys::{Outcome, Secret, add, remove};
}
pub use crate::engine::{
    Engine, FrontEnd, OpenError, Refreshed, Reply, Request, RunEnd, Running, Seam, SessionInfo,
    spawn,
};
pub(crate) use crate::harness::{Conversation, Harness, conversation_of};
#[cfg(test)]
pub(crate) use crate::harness::{Managers, tests};
pub use crate::options::{Options, SessionChoice};
pub use crate::prompter::{McpApproval, NoPrompter, Prompter};
pub use crate::remote::Level;
pub use crate::session::{Holder, Mode};
#[cfg(test)]
pub(crate) use crate::status::Gauges;

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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

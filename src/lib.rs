//! Arko is a general-purpose agent for the terminal, and a library for apps
//! that run its sessions.
//!
//! In a session the user talks with one agent, which works with a shell,
//! background terminal jobs and file tools in the working directory. It can
//! start workers that run while it goes on, each in the working directory or
//! in a git worktree of its own. A message sent while the agent is idle
//! starts a turn of one conversation, and lines typed while it works reach it
//! as notes. The workers' reports reach it as notes too, and start a turn
//! when it is idle. Ctrl-C stops only the main agent, and the next message
//! reopens its conversation. Every session is saved and can be resumed.
//!
//! Besides the shell, the agents get the tools of the MCP servers set up, the
//! skills in the skill folders, and semantic search over indexes of code and
//! docs that Arko keeps current (also run from the command line as
//! `arko index`).
//!
//! An app opens a session with [`Engine::open`] and runs it with
//! [`Engine::run`], or runs several side by side in one process with
//! [`spawn`]. The session's [`Seam`] holds the feed of items the app shows
//! and the channels it sends the user's [`Input`] and its [`Request`]s on.
//! [`accounts`] adds and removes API keys and logins without a session,
//! [`sessions`] lists, renames, branches and deletes saved sessions, and
//! [`session_names`] sets how they are named.
//!
//! The `tui` feature builds the command line and the terminal's front ends,
//! and `remote` the web page's server. Both are on by default. An app builds
//! Arko with `default-features = false` and adds `remote` for the web page.

mod agents;
pub mod approvals;
pub mod arko_dir;
mod asking;
mod billing;
pub mod builtin_skills;
mod cache_memory;
pub mod cache_timer;
mod chat;
mod chooser;
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
pub mod extension_prefs;
pub mod extensions;
mod glyphs;
mod handover;
mod harness;
pub mod history;
pub mod images;
mod index;
mod inputs;
mod jobs;
pub mod kept_notes;
mod keys;
pub mod logging;
mod logins;
mod mcp;
mod message;
#[cfg(any(test, feature = "testing"))]
mod mock_model;
mod model_command;
pub mod models;
pub mod monitors;
mod openings;
pub mod openrouter;
mod options;
mod outside;
pub mod profiles;
mod project;
mod prompter;
mod providers;
pub mod questions;
pub mod remote;
#[cfg(test)]
mod remote_demo;
mod session;
pub mod session_names;
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
mod worker_commands;
mod worker_tools;
pub mod workers;

use std::sync::Mutex;

pub use crate::agents::{Effort, ExtraRules, ModelChoice};
pub use crate::billing::WorkerBilling;
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
        ClaudeCodeStatus, Problem, Ready, add_to_path, find_installed, install_dirs, outdated_text,
        status as claude_code,
    };
    pub use crate::credentials::{
        ChatGptLogin, KEYS, Key, KeySource, KeyState, LoginSource, LoginState, add_chatgpt_login,
        chatgpt_login, chatgpt_login_in, chatgpt_logins_in, clear_chatgpt_login,
        first_chatgpt_login, key_states, key_states_in, login_changes, remove_chatgpt_login,
        set_chatgpt_login, use_env,
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

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
mod cli;
mod commands;
mod conversations;
mod credentials;
mod crew;
mod glyphs;
mod handover;
mod harness;
mod index;
mod inputs;
mod keys;
mod mcp;
mod message;
#[cfg(test)]
mod mock_model;
mod model_command;
mod options;
mod outside;
mod project;
mod prompter;
mod providers;
mod remote;
#[cfg(test)]
mod remote_demo;
mod session;
mod shell;
mod skills;
mod status;
mod term;
mod terminal;
mod tui;
mod ui;

use std::sync::Mutex;

pub use crate::cli::cli_main;
pub use crate::credentials::Need;
pub(crate) use crate::harness::{Conversation, Harness, conversation_of};
#[cfg(test)]
pub(crate) use crate::harness::{Managers, tests};
pub use crate::options::{Options, SessionChoice};
pub use crate::prompter::{McpApproval, NoPrompter, Prompter};
pub use crate::remote::Level;
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

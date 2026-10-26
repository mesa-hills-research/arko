//! What the remote's server and its page say to each other, as JSON.
//!
//! The page built in Rust (`web/`) compiles this file as a module of its
//! own, so it depends on serde alone: no `crate::` or `super::` paths. The
//! server writes these types, and the page reads them. Both ship in the
//! same binary, so they never differ in version.

use serde::{Deserialize, Serialize};

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Kind {
    /// The user's message, note, answer or command.
    User,
    /// An agent's text, as it streams.
    Text,
    /// An agent's thinking (its reasoning, or its summary), as it streams.
    /// Clients show it folded. The user reads the agent's text, not this.
    Thinking,
    /// The main agent's message to the user while it works, or a message a
    /// worker got (`from` says whose).
    Message,
    /// A tool call, whose `status` and `outcome` follow as an
    /// `Update::Change`.
    Call,
    /// A result that needs a block under its call, such as a command's
    /// output or a long error.
    Result,
    /// What Arko says, such as a stop, an error, a retry or a handover.
    Notice,
    /// A command's output.
    Output,
    /// The main agent's question.
    Question,
    /// The main agent's reply, which ends its turn, or a worker's report,
    /// which ends its run.
    Reply,
    /// The end of a turn, with its time and cost.
    Turn,
    /// Where a worker started, in the main transcript. Clients draw the
    /// worker's `WorkerSummary` there.
    WorkerLine,
    /// Where the conversation changes (`/new`, `/resume`), so what follows
    /// is another conversation's. A client that shows one conversation at a
    /// time shows what follows the last of these, and one that keeps the
    /// whole session draws it as a separator.
    Conversation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// Where the user's text was typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Source {
    Terminal,
    Web,
    /// An app in this process.
    App,
}

/// What became of the user's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Delivery {
    /// It started a turn.
    Turn,
    /// It went to the working agent as a note, `pending` until the agent
    /// reads it.
    Note,
    /// It answered the agent's question.
    Answer,
    /// It was a command.
    Command,
    /// It waits for a turn of its own.
    Queued,
    /// Arko sent it for the user, to keep the prompt cache warm
    /// (`REFRESH_MESSAGE`). An app shows it and its reply as one muted
    /// row.
    Refresh,
}

/// How a tool call is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CallStatus {
    /// No result yet.
    Running,
    Ok,
    /// It failed, was refused, or ended with its turn or worker before its
    /// result came.
    Failed,
}

/// The question the main agent waits on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: u64,
    pub text: String,
}

/// Whether the main agent works, and what it asks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub working: bool,
    pub question: Option<Question>,
    /// When the turn started (Unix milliseconds), while one runs.
    pub since: Option<i64>,
}
// ----- what the page reads -----
//
// The server writes richer types than these (its items hold what only the
// terminal and apps get), so the page reads them with types of its own.
// Arko's tests read every item the feed writes with them, and refuse any
// field they don't know.

/// An item as the stream sends it (`item`, `change`, and in `reset`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
#[allow(dead_code)]
pub struct ItemData {
    pub id: u64,
    pub at: i64,
    pub kind: Kind,
    pub agent: String,
    pub turn: Option<u64>,
    pub text: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub level: Option<Level>,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub delivery: Option<Delivery>,
    #[serde(default)]
    pub status: Option<CallStatus>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub call: Option<u64>,
    #[serde(default)]
    pub job: Option<String>,
    #[serde(default)]
    pub pending: bool,
}

/// More text for an item (`append`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Append {
    pub id: u64,
    pub text: String,
}

/// An item that went (`remove`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Removed {
    pub id: u64,
}

/// The part of the status (`status`, and in `reset`) the page reads. The
/// rest is left unread.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
#[allow(dead_code)]
pub struct StatusData {
    pub working: bool,
    pub question: Option<Question>,
    pub since: Option<i64>,
    pub cost: f64,
    pub context_tokens: Option<usize>,
}

/// The start of a stream, and of each start over (`reset`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[allow(dead_code)]
pub struct ResetData {
    pub items: Vec<ItemData>,
    #[serde(default)]
    pub status: StatusData,
}

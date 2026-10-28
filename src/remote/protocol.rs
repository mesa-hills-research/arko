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

/// The question card the main agent waits on: its item `id`, and its
/// text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: u64,
    pub text: String,
}

/// Whether the main agent works, and what it asks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub working: bool,
    /// The question card the main agent waits on (it asked with `wait`).
    /// All its open questions, those it doesn't wait on too, are the
    /// status's `questions`.
    pub question: Option<Question>,
    /// When the turn started (Unix milliseconds), while one runs.
    pub since: Option<i64>,
}
/// Where a worker is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkerState {
    /// Waiting for a place (the most workers run).
    Queued,
    Running,
    /// It reported.
    Finished,
    /// It ended with an error.
    Failed,
    /// Its lead, the user or Arko's exit stopped it.
    Stopped,
}

impl WorkerState {
    /// Running or waiting to run.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// What a background job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum JobKind {
    /// A shell command that moved to the background, or one
    /// `terminal_start` started.
    Command,
    /// A tool call that moved to the background (an MCP tool's,
    /// `index_create`).
    Call,
    /// A monitor that watches a command for its agent (`monitor_start`).
    Monitor,
}

/// How a background job is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum JobState {
    Running,
    /// It ended well.
    Finished,
    /// It failed, or ran out of time.
    Failed,
    /// The agent or the user stopped it, or Arko's exit did.
    Stopped,
    /// Arko stopped before it ended (a crash, or `arko -c` after one).
    Interrupted,
    /// A monitor that lasted as long as it was armed for.
    Expired,
}

impl JobState {
    pub fn is_running(self) -> bool {
        self == Self::Running
    }
}

/// A background job, as a line such as `index_create · 3 min · running`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JobSummary {
    /// Its ID, as the terminal tools name it.
    pub id: String,
    /// Whose it is, `main` or a worker's ID.
    pub agent: String,
    /// The call item that made it, while the transcript has it.
    pub call: Option<u64>,
    pub kind: JobKind,
    /// The command, or the tool of a moved call.
    pub label: String,
    pub state: JobState,
    /// When it started (Unix milliseconds). A moved call's job starts as it
    /// moves.
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// How it ended, in a few words, once it did ("exit 1", "timed out").
    pub outcome: Option<String>,
    /// What a monitor watches ("lines that match error|warn", "a check
    /// every 15 s").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watching: Option<String>,
    /// How many of its agent's notes a monitor's alerts went in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alerts: Option<u32>,
    /// When a monitor ends, as last armed (Unix milliseconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
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
    /// Whether this notice opens a turn Arko started without a message from
    /// the user.
    #[serde(default)]
    pub opens_turn: bool,
    /// The highlighting of its Markdown's finished code blocks.
    #[serde(default)]
    pub code: Vec<CodeSpans>,
    /// A question card's questions, with what became of each. `None` for a
    /// question asked before questions had ids.
    #[serde(default)]
    pub questions: Option<Vec<QuestionData>>,
    /// The images it shows, which the user sent with a message or a call's
    /// tool returned.
    #[serde(default)]
    pub images: Vec<ImageData>,
}

/// The types of image the page shows, the only ones Arko serves it.
pub const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// An image an item shows, whose bytes are at `url`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub struct ImageData {
    pub id: u64,
    /// Its MIME type, such as `image/png`.
    pub mime: String,
    /// What it is, such as a file's name.
    pub label: String,
    /// Its size in bytes.
    pub bytes: u64,
    /// Where the page gets it. The address holds the server's version, so a
    /// browser may keep it as long as the server runs.
    pub url: String,
}

/// One of the main agent's questions, as a card and the status give it.
/// The user never sees its `id`: a question shows its `number` in its ask
/// (1, 2, 3), or none when it came alone.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
#[allow(dead_code)]
pub struct QuestionData {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub choices: Vec<String>,
    /// Several choices may be picked.
    #[serde(default)]
    pub multiple: bool,
    /// What "You decide" means.
    #[serde(default)]
    pub default: Option<String>,
    /// `open`, `answered` (with `answer`), `answered_in_message`, `closed`
    /// (with `why`) or `dismissed`.
    pub state: String,
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default)]
    pub why: Option<String>,
    /// The agent waits for the answer.
    #[serde(default)]
    pub waits: bool,
    /// The item of its question card.
    #[serde(default)]
    pub item: u64,
    #[serde(default)]
    pub number: Option<u32>,
    /// A worker profile Arko asks the user to approve, for the agent.
    #[serde(default)]
    pub approval: Option<ApprovalData>,
}

/// What every front end shows, muted, right after the text of a question
/// that takes any number of its choices.
pub const CHOOSE_ANY: &str = "Choose any";

/// The choice that saves an approved profile.
pub const SAVE: &str = "Save";
/// The choice that doesn't save it.
pub const DONT_SAVE: &str = "Don't save";

/// A worker profile to approve, with Save or Don't save.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
#[allow(dead_code)]
pub struct ApprovalData {
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
    /// "this project" or "every project".
    pub scope: String,
    pub description: String,
    #[serde(default)]
    pub instructions: String,
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

/// A worker's summary (`worker`, and in `reset`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
#[allow(dead_code)]
pub struct WorkerData {
    pub id: String,
    pub title: String,
    pub state: WorkerState,
    pub tool_calls: u32,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub last: Option<String>,
    pub report: Option<String>,
    #[serde(default)]
    pub ran_ms: u64,
    pub running_since: Option<i64>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub profile: Option<String>,
    pub parent: Option<String>,
    #[serde(default)]
    pub depth: u32,
    #[serde(default, deserialize_with = "lenient")]
    pub cost_billed: f64,
    #[serde(default, deserialize_with = "lenient")]
    pub cost_list: f64,
    #[serde(default, deserialize_with = "lenient")]
    pub family_cost_billed: f64,
    #[serde(default, deserialize_with = "lenient")]
    pub family_cost_list: f64,
}

/// The part of the status (`status`, and in `reset`) the page reads. The
/// rest is left unread, and a value that is `null` (a cost that isn't a
/// number) reads as none.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
#[allow(dead_code)]
pub struct StatusData {
    #[serde(deserialize_with = "lenient")]
    pub working: bool,
    pub question: Option<Question>,
    pub since: Option<i64>,
    #[serde(deserialize_with = "lenient")]
    pub cost: f64,
    pub api_price: Option<f64>,
    pub context_tokens: Option<u64>,
    pub plan_usage: Option<String>,
    /// Each worker, a line or two.
    pub workers: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub settings: SettingsData,
    #[serde(deserialize_with = "lenient")]
    pub movable: Vec<u64>,
    #[serde(deserialize_with = "lenient")]
    pub models: ModelsData,
    #[serde(deserialize_with = "lenient")]
    pub agents: Vec<AgentData>,
    #[serde(deserialize_with = "lenient")]
    pub cache: CacheData,
    #[serde(deserialize_with = "lenient")]
    pub info: InfoData,
    /// The current conversation's questions that wait for the user, in the
    /// order asked.
    #[serde(deserialize_with = "lenient")]
    pub questions: Vec<QuestionData>,
}

/// The session's settings a client may change, with commands.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
#[allow(dead_code)]
pub struct SettingsData {
    #[serde(deserialize_with = "lenient")]
    pub outside: bool,
    #[serde(deserialize_with = "lenient")]
    pub handover_at: u64,
    #[serde(deserialize_with = "lenient")]
    pub workers_handover_at: u64,
    #[serde(deserialize_with = "lenient")]
    pub text_replies: bool,
    pub cc_compact: Option<bool>,
    /// `small`, `all` or `off`.
    pub extra_rules: Option<String>,
    /// `one`, `both` or `off`.
    pub project_instructions: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub worker_project_instructions: bool,
    /// Seconds, 0 for never.
    #[serde(deserialize_with = "lenient")]
    pub background_after: u64,
    /// As `/monitor-wakes` takes it (`auto`, `2h`).
    pub monitor_wakes: Option<String>,
    /// Seconds now, 0 for never, none for always.
    pub monitor_wakes_within: Option<u64>,
    /// `agent`, `profiles` or `session`.
    pub worker_models: Option<String>,
    /// `plans`, `ask` or `api`.
    pub worker_billing: Option<String>,
    /// Dollars a day, none for no budget.
    pub api_budget: Option<f64>,
    pub worker_depth: Option<u32>,
    pub worker_retry_cap: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub message_copies: bool,
    #[serde(deserialize_with = "lenient")]
    pub builtin_skills: bool,
    #[serde(deserialize_with = "lenient")]
    pub builtin_skill_list: Vec<SkillData>,
}

/// A built-in skill and whether it is on.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[allow(dead_code)]
pub struct SkillData {
    pub name: String,
    pub on: bool,
}

/// The agents' models.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
#[allow(dead_code)]
pub struct ModelsData {
    pub main: String,
    pub workers: String,
    #[serde(deserialize_with = "lenient")]
    pub started: bool,
    #[serde(deserialize_with = "lenient")]
    pub has_workers: bool,
}

/// A worker's context, for its own view.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
#[allow(dead_code)]
pub struct AgentData {
    pub id: String,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub handover_at: Option<u64>,
    #[serde(deserialize_with = "lenient")]
    pub compacts: bool,
}

/// The main agent's prompt cache, as its latest request left it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
#[allow(dead_code)]
pub struct CacheData {
    /// Milliseconds.
    pub lifetime: Option<i64>,
    pub refreshed_at: Option<i64>,
    pub tokens: Option<u64>,
    pub read: Option<u64>,
    pub written: Option<u64>,
    pub miss_cost: Option<f64>,
    pub refresh_cost: Option<f64>,
    pub on_plan: Option<String>,
}

/// What the header says of the session.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
#[allow(dead_code)]
pub struct InfoData {
    pub model: String,
    pub workspace: String,
    pub session: String,
    pub app: String,
}

/// The start of a stream, and of each start over (`reset`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[allow(dead_code)]
pub struct ResetData {
    pub items: Vec<ItemData>,
    #[serde(default)]
    pub workers: Vec<WorkerData>,
    #[serde(default)]
    pub jobs: Vec<JobSummary>,
    #[serde(default)]
    pub status: StatusData,
}

/// A command the page runs (`GET /api/commands`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[allow(dead_code)]
pub struct CommandData {
    pub name: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub about: String,
}

/// A link to the page (`GET /api/links`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[allow(dead_code)]
pub struct LinkData {
    pub url: String,
    pub interface: Option<String>,
    /// `lan`, `tailscale`, `vpn`, `other` or `loopback`.
    pub kind: String,
    pub qr_svg: Option<String>,
}

/// A value, or its type's default where the JSON has `null`.
#[allow(dead_code)]
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

// ----- code highlighting -----

/// The highlight names of a code block's spans, by their index: GPUI Kit's,
/// whose styles the themes give (`highlight.syntax`). A capture that isn't
/// one of them takes the name its first word is (`function.method` is a
/// `function`), as the kit's themes do.
pub const HIGHLIGHT_NAMES: [&str; 41] = [
    "attribute",
    "boolean",
    "comment",
    "comment.doc",
    "constant",
    "constructor",
    "embedded",
    "emphasis",
    "emphasis.strong",
    "enum",
    "function",
    "hint",
    "keyword",
    "label",
    "link_text",
    "link_uri",
    "number",
    "operator",
    "predictive",
    "preproc",
    "primary",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.list_marker",
    "punctuation.special",
    "string",
    "string.escape",
    "string.regex",
    "string.special",
    "string.special.symbol",
    "tag",
    "tag.doctype",
    "text.code.span",
    "text.literal",
    "title",
    "type",
    "variable",
    "variable.special",
    "variant",
];

/// The highlighting of a code block in an item's Markdown: the code it's
/// for (its [`code_hash`]) and its spans, each `[start, end, name]` in bytes
/// of the code, with the name's index in [`HIGHLIGHT_NAMES`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeSpans {
    pub hash: String,
    pub spans: Vec<[u32; 3]>,
}

/// More of an item's code blocks, highlighted once they're finished as it
/// streams (`code`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct CodeData {
    pub id: u64,
    pub code: Vec<CodeSpans>,
}

/// A code block's hash, as [`CodeSpans`] names its code: FNV-1a over its
/// bytes, in hex.
pub fn code_hash(code: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in code.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

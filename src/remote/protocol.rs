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
    /// The worker a `start_agent` call started at once. The Quiet view
    /// leaves out such a call, because the worker's own line stands for it.
    #[serde(default)]
    pub started: Option<String>,
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
}

/// What every front end shows, muted, right after the text of a question
/// that takes any number of its choices.
pub const CHOOSE_ANY: &str = "Choose any";

// ----- approvals -----
//
// What an agent's tool asks the user to allow before it acts (save a worker
// profile or a skill, add an MCP server, start a worker billed per token).
// The requests wait in the approval tray above the message box, the status's
// `approvals`, until the user answers them all at once. The server and the
// page share these types, and the words every front end shows of them.

/// A request in the approval tray: its id in the session (`a3`), the tray
/// it opened in, and what it asks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub struct ApprovalRequest {
    pub id: String,
    /// Requests that came while others waited share their batch. One that
    /// comes to an empty tray starts the next.
    pub batch: u64,
    pub approval: Approval,
}

/// What a request asks the user to approve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Approval {
    /// A worker profile the main agent saves for the project or for every
    /// project.
    Profile(ProfileApproval),
    /// A skill the main agent saves.
    Skill(SkillApproval),
    /// An MCP server the main agent adds.
    McpServer(ServerApproval),
    /// A worker the main agent starts on a model billed per token.
    WorkerBilling(BillingApproval),
}

/// Where a skill or a server is kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// This project's `.arko` folder.
    #[default]
    Project,
    /// Arko's folder, for every project.
    User,
}

impl Scope {
    /// "this project" or "every project".
    pub fn words(self) -> &'static str {
        match self {
            Scope::Project => "this project",
            Scope::User => "every project",
        }
    }
}

/// A worker profile to save.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub struct ProfileApproval {
    /// The profile's name, such as `3d-judge`.
    pub name: String,
    /// The model it runs on, as the profile names it, or "the session's
    /// worker model".
    pub model: String,
    /// Its reasoning effort, such as `high`, unless the model's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Where it is kept: "this project" or "every project".
    pub scope: String,
    /// What it is for, in one line.
    pub description: String,
    /// What it narrows for the workers started with it, a line each, such
    /// as "Workers with it get only the skills ponytail and release-notes."
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<String>,
    /// Its standing instructions (empty without any).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instructions: String,
}

/// A skill to save.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub struct SkillApproval {
    /// Its name, such as `release-notes`.
    pub name: String,
    pub scope: Scope,
    /// Its front matter's description.
    pub description: String,
    pub source: SkillSource,
    /// Workers get it (its front matter has no `workers: false`).
    #[serde(default = "yes")]
    pub workers: bool,
    /// Its front matter's `models`, the models whose agents list it (empty
    /// for all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// The scripts an agent may run, by their paths in the skill
    /// (`scripts/release.sh`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<String>,
    /// When an agent saved the skill of the same name it replaces, such as
    /// "26 Oct".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    /// The agent's reason, for the user.
    pub why: String,
    /// Its files, `SKILL.md` first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<SkillFile>,
}

/// Where a skill to save came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum SkillSource {
    /// The agent wrote it in place.
    #[default]
    Agent,
    /// Copied from a folder or a file, as the user reads its path.
    Copied { path: String },
    /// Downloaded from `url`, `bytes` long.
    Downloaded { url: String, bytes: u64 },
}

/// One of a skill's files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub struct SkillFile {
    /// Its path in the skill, such as `SKILL.md` or `scripts/release.sh`.
    pub path: String,
    pub bytes: u64,
    /// Its text, for a text file the user may read on the card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// An MCP server to add.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub struct ServerApproval {
    /// Its name, such as `github`.
    pub name: String,
    pub scope: Scope,
    /// The command line it runs, as written.
    pub command: String,
    /// The environment variables it gets from the user's environment, by
    /// name. Their values are never shown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvName>,
    /// Workers can use it.
    #[serde(default = "yes")]
    pub workers: bool,
    /// Git ignores the project's file it is kept in.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ignored: bool,
    /// Open sessions would share one copy of it, and why the agent says
    /// that is safe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<String>,
    /// When an agent added the server of the same name it replaces, such as
    /// "26 Oct".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    /// The agent's reason, for the user.
    pub why: String,
}

/// An environment variable a server gets, and whether it is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub struct EnvName {
    pub name: String,
    pub set: bool,
}

/// A worker to start on a model billed per token.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub struct BillingApproval {
    /// The model, as the user reads its name ("Opus 5.5").
    pub model: String,
    /// Its prices in dollars per million input and output tokens, when
    /// known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_price: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_price: Option<f64>,
    /// What the user's sessions were billed per token today, in dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
    /// The daily budget, when today's spend reached it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<f64>,
    /// The worker's task.
    pub task: String,
}

fn yes() -> bool {
    true
}

/// The user's answer to the requests in the tray, which a front end sends
/// once for all it shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalAnswers {
    pub choices: Vec<ApprovalChoice>,
    /// What the message box held, which goes to the agent with each answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub words: Option<String>,
}

/// The user's choice for one request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalChoice {
    pub id: String,
    pub approve: bool,
    /// The request's option is on ([`Approval::option`]), such as allowing
    /// a model for the session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub option: bool,
}

impl ApprovalAnswers {
    /// Approves those of `requests` in `approved` (by id), with the option
    /// on for those in `options`, and rejects the others, with `words`.
    pub fn of(
        requests: &[ApprovalRequest],
        approved: impl Fn(&str) -> bool,
        options: impl Fn(&str) -> bool,
        words: &str,
    ) -> Self {
        let words = words.trim();
        Self {
            choices: requests
                .iter()
                .map(|request| {
                    let approve = approved(&request.id);
                    ApprovalChoice {
                        id: request.id.clone(),
                        approve,
                        option: approve
                            && request.approval.option().is_some()
                            && options(&request.id),
                    }
                })
                .collect(),
            words: (!words.is_empty()).then(|| words.to_string()),
        }
    }
}

/// How a line of a request's details shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// A sentence, whose `code` spans show as code.
    Text,
    /// A muted heading ("Why", "Files").
    Heading,
    /// In the warning style.
    Warning,
    /// Text kept as written, in a box that scrolls (a profile's
    /// instructions, a skill's SKILL.md).
    Box,
    /// One of a skill's files, beside the box.
    File,
}

/// A line of a request's details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    pub look: Look,
    pub text: String,
}

impl Detail {
    fn new(look: Look, text: impl Into<String>) -> Self {
        Self {
            look,
            text: text.into(),
        }
    }
}

/// The longest words the message box may send with an answer, in bytes.
pub const MAX_APPROVAL_WORDS: usize = 16 * 1024;

/// Why `words` can't go with an answer, or `None` when they can.
pub fn approval_words_refusal(words: &str) -> Option<String> {
    (words.trim().len() > MAX_APPROVAL_WORDS).then(|| {
        format!(
            "Your words are too long to send. Shorten them to {} KB.",
            MAX_APPROVAL_WORDS / 1024
        )
    })
}

/// The tray's heading: "1 approval", "3 approvals".
pub fn approvals_heading(count: usize) -> String {
    if count == 1 {
        "1 approval".to_string()
    } else {
        format!("{count} approvals")
    }
}

/// What the button that sends the selection does, with `selected` of
/// `count` requests selected: "Approve", "Approve all", "Approve 2, reject
/// 1", or "Reject all" with none selected.
pub fn approve_label(selected: usize, count: usize) -> String {
    match (selected, count) {
        (_, 1) if selected == 1 => "Approve".to_string(),
        (0, 1) => "Reject".to_string(),
        (0, _) => "Reject all".to_string(),
        (selected, count) if selected >= count => "Approve all".to_string(),
        (selected, count) => format!("Approve {selected}, reject {}", count - selected),
    }
}

/// The button that rejects every request.
pub fn reject_label(count: usize) -> &'static str {
    if count == 1 { "Reject" } else { "Reject all" }
}

impl Approval {
    /// What it is, in a word: Profile, Skill, Server or Worker.
    pub fn kind_word(&self) -> &'static str {
        match self {
            Approval::Profile(_) => "Profile",
            Approval::Skill(_) => "Skill",
            Approval::McpServer(_) => "Server",
            Approval::WorkerBilling(_) => "Worker",
        }
    }

    /// Its name: the profile's, the skill's, the server's, or the worker's
    /// model.
    pub fn name(&self) -> &str {
        match self {
            Approval::Profile(profile) => &profile.name,
            Approval::Skill(skill) => &skill.name,
            Approval::McpServer(server) => &server.name,
            Approval::WorkerBilling(billing) => &billing.model,
        }
    }

    /// Where it applies: "every project", "this project", or "billed per
    /// token" for a worker.
    pub fn scope(&self) -> &str {
        match self {
            Approval::Profile(profile) => &profile.scope,
            Approval::Skill(skill) => skill.scope.words(),
            Approval::McpServer(server) => server.scope.words(),
            Approval::WorkerBilling(_) => "billed per token",
        }
    }

    /// What its row says after its scope: a profile's model and effort, a
    /// skill's description, a server's command, a worker's prices and the
    /// day's spend.
    pub fn summary(&self) -> String {
        match self {
            Approval::Profile(profile) => match &profile.effort {
                Some(effort) => format!("{} {effort}", profile.model),
                None => profile.model.clone(),
            },
            Approval::Skill(skill) => first_line(&skill.description),
            Approval::McpServer(server) => first_line(&server.command),
            Approval::WorkerBilling(billing) => {
                let mut parts = Vec::new();
                if let (Some(input), Some(output)) = (billing.input_price, billing.output_price) {
                    parts.push(format!(
                        "${}/${} per million tokens",
                        per_million(input),
                        per_million(output)
                    ));
                }
                if let Some(spent) = billing.spent {
                    parts.push(format!("{} spent today", money(spent)));
                }
                parts.join(", ")
            }
        }
    }

    /// What it asks, as a question: "Save the skill release-notes for every
    /// project?"
    pub fn title(&self) -> String {
        match self {
            Approval::Profile(profile) => format!(
                "Save the worker profile {} for {}?",
                profile.name, profile.scope
            ),
            Approval::Skill(skill) => {
                format!("Save the skill {} for {}?", skill.name, skill.scope.words())
            }
            Approval::McpServer(server) => format!(
                "Add the MCP server {} for {}?",
                server.name,
                server.scope.words()
            ),
            Approval::WorkerBilling(billing) => {
                format!("Start a worker on {}, billed per token?", billing.model)
            }
        }
    }

    /// What the user may turn on in its details for an approval to cover,
    /// such as "Allow Opus 5.5 for this session".
    pub fn option(&self) -> Option<String> {
        match self {
            Approval::WorkerBilling(billing) => {
                Some(format!("Allow {} for this session", billing.model))
            }
            _ => None,
        }
    }

    /// Its details, line by line, as its row opens to them.
    pub fn details(&self) -> Vec<Detail> {
        fn text(lines: &mut Vec<Detail>, text: String) {
            if !text.trim().is_empty() {
                lines.push(Detail::new(Look::Text, text));
            }
        }
        let mut lines = Vec::new();
        match self {
            Approval::Profile(profile) => {
                text(
                    &mut lines,
                    match &profile.effort {
                        Some(effort) => format!("Runs on {} at {effort} effort.", profile.model),
                        None => format!("Runs on {}.", profile.model),
                    },
                );
                text(&mut lines, profile.description.trim().to_string());
                for limit in &profile.limits {
                    text(&mut lines, limit.trim().to_string());
                }
                let instructions = profile.instructions.trim_end();
                if instructions.trim().is_empty() {
                    text(&mut lines, "It has no instructions.".to_string());
                } else {
                    lines.push(Detail::new(Look::Heading, "Instructions"));
                    lines.push(Detail::new(Look::Box, instructions));
                }
            }
            Approval::Skill(skill) => {
                text(&mut lines, skill.description.trim().to_string());
                text(
                    &mut lines,
                    match &skill.source {
                        SkillSource::Agent => "Written by the agent.".to_string(),
                        SkillSource::Copied { path } => format!("Copied from {path}."),
                        SkillSource::Downloaded { url, bytes } => {
                            format!("Downloaded from {url} ({}).", size(*bytes))
                        }
                    },
                );
                let mut who = if skill.workers {
                    "Workers get it too.".to_string()
                } else {
                    "Only the main agent gets it.".to_string()
                };
                if !skill.models.is_empty() {
                    who.push_str(&format!(
                        " Listed for agents on {}.",
                        and_list(&skill.models)
                    ));
                }
                text(&mut lines, who);
                let mut size_line = String::new();
                if let Some(main) = skill.files.first() {
                    let main_text = main.text.as_deref().unwrap_or_default();
                    let characters = main_text.chars().count();
                    size_line = format!(
                        "{} is {} characters ({} lines)",
                        main.path,
                        thousands(characters as u64),
                        thousands(main_text.lines().count() as u64)
                    );
                    size_line.push_str(&match skill.files.len() - 1 {
                        0 => ".".to_string(),
                        1 => ", with 1 more file.".to_string(),
                        more => format!(", with {more} more files."),
                    });
                }
                if !skill.scripts.is_empty() {
                    size_line.push_str(&format!(
                        " It has scripts an agent may run ({}).",
                        skill.scripts.join(", ")
                    ));
                }
                text(&mut lines, size_line.trim().to_string());
                if let Some(when) = &skill.replaces {
                    text(
                        &mut lines,
                        format!(
                            "It replaces the skill {} an agent saved on {when}.",
                            skill.name
                        ),
                    );
                }
                if !skill.why.trim().is_empty() {
                    lines.push(Detail::new(Look::Heading, "Why"));
                    text(&mut lines, skill.why.trim().to_string());
                }
                if let Some((main, rest)) = skill.files.split_first() {
                    lines.push(Detail::new(Look::Heading, "Files"));
                    if let Some(main_text) = &main.text {
                        lines.push(Detail::new(Look::Box, main_text.trim_end()));
                    }
                    for file in std::iter::once(main)
                        .filter(|_| main.text.is_none())
                        .chain(rest)
                    {
                        lines.push(Detail::new(
                            Look::File,
                            format!("{} ({})", file.path, size(file.bytes)),
                        ));
                    }
                }
            }
            Approval::McpServer(server) => {
                text(&mut lines, format!("Runs `{}`.", server.command.trim()));
                if !server.env.is_empty() {
                    let names: Vec<String> =
                        server.env.iter().map(|env| env.name.clone()).collect();
                    let unset: Vec<String> = server
                        .env
                        .iter()
                        .filter(|env| !env.set)
                        .map(|env| env.name.clone())
                        .collect();
                    let mut line = format!("Gets {} from your environment.", and_list(&names));
                    match unset.len() {
                        0 => {}
                        1 => line.push_str(&format!(" {} isn't set.", unset[0])),
                        _ => line.push_str(&format!(" {} aren't set.", and_list(&unset))),
                    }
                    text(&mut lines, line);
                }
                text(
                    &mut lines,
                    if server.workers {
                        "Workers can use it too."
                    } else {
                        "Only the main agent can use it."
                    }
                    .to_string(),
                );
                text(
                    &mut lines,
                    match (server.scope, server.ignored) {
                        (Scope::Project, true) => {
                            "Kept in this project's .arko/mcp.json, which git ignores."
                        }
                        (Scope::Project, false) => "Kept in this project's .arko/mcp.json.",
                        (Scope::User, _) => "Kept in Arko's mcp.json.",
                    }
                    .to_string(),
                );
                if server.shared.is_some() {
                    text(
                        &mut lines,
                        "Open sessions share one copy of it.".to_string(),
                    );
                }
                lines.push(Detail::new(
                    Look::Warning,
                    "It runs with your permissions, outside the command guard.",
                ));
                if let Some(when) = &server.replaces {
                    text(
                        &mut lines,
                        format!(
                            "It replaces the server {} an agent added on {when}.",
                            server.name
                        ),
                    );
                }
                if !server.why.trim().is_empty() {
                    lines.push(Detail::new(Look::Heading, "Why"));
                    text(&mut lines, server.why.trim().to_string());
                }
                if let Some(why) = server
                    .shared
                    .as_deref()
                    .filter(|why| !why.trim().is_empty())
                {
                    lines.push(Detail::new(Look::Heading, "Why shared"));
                    text(&mut lines, why.trim().to_string());
                }
            }
            Approval::WorkerBilling(billing) => {
                if let (Some(input), Some(output)) = (billing.input_price, billing.output_price) {
                    text(
                        &mut lines,
                        format!(
                            "It costs ${} per million input tokens and ${} per million output tokens.",
                            per_million(input),
                            per_million(output)
                        ),
                    );
                }
                match (billing.spent, billing.budget) {
                    (Some(spent), Some(budget)) => lines.push(Detail::new(
                        Look::Warning,
                        format!(
                            "Today’s API spend of {} has reached your daily budget of {}.",
                            money(spent),
                            money(budget)
                        ),
                    )),
                    (Some(spent), None) => {
                        text(
                            &mut lines,
                            format!("Today’s API spend is {}.", money(spent)),
                        );
                    }
                    _ => {}
                }
                let task = billing.task.trim();
                if !task.is_empty() {
                    let end = if task.ends_with(['.', '!', '?', '…']) {
                        ""
                    } else {
                        "."
                    };
                    text(&mut lines, format!("The task is “{task}”{end}"));
                }
            }
        }
        lines
    }
}

fn first_line(text: &str) -> String {
    text.trim().lines().next().unwrap_or_default().to_string()
}

/// "a", "a and b", "a, b and c".
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A count with its thousands marked: `3,210`.
fn thousands(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A file's size as people say it: `512 bytes`, `14 KB`, `1.2 MB`.
fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1_048_576 => format!("{} KB", (bytes as f64 / 1024.0).round() as u64),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

/// Dollars as people write them: `$12`, `$3.20`.
pub fn money(amount: f64) -> String {
    let cents = (amount * 100.0).round();
    if cents % 100.0 == 0.0 {
        format!("${:.0}", cents / 100.0)
    } else {
        format!("${:.2}", cents / 100.0)
    }
}

/// A price per million tokens in dollars, as people write it: `5`,
/// `1.25`, `0.10`, `0.075`.
pub fn per_million(amount: f64) -> String {
    if amount.fract() == 0.0 {
        return format!("{amount:.0}");
    }
    let mut text = format!("{amount:.4}");
    while text.ends_with('0')
        && text
            .split_once('.')
            .is_some_and(|(_, cents)| cents.len() > 2)
    {
        text.pop();
    }
    text
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
    /// The skills and MCP servers it is narrowed to, when it is.
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub mcp: Option<Vec<String>>,
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
    /// The requests in the approval tray, in the order they came.
    #[serde(deserialize_with = "lenient")]
    pub approvals: Vec<ApprovalRequest>,
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
    /// None until the status says. Both safety switches are on by default,
    /// so the page treats anything but `Some(false)` as on.
    pub ask_before_adding_mcp: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    pub review_project_skills: bool,
    pub keep_agents_files_out_of_git: Option<bool>,
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
    /// The model workers start on unless told otherwise, and their effort.
    /// A worker's words name its model and effort only when they differ.
    pub workers: String,
    pub workers_effort: String,
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

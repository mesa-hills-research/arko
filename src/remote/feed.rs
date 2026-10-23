//! The session as its clients show it (the web page, and the terminal's
//! full-screen view): a bounded history of items and the updates that
//! follow it, made from the workflows' events and from what Arko says of
//! the turns (the user's messages, the agent's questions and replies,
//! stops and failures).
//!
//! Every item says whose it is (`agent`: the main agent, or a worker's ID
//! such as `r1`) and in which of the main agent's turns it came (`turn`).
//! A worker's own items are its transcript; the main transcript gets a
//! placeholder (`worker_line`) where a worker started, and a
//! [`WorkerSummary`] per worker says how it is doing. The feed doesn't
//! know about views: each client makes its views from these.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use arko_agents::AgentEvent;
use arko_workflows::WorkflowEvent;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use super::render::{self, Outcome};

/// The `agent` of the main agent's items, and of what Arko says.
pub const MAIN_AGENT: &str = "main";

/// How much of a transcript is kept.
#[derive(Debug, Clone, Copy)]
struct Budget {
    items: usize,
    /// About this many bytes of text.
    bytes: usize,
    /// About this many bytes of commands' whole outputs (an item's
    /// `whole`) and of calls' parameters and results (`parameters`,
    /// `output`), the latest. They don't count in `bytes`, so long outputs
    /// don't push items out. Past it, the oldest commands' blocks keep
    /// their first lines only, and the oldest calls their lines.
    wholes: usize,
}

/// The main transcript kept for clients that connect.
const MAIN_BUDGET: Budget = Budget {
    items: 3000,
    bytes: 2 * 1024 * 1024,
    wholes: 4 * 1024 * 1024,
};
/// The workers' items, kept apart: a busy worker doesn't push the main
/// agent's items out.
const WORKER_BUDGET: Budget = Budget {
    items: 3000,
    bytes: 2 * 1024 * 1024,
    wholes: 4 * 1024 * 1024,
};
/// The latest calls whose items the images their tools return find.
const CALL_ITEMS: usize = 512;
/// The images kept for calls whose items haven't come yet.
const EARLY_IMAGES: usize = 64;
/// The notes the main agent read before their items came that are kept.
const EARLY_READS: usize = 64;
/// Updates a client's stream may fall behind by before it starts over.
const BACKLOG: usize = 2048;
/// A worker's summary goes out at most this often while it works (four
/// times a second); a change of its state goes out at once.
const WORKER_EVERY: Duration = Duration::from_millis(250);

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
    /// [`Update::Change`].
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
    /// worker's [`WorkerSummary`] there.
    WorkerLine,
    /// Where the conversation changes (`/new`, `/resume`), so what follows
    /// is another conversation's. A client that shows one conversation at a
    /// time shows what follows the last of these, and one that keeps the
    /// whole session draws it as a separator.
    Conversation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// Where the user's text was typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Source {
    Terminal,
    Web,
    /// An app in this process.
    App,
}

/// What became of the user's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
    /// ([`REFRESH_MESSAGE`]). An app shows it and its reply as one muted
    /// row.
    Refresh,
}

/// The message that keeps the prompt cache warm where the provider takes
/// no refresh out of band.
pub const REFRESH_MESSAGE: &str = "(Arko sends this message only to keep the prompt cache warm. Reply with \"Acknowledged\" and nothing else.)";

/// How a tool call is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

/// One entry of a transcript.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub id: u64,
    /// When, in Unix milliseconds.
    pub at: i64,
    pub kind: Kind,
    /// Whose it is, [`MAIN_AGENT`] (the main agent, or Arko) or a worker's
    /// ID (`r1`).
    pub agent: String,
    /// The main agent's turn it came in, one more with each turn, or `None`
    /// before the first.
    pub turn: Option<u64>,
    pub text: String,
    /// More, shown folded or under the text, such as a call's whole
    /// parameter or a command's first lines of output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
    /// A call's status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<CallStatus>,
    /// A call's result in a few words ("12 passed", "exit 1"), for the
    /// call's own line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// A command's whole output, when its `detail` shows the first lines.
    /// The terminal's view opens it on a click. The web page doesn't get
    /// it, and its clones share it.
    #[serde(skip)]
    pub whole: Option<Arc<str>>,
    /// The tool a `call` calls (`read_file`), for an app's view of it. The
    /// web page doesn't get it.
    #[serde(skip)]
    pub tool: Option<String>,
    /// A `call`'s parameters, as pretty JSON with its names in order, for
    /// an app's view of it (an edit's diff, a file written). Neither the
    /// web page nor the terminal gets them. The latest are kept.
    #[serde(skip)]
    pub parameters: Option<Arc<str>>,
    /// A `call`'s whole result, once it came (a [`Update::Change`] of the
    /// call), for an app's view of it. A command's output is its block's
    /// `whole` instead. Neither the web page nor the terminal gets it. The
    /// latest are kept.
    #[serde(skip)]
    pub output: Option<Arc<str>>,
    /// For a user's message that started a turn, its index in the main
    /// agent's conversation, where an app can edit it or send it again
    /// (`Request::Edit`, `Request::Regenerate`). The web page doesn't get
    /// it.
    #[serde(skip)]
    pub message_index: Option<usize>,
    /// The images it shows, which the user sent with a message or a call's
    /// tool returned. Their bytes are in the session's
    /// [`Images`](crate::images::Images).
    #[serde(skip)]
    pub images: Vec<crate::images::ImageRef>,
    /// The worker a `worker_line` stands for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    /// For a worker's stop notice, the worker that started the worker. None
    /// for a worker of the main agent. The web page doesn't get it.
    #[serde(skip)]
    pub parent: Option<String>,
    /// Whether it is a worker's stop notice. The web page doesn't get it.
    #[serde(skip)]
    pub stop: bool,
    /// Who sent a worker's `message`, [`MAIN_AGENT`] (its lead) or another
    /// worker's ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The call a `result` is of. Clients show it under that call, which
    /// other items may have followed by the time the result came.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call: Option<u64>,
    /// The ID of the background job a `call` made, when the call moved to
    /// the background or started a job (`terminal_start`). Its
    /// [`JobSummary`] says how the job is doing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// A note of the user's that the main agent hasn't read yet. Clients
    /// show it muted, after the other items of its conversation, because
    /// the agent's output goes on above it. Once the agent reads it, the
    /// item goes ([`Update::Remove`]) and comes again where the agent read
    /// it ([`Update::Item`]), with a new `id` and this unset.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}

impl Item {
    /// An item of the main agent's.
    pub fn new(kind: Kind, text: impl Into<String>) -> Self {
        Self {
            id: 0,
            at: 0,
            kind,
            agent: MAIN_AGENT.to_string(),
            turn: None,
            text: text.into(),
            detail: None,
            level: None,
            source: None,
            delivery: None,
            status: None,
            outcome: None,
            whole: None,
            tool: None,
            parameters: None,
            output: None,
            message_index: None,
            images: Vec::new(),
            worker: None,
            parent: None,
            stop: false,
            from: None,
            call: None,
            job: None,
            pending: false,
        }
    }

    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = agent.into();
        self
    }

    /// Sets the detail, unless it is empty.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        self.detail = (!detail.trim().is_empty()).then_some(detail);
        self
    }

    pub fn level(mut self, level: Level) -> Self {
        self.level = Some(level);
        self
    }

    /// Whether it is the main transcript's (or a worker's).
    pub fn is_main(&self) -> bool {
        self.agent == MAIN_AGENT
    }

    /// Whether it goes beside the agent's output rather than after it. That
    /// is what the user typed while the agent worked (a note it hasn't
    /// read, a command, a line waiting for a turn of its own) and Arko's
    /// output. The agent's text goes on in the item it was in, above it.
    fn is_aside(&self) -> bool {
        match self.kind {
            Kind::Output => true,
            Kind::User => match self.delivery {
                Some(Delivery::Note) => self.pending,
                Some(Delivery::Command | Delivery::Queued) => true,
                _ => false,
            },
            _ => false,
        }
    }

    fn size(&self) -> usize {
        96 + self.text.len()
            + self.detail.as_ref().map_or(0, String::len)
            + self.outcome.as_ref().map_or(0, String::len)
    }

    /// The bytes of its whole output.
    pub fn whole_size(&self) -> usize {
        self.whole.as_ref().map_or(0, |whole| whole.len())
    }

    /// The bytes of what only some clients get: its whole output, a call's
    /// parameters and result.
    fn kept_size(&self) -> usize {
        self.whole_size()
            + [&self.parameters, &self.output]
                .into_iter()
                .map(|part| part.as_ref().map_or(0, |part| part.len()))
                .sum::<usize>()
    }

    /// Drops what only some clients get (its whole output, a call's
    /// parameters and result).
    fn drop_kept(&mut self) {
        self.whole = None;
        self.parameters = None;
        self.output = None;
    }
}

/// The question the main agent waits on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Question {
    pub id: u64,
    pub text: String,
}

/// Whether the main agent works, and what it asks.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Activity {
    pub working: bool,
    pub question: Option<Question>,
    /// When the turn started (Unix milliseconds), while one runs.
    pub since: Option<i64>,
}

/// Where a worker is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

/// A worker, as a line such as
/// `r2 · Fix flaky test · 18 tool calls · 2m · running`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct WorkerSummary {
    /// Its ID, such as `r1` or `r2` (as `/workers` says).
    pub id: String,
    /// Its task's first line, in brief.
    pub title: String,
    pub state: WorkerState,
    /// The calls it made, in all its runs.
    pub tool_calls: u32,
    /// When its latest run started (Unix milliseconds). How long it ran in
    /// all its runs is [`run_time_ms`](Self::run_time_ms).
    pub started_at: Option<i64>,
    /// When its latest run ended.
    pub ended_at: Option<i64>,
    /// What it did last, in a line, which is its latest call or why it
    /// stopped.
    pub last: Option<String>,
    /// Its report, once it finished.
    pub report: Option<String>,
    /// Why it failed, whole, once it did. `last` has the start of it in one
    /// line. The web page doesn't get it.
    #[serde(skip)]
    pub error: Option<String>,
    /// How long its runs that ended ran, in milliseconds. That's the time it
    /// worked (not the time it waited) in the runs that ended or that Arko
    /// stopped as it closed, also before the session was opened again. A
    /// run going on when Arko ended without closing (a crash) isn't
    /// counted.
    pub ran_ms: u64,
    /// While it runs, when its current run started (Unix milliseconds), to
    /// add to `ran_ms` ([`run_time_ms`](Self::run_time_ms)).
    pub running_since: Option<i64>,
    /// The model it runs, as the session names it (such as `gpt-6.1-sol`),
    /// once the session has recorded it.
    pub model: Option<String>,
    /// The reasoning effort it runs at, when its model reasons.
    pub effort: Option<String>,
    /// The profile it started with, if any.
    pub profile: Option<String>,
    /// The worker that started it, such as `r1`. None for a worker the main
    /// agent started.
    pub parent: Option<String>,
    /// How far it is from the main agent, 1 for the main agent's workers.
    pub depth: u32,
    /// Whether it may start workers of its own (it started with `nested`).
    /// The web page doesn't get it.
    #[serde(skip)]
    pub nested: bool,
    /// What its model calls billed, in dollars.
    pub cost_billed: f64,
    /// What its model calls would cost at API list prices. That is the
    /// billed amount, plus the estimate for the calls a plan covered (Claude
    /// Code, the ChatGPT login).
    pub cost_list: f64,
    /// What it and the workers under it (and theirs) billed. The feed adds
    /// it up as the costs change, so a client shows a family's cost without
    /// following [`parent`](Self::parent) itself. A worker with none under it
    /// has its own.
    pub family_cost_billed: f64,
    /// What it and the workers under it would cost at API list prices.
    pub family_cost_list: f64,
}

impl WorkerSummary {
    /// A summary of the worker `id` that waits to run, with nothing known of
    /// it yet. Set the fields it has.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: String::new(),
            state: WorkerState::Queued,
            tool_calls: 0,
            started_at: None,
            ended_at: None,
            last: None,
            report: None,
            error: None,
            ran_ms: 0,
            running_since: None,
            model: None,
            effort: None,
            profile: None,
            parent: None,
            depth: 1,
            nested: false,
            cost_billed: 0.0,
            cost_list: 0.0,
            family_cost_billed: 0.0,
            family_cost_list: 0.0,
        }
    }

    /// How long it has run in all its runs at `now` (Unix milliseconds),
    /// the runs that ended and the current one so far.
    pub fn run_time_ms(&self, now: i64) -> u64 {
        let current = self
            .running_since
            .map_or(0, |since| u64::try_from(now - since).unwrap_or(0));
        self.ran_ms + current
    }

    /// Its current run, if it runs, ends at `now`: its time joins `ran_ms`.
    fn stop_counting(&mut self, now: i64) {
        self.ran_ms = self.run_time_ms(now);
        self.running_since = None;
    }
}

/// The workers in the order a list shows a family: each worker that no
/// worker in `workers` started, then the workers it started, depth first, in
/// the order they started. The result holds the index of each of `workers`,
/// which is given in the order they started. A worker whose parent isn't in
/// `workers` comes among the first.
pub fn family_order(workers: &[WorkerSummary]) -> Vec<usize> {
    family_order_of(
        workers
            .iter()
            .map(|worker| (worker.id.as_str(), worker.parent.as_deref())),
    )
}

/// [`family_order`] with each worker's level among `workers`. A worker whose
/// parent isn't in `workers` is a root at level 1, the workers it started
/// are at level 2, and so on (unlike [`WorkerSummary::depth`], which counts
/// from the main agent).
pub fn family_order_with_levels(workers: &[WorkerSummary]) -> Vec<(usize, u32)> {
    family_levels_of(
        workers
            .iter()
            .map(|worker| (worker.id.as_str(), worker.parent.as_deref())),
    )
}

/// [`family_order`] for workers given as their ID and their parent's.
pub(crate) fn family_order_of<'a>(
    links: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
) -> Vec<usize> {
    family_levels_of(links)
        .into_iter()
        .map(|(at, _)| at)
        .collect()
}

/// [`family_order_with_levels`] for workers given as their ID and their
/// parent's.
fn family_levels_of<'a>(
    links: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
) -> Vec<(usize, u32)> {
    let links: Vec<(&str, Option<&str>)> = links.into_iter().collect();
    let index: HashMap<&str, usize> = links
        .iter()
        .enumerate()
        .map(|(at, (id, _))| (*id, at))
        .collect();
    // Each worker's own workers in the order they started, and the workers
    // no listed worker started.
    let mut under: Vec<Vec<usize>> = vec![Vec::new(); links.len()];
    let mut roots = Vec::new();
    for (at, (_, parent)) in links.iter().enumerate() {
        match parent.and_then(|parent| index.get(parent)) {
            Some(&parent) if parent != at => under[parent].push(at),
            _ => roots.push(at),
        }
    }
    let mut order = Vec::with_capacity(links.len());
    let mut pending: Vec<(usize, u32)> = Vec::new();
    for root in roots {
        pending.push((root, 1));
        while let Some((at, level)) = pending.pop() {
            order.push((at, level));
            // (The earliest of its workers comes out first.)
            pending.extend(under[at].iter().rev().map(|at| (*at, level + 1)));
        }
    }
    // (A loop of parents, which no session makes, would leave some out.)
    if order.len() < links.len() {
        let mut placed = vec![false; links.len()];
        for (at, _) in &order {
            placed[*at] = true;
        }
        order.extend((0..links.len()).filter(|at| !placed[*at]).map(|at| (at, 1)));
    }
    order
}

/// What each of `workers` and the workers under it (and theirs) cost, as
/// (billed, at API list prices), in their order.
fn family_costs<'a>(workers: impl Iterator<Item = &'a WorkerSummary>) -> Vec<(f64, f64)> {
    let workers: Vec<&WorkerSummary> = workers.collect();
    let index: HashMap<&str, usize> = workers
        .iter()
        .enumerate()
        .map(|(at, worker)| (worker.id.as_str(), at))
        .collect();
    let mut totals: Vec<(f64, f64)> = workers
        .iter()
        .map(|worker| (worker.cost_billed, worker.cost_list))
        .collect();
    // In the family's order a worker comes before its workers, so from the
    // end each worker's total is whole when it is added to its parent's.
    let order = family_order_of(
        workers
            .iter()
            .map(|worker| (worker.id.as_str(), worker.parent.as_deref())),
    );
    for at in order.into_iter().rev() {
        let parent = workers[at]
            .parent
            .as_deref()
            .and_then(|parent| index.get(parent).copied())
            .filter(|parent| *parent != at);
        if let Some(parent) = parent {
            totals[parent].0 += totals[at].0;
            totals[parent].1 += totals[at].1;
        }
    }
    totals
}

/// What a worker started with and what it has cost, for its summary
/// ([`Feed::worker_info`]).
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct WorkerInfo {
    /// The model it runs, as the session names it.
    pub model: Option<String>,
    /// Its reasoning effort.
    pub effort: Option<String>,
    /// The profile it started with.
    pub profile: Option<String>,
    /// How far it is from the main agent (0 is taken as 1).
    pub depth: u32,
    /// Whether it may start workers of its own.
    pub nested: bool,
    /// What it has billed and would cost at API list prices, when the
    /// session knows (a resumed session reads them from its saved runs).
    /// Without them, the costs the events add up stay as they are.
    pub costs: Option<(f64, f64)>,
}

impl WorkerInfo {
    /// Nothing known yet, for a worker of the main agent.
    pub fn new() -> Self {
        Self {
            depth: 1,
            ..Self::default()
        }
    }
}

/// What became of a worker, as the workflow says of its runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerChange<'a> {
    Queued,
    /// It started, or goes on again (continued, restarted, recovered).
    Started,
    Finished {
        report: &'a str,
    },
    Failed {
        error: &'a str,
    },
    Stopped {
        reason: &'a str,
    },
}

/// What a background job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct JobSummary {
    /// Its ID, as the terminal tools name it.
    pub id: String,
    /// Whose it is, [`MAIN_AGENT`] or a worker's ID.
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

/// A background job that started, for [`Feed::job_started`]. An app that
/// follows a terminal manager itself, or a test, makes one with
/// [`JobStart::new`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct JobStart {
    /// The job's ID.
    pub id: Uuid,
    /// Whose job it is, as the feed names agents ([`MAIN_AGENT`] or a
    /// worker's ID).
    pub agent: String,
    /// The call that made it, if one did.
    pub tool_call: Option<Uuid>,
    pub kind: JobKind,
    /// What it runs, its command or tool.
    pub label: String,
    /// When it started, in Unix milliseconds.
    pub started_at: i64,
    /// What a monitor watches, and when it ends (Unix milliseconds).
    pub monitor: Option<(String, i64)>,
}

impl JobStart {
    /// `agent`'s job `id`, a `kind` of job that runs `label`, which started
    /// now and no call made.
    pub fn new(
        id: Uuid,
        agent: impl Into<String>,
        kind: JobKind,
        label: impl Into<String>,
    ) -> Self {
        Self {
            id,
            agent: agent.into(),
            tool_call: None,
            kind,
            label: label.into(),
            started_at: now_ms(),
            monitor: None,
        }
    }

    /// The call `call` made the job.
    pub fn made_by(mut self, call: Uuid) -> Self {
        self.tool_call = Some(call);
        self
    }

    /// The job started at `at`, in Unix milliseconds.
    pub fn started(mut self, at: i64) -> Self {
        self.started_at = at;
        self
    }

    /// The job is a monitor that watches `watching` until `expires_at`
    /// (Unix milliseconds).
    pub fn monitoring(mut self, watching: impl Into<String>, expires_at: i64) -> Self {
        self.monitor = Some((watching.into(), expires_at));
        self
    }
}

/// The ended jobs kept for the clients, besides those running.
const ENDED_JOBS: usize = 100;

/// What changed, for the clients connected.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Update {
    /// A new item.
    Item(Item),
    /// More text for the item `id`.
    Append { id: u64, text: String },
    /// The item with this one's `id` is now this one (a call's result came).
    Change(Item),
    /// A worker's summary, whole.
    Worker(WorkerSummary),
    /// The worker `id` ended as `outcome` (finished, failed or stopped).
    /// Its [`Update::Worker`] with that state, its `report` or `error`
    /// comes right before, also in a merged batch.
    WorkerEnded { id: String, outcome: WorkerState },
    /// The activity changed.
    Activity,
    /// The main agent's items from `from` on are gone (the conversation was
    /// cut back to an earlier message), and clients drop them too.
    Truncate { from: u64 },
    /// The item `id` is gone. It held text a model's response streamed
    /// before it failed, which its retry streams again.
    Remove { id: u64 },
    /// The whole summary of a background job that started, changed or
    /// ended.
    Job(JobSummary),
}

/// A batched client's updates, from [`Feed::subscribe_batched`].
pub struct Batches {
    updates: mpsc::UnboundedReceiver<Update>,
}

impl Batches {
    /// The updates since the last batch, after waiting for the first. They
    /// are merged into fewer that leave the client where they all would, so
    /// it applies a burst at once. `None` once the feed is gone.
    pub async fn next(&mut self) -> Option<Vec<Update>> {
        let mut batch = Vec::new();
        if self.updates.recv_many(&mut batch, 1024).await == 0 {
            return None;
        }
        while let Ok(update) = self.updates.try_recv() {
            batch.push(update);
        }
        Some(merged(batch))
    }

    /// The updates since the last batch, merged, if there are any.
    pub fn try_next(&mut self) -> Option<Vec<Update>> {
        let mut batch = Vec::new();
        while let Ok(update) = self.updates.try_recv() {
            batch.push(update);
        }
        (!batch.is_empty()).then(|| merged(batch))
    }
}

/// `updates`, merged into fewer that leave a client where they all would:
/// text appended to an item goes into the item when it came in the same
/// batch (or into the batch's one append to it), an item's change replaces
/// what the batch had of it (it is the whole item), a worker's summary
/// only its last (but the one a worker ended with stays), and the activity
/// is told once.
pub fn merged(updates: Vec<Update>) -> Vec<Update> {
    let mut out: Vec<Option<Update>> = Vec::with_capacity(updates.len());
    // Where the batch has each item: created, changed or appended to.
    let mut whole: HashMap<u64, usize> = HashMap::new();
    let mut appended: HashMap<u64, usize> = HashMap::new();
    let mut workers: HashMap<String, usize> = HashMap::new();
    let mut jobs: HashMap<String, usize> = HashMap::new();
    let mut activity: Option<usize> = None;
    for update in updates {
        match update {
            Update::Item(item) => {
                whole.insert(item.id, out.len());
                out.push(Some(Update::Item(item)));
            }
            Update::Append { id, text } => {
                if let Some(&at) = whole.get(&id)
                    && let Some(Update::Item(item) | Update::Change(item)) = &mut out[at]
                {
                    item.text.push_str(&text);
                } else if let Some(&at) = appended.get(&id)
                    && let Some(Update::Append { text: earlier, .. }) = &mut out[at]
                {
                    earlier.push_str(&text);
                } else {
                    appended.insert(id, out.len());
                    out.push(Some(Update::Append { id, text }));
                }
            }
            Update::Change(item) => {
                // It is the whole item: what the batch appended to it is in it.
                if let Some(at) = appended.remove(&item.id) {
                    out[at] = None;
                }
                match whole.get(&item.id).copied() {
                    Some(at) => {
                        out[at] = Some(match out[at].take() {
                            Some(Update::Item(_)) => Update::Item(item),
                            _ => Update::Change(item),
                        });
                    }
                    None => {
                        whole.insert(item.id, out.len());
                        out.push(Some(Update::Change(item)));
                    }
                }
            }
            Update::Worker(summary) => {
                if let Some(at) = workers.insert(summary.id.clone(), out.len()) {
                    out[at] = None;
                }
                out.push(Some(Update::Worker(summary)));
            }
            Update::WorkerEnded { id, outcome } => {
                // The summary it ended with stays, also if the worker goes
                // on in the same batch.
                workers.remove(&id);
                out.push(Some(Update::WorkerEnded { id, outcome }));
            }
            Update::Job(summary) => {
                if let Some(at) = jobs.insert(summary.id.clone(), out.len()) {
                    out[at] = None;
                }
                out.push(Some(Update::Job(summary)));
            }
            Update::Activity => {
                if let Some(at) = activity.replace(out.len()) {
                    out[at] = None;
                }
                out.push(Some(Update::Activity));
            }
            Update::Truncate { from } => {
                // What the batch had of the main agent's items cut goes with
                // them (a worker's stay: their appends too).
                let cut: std::collections::HashSet<u64> = out
                    .iter()
                    .flatten()
                    .filter_map(|update| match update {
                        Update::Item(item) | Update::Change(item)
                            if item.id >= from && item.is_main() =>
                        {
                            Some(item.id)
                        }
                        _ => None,
                    })
                    .collect();
                for slot in out.iter_mut() {
                    let id = match slot {
                        Some(Update::Item(item) | Update::Change(item)) => item.id,
                        Some(Update::Append { id, .. }) => *id,
                        _ => continue,
                    };
                    if cut.contains(&id) {
                        *slot = None;
                    }
                }
                whole.retain(|id, _| !cut.contains(id));
                appended.retain(|id, _| !cut.contains(id));
                out.push(Some(Update::Truncate { from }));
            }
            Update::Remove { id } => {
                for at in [whole.remove(&id), appended.remove(&id)]
                    .into_iter()
                    .flatten()
                {
                    out[at] = None;
                }
                out.push(Some(Update::Remove { id }));
            }
        }
    }
    out.into_iter().flatten().collect()
}

/// The history, the workers and the activity at one moment.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The main agent's items and the workers', in order (by `id`).
    pub items: Vec<Item>,
    /// The workers, in the order they started.
    pub workers: Vec<WorkerSummary>,
    /// The background jobs running, and the latest that ended, in the order
    /// they started.
    pub jobs: Vec<JobSummary>,
    pub activity: Activity,
}

/// The session's feed. It's cheap to clone, and every clone feeds the same
/// history.
#[derive(Clone)]
pub struct Feed {
    inner: Arc<Inner>,
}

struct Inner {
    /// The main agent's name: its events are the conversation; the others'
    /// are workers'.
    main: String,
    state: Mutex<State>,
    updates: broadcast::Sender<Update>,
    /// The batched clients' queues ([`Feed::subscribe_batched`]).
    batched: Mutex<Vec<mpsc::UnboundedSender<Update>>>,
}

/// A transcript kept within its budget.
#[derive(Default)]
struct Lane {
    items: VecDeque<Item>,
    bytes: usize,
    /// The bytes of the items' whole outputs, calls' parameters and
    /// results ([`Item::kept_size`]).
    wholes: usize,
}

impl Lane {
    fn push(&mut self, item: Item, budget: Budget) {
        self.bytes += item.size();
        self.wholes += item.kept_size();
        self.items.push_back(item);
        while self.items.len() > budget.items || self.bytes > budget.bytes {
            match self.items.pop_front() {
                Some(old) => {
                    self.bytes = self.bytes.saturating_sub(old.size());
                    self.wholes -= old.kept_size();
                }
                None => break,
            }
        }
        self.keep_wholes(budget);
    }

    /// The call `id`'s whole result came: it is kept with the latest.
    fn call_output(&mut self, id: u64, output: Arc<str>, budget: Budget) {
        let Some(item) = self.find(id) else {
            return;
        };
        let before = item.kept_size();
        item.output = Some(output);
        let after = item.kept_size();
        self.wholes = self.wholes - before + after;
        self.keep_wholes(budget);
    }

    /// The items' whole outputs, calls' parameters and results within
    /// the budget: the oldest go first.
    fn keep_wholes(&mut self, budget: Budget) {
        for item in self.items.iter_mut() {
            if self.wholes <= budget.wholes {
                break;
            }
            self.wholes -= item.kept_size();
            item.drop_kept();
        }
    }

    /// Where the item `id` is, while it is kept.
    fn index(&self, id: u64) -> Option<usize> {
        // IDs grow as items are pushed.
        self.items.binary_search_by_key(&id, |item| item.id).ok()
    }

    /// The item `id`, while it is kept.
    fn find(&mut self, id: u64) -> Option<&mut Item> {
        let index = self.index(id)?;
        self.items.get_mut(index)
    }

    /// Takes the item `id` out, while it is kept.
    fn remove(&mut self, id: u64) -> Option<Item> {
        let item = self.items.remove(self.index(id)?)?;
        self.bytes = self.bytes.saturating_sub(item.size());
        self.wholes -= item.kept_size();
        Some(item)
    }

    /// Adds `text` to the item `id`; false when it isn't kept.
    fn append(&mut self, id: u64, text: &str) -> bool {
        let Some(item) = self.find(id) else {
            return false;
        };
        item.text.push_str(text);
        self.bytes += text.len();
        true
    }
}

/// A call waiting for its result.
struct Call {
    item: u64,
    agent: String,
    /// A `message_agent` call's message, for the worker that gets it.
    message: Option<String>,
}

/// Where an agent's output stood as its model request went out.
struct Streamed {
    /// The first item the response may start.
    first: u64,
    /// The item it may add to, and its text's length then.
    open: Option<(u64, usize)>,
    /// The agent's text then ([`State::open_text`]).
    open_text: Option<(Kind, u64)>,
}

struct Worker {
    summary: WorkerSummary,
    /// The turn of its latest placeholder in the main transcript; `None`
    /// until it has one.
    placed: Option<Option<u64>>,
    /// When its summary last went out.
    sent: Option<Instant>,
    /// A change waits to go out.
    pending: bool,
}

#[derive(Default)]
struct State {
    main: Lane,
    workers_lane: Lane,
    next_id: u64,
    /// The item each agent's next chunk continues, if its kind is the
    /// chunk's: its text, or its thinking.
    open_text: HashMap<String, (Kind, u64)>,
    /// The agents' thinking isn't kept (`--hide-thinking`).
    hide_thinking: bool,
    /// The provider's refusal of reasoning summaries was told.
    thinking_refused: bool,
    activity: Activity,
    next_question: u64,
    turn: Option<u64>,
    /// The user's message started the turn `turn`, which hasn't begun yet.
    turn_opened: bool,
    /// Names of the runs that came without a label, by run.
    runs: HashMap<Uuid, String>,
    /// Calls waiting for their results, by call.
    calls: HashMap<Uuid, Call>,
    /// Where each agent's output stood as its latest model request went
    /// out: what its response streams goes if the response fails.
    requests: HashMap<String, Streamed>,
    /// The latest calls' items, by call: the images their tools return
    /// come apart from the events, maybe after the call's result.
    call_items: VecDeque<(Uuid, String, u64)>,
    /// Images that came before their call's item, by call.
    early_images: VecDeque<(Uuid, Vec<crate::images::ImageRef>)>,
    workers: Vec<Worker>,
    /// The worker that started each worker that a worker started, by ID.
    /// Known from the start hook, before the worker's first event.
    parents: HashMap<String, String>,
    /// The user's notes the main agent hasn't read, by their numbers on the
    /// workflow's note board, with their items.
    unread_notes: Vec<(u64, u64)>,
    /// The latest notes the main agent read before their items came, by
    /// number, since an item may come just after.
    early_reads: VecDeque<u64>,
    /// The background jobs, in the order they started.
    jobs: Vec<JobSummary>,
    /// The latest jobs that ended without the feed having seen them run,
    /// with how and when: a moved call's result may name one after.
    unseen_ends: VecDeque<(String, JobState, i64)>,
}

impl State {
    fn lane(&mut self, agent: &str) -> (&mut Lane, Budget) {
        if agent == MAIN_AGENT {
            (&mut self.main, MAIN_BUDGET)
        } else {
            (&mut self.workers_lane, WORKER_BUDGET)
        }
    }

    /// The call lines, by agent and item, that name the job `id`.
    fn lines_naming(&self, id: &str) -> Vec<(String, u64)> {
        [&self.main, &self.workers_lane]
            .into_iter()
            .flat_map(|lane| &lane.items)
            .filter(|item| item.job.as_deref() == Some(id))
            .map(|item| (item.agent.clone(), item.id))
            .collect()
    }

    fn worker(&mut self, id: &str) -> Option<&mut Worker> {
        self.workers
            .iter_mut()
            .find(|worker| worker.summary.id == id)
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// How a monitor's call line reads, from the monitor's summary.
fn monitor_outcome(job: &JobSummary) -> (CallStatus, String) {
    let alerts = match job.alerts.unwrap_or(0) {
        0 => String::new(),
        1 => " · 1 alert".to_string(),
        count => format!(" · {count} alerts"),
    };
    let ran = job.ended_at.map(|at| {
        render::duration_words(Duration::from_millis(
            u64::try_from(at - job.started_at).unwrap_or(0),
        ))
    });
    match job.state {
        JobState::Running => (CallStatus::Ok, format!("{}{alerts}", render::MONITORING)),
        JobState::Expired => (
            CallStatus::Ok,
            match ran {
                Some(ran) => format!("ended after {ran}{alerts}"),
                None => format!("ended{alerts}"),
            },
        ),
        JobState::Finished => (CallStatus::Ok, format!("command exited{alerts}")),
        JobState::Failed => (
            CallStatus::Failed,
            match &job.outcome {
                Some(how) => format!("command exited ({how}){alerts}"),
                None => format!("command exited{alerts}"),
            },
        ),
        JobState::Interrupted => (CallStatus::Failed, "interrupted".to_string()),
        _ => (CallStatus::Ok, format!("stopped{alerts}")),
    }
}

/// A worker's title: its task's first line, in brief.
fn title(task: &str) -> String {
    let line = task
        .lines()
        .map(|line| line.trim().trim_start_matches(['#', '-', '*', ' ']))
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    render::cut(line, 72)
}

impl Feed {
    /// A feed for a workflow whose main agent is `main`.
    pub fn new(main: &str) -> Self {
        let (updates, _) = broadcast::channel(BACKLOG);
        Self {
            inner: Arc::new(Inner {
                main: main.to_string(),
                state: Mutex::new(State::default()),
                updates,
                batched: Mutex::default(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The history, the workers and the activity now, and the updates
    /// from then on.
    pub fn subscribe(&self) -> (Snapshot, broadcast::Receiver<Update>) {
        let state = self.lock();
        (Self::snapshot(&state), self.inner.updates.subscribe())
    }

    /// The history, the workers and the activity now, and from then on
    /// every update, in batches. It's for a client in this process, which
    /// reads them when it can (an app, once per frame). Unlike
    /// [`subscribe`](Self::subscribe), it never misses one, however far
    /// behind it falls.
    pub fn subscribe_batched(&self) -> (Snapshot, Batches) {
        // (Updates are sent with the state locked: none is missed or seen
        // twice.)
        let state = self.lock();
        let (sender, updates) = mpsc::unbounded_channel();
        self.inner
            .batched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(sender);
        (Self::snapshot(&state), Batches { updates })
    }

    fn snapshot(state: &State) -> Snapshot {
        let mut items: Vec<Item> = state
            .main
            .items
            .iter()
            .chain(&state.workers_lane.items)
            .cloned()
            .collect();
        // (Two runs in order: the sort merges them.)
        items.sort_by_key(|item| item.id);
        Snapshot {
            items,
            workers: state.workers.iter().map(|w| w.summary.clone()).collect(),
            jobs: state.jobs.clone(),
            activity: state.activity.clone(),
        }
    }

    pub fn activity(&self) -> Activity {
        self.lock().activity.clone()
    }

    /// How long each worker has run, by its ID, now: to keep with the
    /// session, so that the time goes on from there when it opens again.
    pub(crate) fn worker_run_times(&self) -> Vec<(String, u64)> {
        let now = now_ms();
        self.lock()
            .workers
            .iter()
            .map(|worker| (worker.summary.id.clone(), worker.summary.run_time_ms(now)))
            .collect()
    }

    /// The workers, in the order they started.
    #[cfg(test)]
    pub fn workers(&self) -> Vec<WorkerSummary> {
        self.lock()
            .workers
            .iter()
            .map(|worker| worker.summary.clone())
            .collect()
    }

    fn send(&self, update: Update) {
        let mut batched = self
            .inner
            .batched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !batched.is_empty() {
            batched.retain(|client| client.send(update.clone()).is_ok());
        }
        // No client may be connected.
        let _ = self.inner.updates.send(update);
    }

    fn push(&self, state: &mut State, mut item: Item) -> u64 {
        state.next_id += 1;
        item.id = state.next_id;
        item.at = now_ms();
        item.turn = state.turn;
        match item.kind {
            Kind::Text | Kind::Thinking => {
                state
                    .open_text
                    .insert(item.agent.clone(), (item.kind, item.id));
            }
            // A worker's placeholder doesn't break the main agent's text,
            // nor does what goes beside it, so the next chunks still join it.
            Kind::WorkerLine => {}
            _ if item.is_aside() => {}
            _ => {
                state.open_text.remove(&item.agent);
            }
        }
        let id = item.id;
        let (lane, budget) = state.lane(&item.agent);
        lane.push(item.clone(), budget);
        self.send(Update::Item(item));
        id
    }

    fn add(&self, item: Item) -> u64 {
        let mut state = self.lock();
        self.push(&mut state, item)
    }

    /// An agent's text (or its thinking, `kind`) goes on: in the item it
    /// was in, else in a new one. Thinking and text don't share an item.
    fn append(&self, state: &mut State, agent: &str, kind: Kind, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        if let Some((open, id)) = state.open_text.get(agent).copied()
            && open == kind
            && state.lane(agent).0.append(id, chunk)
        {
            self.send(Update::Append {
                id,
                text: chunk.to_string(),
            });
            return;
        }
        self.push(state, Item::new(kind, chunk).agent(agent));
    }

    /// Whether the agents' thinking is left out (`--hide-thinking`).
    pub fn hide_thinking(&self, hide: bool) {
        self.lock().hide_thinking = hide;
    }

    /// The call `id` ended with `outcome`: its line says so, and a block
    /// follows it when there is one.
    fn settle(&self, state: &mut State, id: &Uuid, outcome: Outcome) {
        let Some(call) = state.calls.remove(id) else {
            return;
        };
        let (lane, budget) = state.lane(&call.agent);
        if let Some(output) = outcome.output {
            lane.call_output(call.item, output, budget);
        }
        if let Some(item) = lane.find(call.item) {
            item.status = Some(outcome.status);
            item.outcome = outcome.text;
            if let Some(job) = outcome.job {
                item.job = Some(job.to_string());
            }
            let item = item.clone();
            self.send(Update::Change(item));
        }
        if let Some(job) = outcome.job
            && let Some(summary) = state
                .jobs
                .iter_mut()
                .find(|summary| summary.id == job.to_string())
            && summary.call.is_none()
        {
            summary.call = Some(call.item);
            let summary = summary.clone();
            self.send(Update::Job(summary));
        }
        // A job that ended before the call's result came (in a parallel
        // group, whose results come together) ends its line now.
        if let Some(job) = outcome.job.map(|job| job.to_string()) {
            let ended = match state.jobs.iter().find(|summary| summary.id == job) {
                Some(summary) => summary
                    .ended_at
                    .map(|at| (summary.state, Some(summary.started_at), at)),
                None => state
                    .unseen_ends
                    .iter()
                    .find(|(id, ..)| *id == job)
                    .map(|(_, ended, at)| (*ended, None, *at)),
            };
            if let Some((ended, started, at)) = ended {
                self.end_moved_call(state, &call.agent, call.item, ended, started, Some(at));
                if started.is_none() {
                    self.end_monitor_line(state, &call.agent, call.item, ended);
                }
            }
            // A monitor's line follows its monitor from its start.
            if let Some(summary) = state
                .jobs
                .iter()
                .find(|summary| summary.id == job && summary.kind == JobKind::Monitor)
                .cloned()
            {
                self.follow_monitor(state, &call.agent, call.item, &summary);
            }
        }
        if let Some(mut block) = outcome.block {
            block.call = Some(call.item);
            self.push(state, block.agent(call.agent));
        }
    }

    /// What `agent`'s failed response streamed since `streamed` goes: the
    /// items it started, and what it added to the one before (the retry
    /// streams it again).
    fn discard(&self, state: &mut State, agent: &str, streamed: Streamed) {
        let (lane, _) = state.lane(agent);
        let gone: Vec<u64> = lane
            .items
            .iter()
            .filter(|item| {
                item.agent == agent
                    && item.id >= streamed.first
                    && matches!(item.kind, Kind::Text | Kind::Thinking)
            })
            .map(|item| item.id)
            .collect();
        if !gone.is_empty() {
            lane.items.retain(|item| !gone.contains(&item.id));
            lane.bytes = lane.items.iter().map(Item::size).sum();
            lane.wholes = lane.items.iter().map(Item::kept_size).sum();
        }
        let shortened = streamed.open.and_then(|(id, length)| {
            let item = lane.find(id)?;
            (item.text.len() > length).then(|| {
                item.text.truncate(length);
                item.clone()
            })
        });
        if let Some(item) = shortened {
            self.send(Update::Change(item));
        }
        for id in gone {
            self.send(Update::Remove { id });
        }
        match streamed.open_text {
            Some(open) => state.open_text.insert(agent.to_string(), open),
            None => state.open_text.remove(agent),
        };
    }

    /// `agent`'s calls still waiting for their results end without them
    /// (its turn or run ended): their lines stop saying they run.
    fn interrupt_calls(&self, state: &mut State, agent: &str) {
        let ids: Vec<Uuid> = state
            .calls
            .iter()
            .filter(|(_, call)| call.agent == agent)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.settle(
                state,
                &id,
                Outcome {
                    status: CallStatus::Failed,
                    text: Some("interrupted".into()),
                    block: None,
                    output: None,
                    job: None,
                },
            );
        }
    }

    fn set_activity(&self, change: impl FnOnce(&mut Activity)) {
        let mut state = self.lock();
        let before = state.activity.clone();
        change(&mut state.activity);
        if state.activity != before {
            self.send(Update::Activity);
        }
    }

    /// A workflow's event. `label` names the worker whose run sent it (the
    /// runs of other agents than the main one are workers too).
    pub fn event(&self, event: &WorkflowEvent, label: Option<&str>) {
        let mut state = self.lock();
        let agent = match label {
            Some(label) => label.to_string(),
            None if event.agent_name == self.inner.main => MAIN_AGENT.to_string(),
            None => {
                let count = state.runs.len() + 1;
                state
                    .runs
                    .entry(event.agent_run_id)
                    .or_insert_with(|| format!("w{count}"))
                    .clone()
            }
        };
        let main = agent == MAIN_AGENT;
        let notice = |text: String, level: Level| {
            Item::new(Kind::Notice, text)
                .level(level)
                .agent(agent.clone())
        };
        match &event.event {
            AgentEvent::AssistantResponseChunk(text) => {
                if !main {
                    self.saw_worker(&mut state, &agent);
                }
                self.append(&mut state, &agent, Kind::Text, text);
            }
            AgentEvent::ReasoningChunk(text) if !state.hide_thinking => {
                if !main {
                    self.saw_worker(&mut state, &agent);
                }
                self.append(&mut state, &agent, Kind::Thinking, text);
            }
            AgentEvent::ToolCallFormatted {
                tool_call_id,
                tool_name,
                parameters,
            } => {
                // The runs the agent has going are what a wait without ids
                // waits for: the workers it started, which are the main
                // agent's for the main agent.
                let running = if tool_name == crate::crew::WAIT_FOR_AGENTS {
                    let starter = (!main).then_some(agent.as_str());
                    crate::ui::sorted_runs(
                        state
                            .workers
                            .iter()
                            .filter(|worker| {
                                worker.summary.state.is_active()
                                    && worker.summary.parent.as_deref() == starter
                            })
                            .map(|worker| worker.summary.id.clone())
                            .collect(),
                    )
                } else {
                    Vec::new()
                };
                let Some(mut item) = render::call(tool_name, parameters, &running) else {
                    // A call the feed doesn't draw (`ask_user`,
                    // `complete_task`) still ends the agent's text: the
                    // next response's text doesn't run on into it.
                    state.open_text.remove(&agent);
                    return;
                };
                if !main {
                    self.saw_worker(&mut state, &agent);
                }
                let call = item.kind == Kind::Call;
                if call {
                    item.status = Some(CallStatus::Running);
                }
                // Images its tool returned before its event came.
                if let Some(at) = state
                    .early_images
                    .iter()
                    .position(|(id, _)| id == tool_call_id)
                    && let Some((_, images)) = state.early_images.remove(at)
                {
                    item.images = images;
                }
                let line = item.text.clone();
                let id = self.push(&mut state, item.agent(agent.clone()));
                state
                    .call_items
                    .push_back((*tool_call_id, agent.clone(), id));
                if state.call_items.len() > CALL_ITEMS {
                    state.call_items.pop_front();
                }
                if call {
                    state.calls.insert(
                        *tool_call_id,
                        Call {
                            item: id,
                            agent: agent.clone(),
                            message: render::message(tool_name, parameters),
                        },
                    );
                }
                if !main && let Some(worker) = state.worker(&agent) {
                    worker.summary.tool_calls += 1;
                    worker.summary.last = Some(render::cut(&line, 100));
                    self.worker_changed(&mut state, &agent, false);
                }
            }
            AgentEvent::ToolResult {
                tool_call_id,
                tool_name,
                output,
            } => {
                let message = state
                    .calls
                    .get_mut(tool_call_id)
                    .and_then(|call| call.message.take());
                self.settle(&mut state, tool_call_id, render::outcome(tool_name, output));
                // The worker that got a message shows it in its own
                // transcript (a message to the lead is its call's line).
                if let (Some(message), Some(to)) = (message, render::sent_to(output)) {
                    let mut item = Item::new(Kind::Message, message).agent(to);
                    item.from = Some(agent.clone());
                    self.push(&mut state, item);
                }
            }
            AgentEvent::ToolRejected {
                tool_call_id,
                reason,
                ..
            } => {
                let why = reason.as_deref().unwrap_or_default().trim();
                // A stop cancels the calls that run ("The task was
                // cancelled while this call was running").
                let message = if why.contains("cancelled") {
                    "stopped".to_string()
                } else if why == arko_agents::claude_code::HELD_FOR_COMPACTION {
                    // The agent makes the call again after the compaction.
                    "held for the compaction".to_string()
                } else if why.is_empty() {
                    "refused".to_string()
                } else {
                    format!("refused ({why})")
                };
                self.settle(&mut state, tool_call_id, Outcome::failed(&message));
            }
            AgentEvent::ErrorEncountered {
                error_message,
                tool_call_id,
                ..
            } => match tool_call_id {
                Some(id) if state.calls.contains_key(id) => {
                    self.settle(&mut state, id, Outcome::failed(error_message));
                }
                _ => {
                    let message = crate::providers::explain_error(error_message);
                    let item = notice(render::cut(&message, 600), Level::Error);
                    self.push(&mut state, item);
                }
            },
            // The conversation's own requests (not Claude Code's subagents').
            AgentEvent::ModelRequestStarted { subagent: None, .. } => {
                let open = state.open_text.get(&agent).copied().and_then(|(_, id)| {
                    let (lane, _) = state.lane(&agent);
                    lane.find(id).map(|item| (id, item.text.len()))
                });
                let streamed = Streamed {
                    first: state.next_id + 1,
                    open,
                    open_text: state.open_text.get(&agent).copied(),
                };
                state.requests.insert(agent.clone(), streamed);
            }
            AgentEvent::StreamedOutputDiscarded { .. } => {
                if let Some(streamed) = state.requests.remove(&agent) {
                    self.discard(&mut state, &agent, streamed);
                }
            }
            // The update names the notes it gave the agent (`note:7`).
            AgentEvent::ContextInjected { ids, .. } if main => {
                let notes = ids
                    .iter()
                    .filter_map(|id| id.strip_prefix("note:")?.parse::<u64>().ok());
                for seq in notes {
                    self.note_read(&mut state, seq);
                }
            }
            AgentEvent::RetryScheduled {
                attempt,
                delay_ms,
                rate_limit,
                ..
            } => {
                let why = if *rate_limit { "rate limited, " } else { "" };
                let text = format!(
                    "{why}retrying in {:.0} s (attempt {attempt})",
                    *delay_ms as f64 / 1000.0
                );
                self.push(&mut state, notice(text, Level::Warn));
            }
            // What a worker's model calls cost.
            AgentEvent::UsageReported(usage) if !main => {
                self.saw_worker(&mut state, &agent);
                let billed = usage.billed();
                let list = crate::providers::at_api_prices(usage, billed);
                if let Some(worker) = state.worker(&agent) {
                    worker.summary.cost_billed += billed;
                    worker.summary.cost_list += list;
                }
                self.worker_changed(&mut state, &agent, false);
                self.add_up_families(&mut state);
            }
            AgentEvent::UsageLimitReached { error_message, .. } => {
                let text = format!("usage limit reached ({})", render::cut(error_message, 300));
                self.push(&mut state, notice(text, Level::Warn));
            }
            AgentEvent::OutputTokenLimitReached { output_tokens, .. } if main => {
                let text =
                    format!("the reply reached the model's output limit ({output_tokens} tokens)");
                self.push(&mut state, notice(text, Level::Warn));
            }
            // Once, in the main transcript: it holds for every agent.
            AgentEvent::ReasoningSummariesRefused { .. } if !state.thinking_refused => {
                state.thinking_refused = true;
                let item = Item::new(Kind::Notice, crate::providers::THINKING_REFUSED);
                self.push(&mut state, item.level(Level::Info));
            }
            AgentEvent::HandoverRequested { input_tokens } if main => {
                let text = crate::handover::requested_text(event.agent_run_id, *input_tokens);
                self.push(&mut state, notice(text, Level::Info));
            }
            AgentEvent::ConversationHandedOver { .. } if main => {
                self.forget_messages(&mut state);
                let text =
                    "handed over, and the agent continues in a fresh conversation from its report";
                self.push(&mut state, notice(text.into(), Level::Info));
            }
            AgentEvent::ConversationCompacted {
                input_tokens,
                compacted_tokens,
            } if main => {
                self.forget_messages(&mut state);
                let text = crate::handover::compacted_text(*input_tokens, *compacted_tokens);
                self.push(&mut state, notice(text, Level::Info));
            }
            AgentEvent::RefusalReceived(refusal) => {
                let text = match &refusal.category {
                    Some(category) => format!("the model refused ({category})"),
                    None => "the model refused".to_string(),
                };
                self.push(&mut state, notice(text, Level::Warn));
            }
            _ => {}
        }
    }

    /// A workflow's events were missed (the printer fell behind).
    pub fn missed(&self, count: u64) {
        self.notice(Level::Warn, &format!("({count} events not shown)"));
    }

    /// The user's text, and what became of it. Text that starts a turn
    /// counts in that turn.
    pub fn user(&self, text: &str, source: Source, delivery: Delivery) -> u64 {
        self.user_with(text, source, delivery, Vec::new())
    }

    /// The user's text with the images it sends, and what became of it.
    pub fn user_with(
        &self,
        text: &str,
        source: Source,
        delivery: Delivery,
        images: Vec<crate::images::ImageRef>,
    ) -> u64 {
        let mut item = Item::new(Kind::User, text.trim());
        item.source = Some(source);
        item.delivery = Some(delivery);
        item.images = images;
        let mut state = self.lock();
        if delivery == Delivery::Turn {
            state.turn = Some(state.turn.map_or(1, |turn| turn + 1));
            state.turn_opened = true;
        }
        self.push(&mut state, item)
    }

    /// The user's text, with the images it sends, that went to the working
    /// main agent as the note `seq` (its number on the workflow's note
    /// board). It waits below the agent's output, pending, until the agent
    /// reads it.
    pub fn note(
        &self,
        text: &str,
        source: Source,
        seq: u64,
        images: Vec<crate::images::ImageRef>,
    ) -> u64 {
        let mut item = Item::new(Kind::User, text.trim());
        item.source = Some(source);
        item.delivery = Some(Delivery::Note);
        item.images = images;
        let mut state = self.lock();
        // (The agent may have read it before it came here.)
        item.pending = match state.early_reads.iter().position(|read| *read == seq) {
            Some(at) => {
                state.early_reads.remove(at);
                false
            }
            None => true,
        };
        let pending = item.pending;
        let id = self.push(&mut state, item);
        if pending {
            state.unread_notes.push((seq, id));
        }
        id
    }

    /// The main agent read the note `seq`.
    fn note_read(&self, state: &mut State, seq: u64) {
        match state.unread_notes.iter().position(|(read, _)| *read == seq) {
            Some(at) => {
                let (_, id) = state.unread_notes.remove(at);
                self.move_read(state, id);
            }
            None => {
                state.early_reads.push_back(seq);
                if state.early_reads.len() > EARLY_READS {
                    state.early_reads.pop_front();
                }
            }
        }
    }

    /// What the user typed that the main agent didn't read (`texts`) went
    /// with a message to it, so those notes are read now.
    pub fn notes_sent(&self, texts: &[String]) {
        let mut state = self.lock();
        for text in texts {
            let lane = &state.main;
            let at = state.unread_notes.iter().position(|(_, id)| {
                lane.index(*id)
                    .is_some_and(|index| lane.items[index].text == text.trim())
            });
            if let Some(at) = at {
                let (_, id) = state.unread_notes.remove(at);
                self.move_read(&mut state, id);
            }
        }
    }

    /// The note `id` was read, so it moves to here, where the agent read
    /// it, as a new item that isn't pending.
    fn move_read(&self, state: &mut State, id: u64) {
        let Some(mut item) = state.main.remove(id) else {
            return;
        };
        self.send(Update::Remove { id });
        item.pending = false;
        self.push(state, item);
    }

    /// The main transcript's item `id`, while it is kept.
    pub fn main_item(&self, id: u64) -> Option<Item> {
        self.lock().main.find(id).map(|item| item.clone())
    }

    /// The user's latest message that started a turn, while it is kept.
    pub fn last_turn_message(&self) -> Option<Item> {
        let state = self.lock();
        let item = state
            .main
            .items
            .iter()
            .rev()
            .find(|item| item.kind == Kind::User && item.delivery == Some(Delivery::Turn));
        item.cloned()
    }

    /// The feed item of the call `call` that is still running, for a client
    /// that offers to move it to the background.
    pub(crate) fn running_call_item(&self, call: Uuid) -> Option<u64> {
        self.lock().calls.get(&call).map(|call| call.item)
    }

    /// The jobs the main transcript's calls moved to, whose lines still say
    /// they are in the background: after a restore, those of calls an
    /// earlier Arko moved.
    pub(crate) fn moved_jobs(&self) -> Vec<Uuid> {
        self.lock()
            .main
            .items
            .iter()
            .filter(|item| {
                item.outcome.as_deref().is_some_and(|outcome| {
                    outcome.starts_with(render::MOVED_OUTCOME)
                        || outcome.starts_with(render::MONITORING)
                })
            })
            .filter_map(|item| item.job.as_deref()?.parse().ok())
            .collect()
    }

    /// The job `id` ended as `ended` some time ago (with an earlier Arko):
    /// the lines of the calls that moved there, or started it as a
    /// monitor, say how.
    pub(crate) fn job_ended_earlier(&self, id: Uuid, ended: JobState) {
        let mut state = self.lock();
        let id = id.to_string();
        for (agent, item) in state.lines_naming(&id) {
            self.end_moved_call(&mut state, &agent, item, ended, None, None);
            self.end_monitor_line(&mut state, &agent, item, ended);
        }
    }

    /// The call `item` of `agent`, if its line says it is monitoring, says
    /// its monitor ended as `ended`, for a monitor whose summary the feed
    /// doesn't have (it ended with an earlier Arko, or before the feed saw
    /// it run).
    fn end_monitor_line(&self, state: &mut State, agent: &str, item: u64, ended: JobState) {
        let (lane, _) = state.lane(agent);
        let Some(call) = lane.find(item) else {
            return;
        };
        if !call
            .outcome
            .as_deref()
            .is_some_and(|outcome| outcome.starts_with(render::MONITORING))
        {
            return;
        }
        let (status, text) = match ended {
            JobState::Interrupted => (CallStatus::Failed, "interrupted"),
            JobState::Failed => (CallStatus::Failed, "command exited"),
            JobState::Finished => (CallStatus::Ok, "command exited"),
            JobState::Stopped => (CallStatus::Ok, "stopped"),
            _ => (CallStatus::Ok, "ended"),
        };
        call.status = Some(status);
        call.outcome = Some(text.to_string());
        let call = call.clone();
        self.send(Update::Change(call));
    }

    /// What the transcript shows of the call `call` (one of the latest),
    /// such as `slow_lookup (slow) {"word":"mesa"}`.
    pub(crate) fn call_text(&self, call: Uuid) -> Option<String> {
        let mut state = self.lock();
        let (_, agent, item) = state
            .call_items
            .iter()
            .find(|(id, ..)| *id == call)
            .cloned()?;
        let (lane, _) = state.lane(&agent);
        lane.find(item).map(|item| item.text.clone())
    }

    /// The call item `item` of a call still running: its call's ID.
    pub(crate) fn running_call(&self, item: u64) -> Option<Uuid> {
        self.lock()
            .calls
            .iter()
            .find(|(_, call)| call.item == item)
            .map(|(id, _)| *id)
    }

    /// A background job started. Its summary goes out, and the line of the
    /// call that made it names it. A job the feed knows already is left
    /// alone. Arko calls this as its terminal manager's jobs start, and an
    /// app that follows the manager itself (or a test) can too.
    ///
    /// ```
    /// use arko::remote::{Feed, JobKind, JobStart, JobState, MAIN_AGENT, Update};
    ///
    /// let feed = Feed::new("main");
    /// let (_, mut updates) = feed.subscribe();
    /// let job = uuid::Uuid::new_v4();
    /// feed.job_started(JobStart::new(job, MAIN_AGENT, JobKind::Command, "cargo build"));
    /// feed.job_ended(job, JobState::Failed, Some("exit 1".into()));
    /// let Ok(Update::Job(summary)) = updates.try_recv() else { panic!() };
    /// assert_eq!(summary.state, JobState::Running);
    /// let Ok(Update::Job(summary)) = updates.try_recv() else { panic!() };
    /// assert_eq!((summary.state, summary.outcome.as_deref()), (JobState::Failed, Some("exit 1")));
    /// ```
    pub fn job_started(&self, start: JobStart) {
        let mut state = self.lock();
        let id = start.id.to_string();
        if state.jobs.iter().any(|job| job.id == id) {
            return;
        }
        let item = start.tool_call.and_then(|call| {
            state
                .call_items
                .iter()
                .find(|(id, ..)| *id == call)
                .map(|(_, agent, item)| (agent.clone(), *item))
        });
        let mut call = None;
        if let Some((agent, item)) = item {
            let (lane, _) = state.lane(&agent);
            if let Some(found) = lane.find(item) {
                call = Some(item);
                if found.job.as_deref() != Some(&id) {
                    found.job = Some(id.clone());
                    let found = found.clone();
                    self.send(Update::Change(found));
                }
            }
        }
        let (watching, expires_at) = start.monitor.unzip();
        let summary = JobSummary {
            id,
            agent: start.agent,
            call,
            kind: start.kind,
            label: start.label,
            state: JobState::Running,
            started_at: start.started_at,
            ended_at: None,
            outcome: None,
            alerts: watching.is_some().then_some(0),
            watching,
            expires_at,
        };
        state.jobs.push(summary.clone());
        let ended = state
            .jobs
            .iter()
            .filter(|job| !job.state.is_running())
            .count();
        if ended > ENDED_JOBS
            && let Some(oldest) = state.jobs.iter().position(|job| !job.state.is_running())
        {
            state.jobs.remove(oldest);
        }
        self.send(Update::Job(summary));
    }

    /// The background job `id` ended as `ended`, with `outcome` in a few
    /// words ("exit 1", say). Its summary goes out, and the line of a call
    /// that moved there says how it ended. The end of a job the feed hasn't
    /// seen start goes to the line of the call that names it, now or when
    /// its result comes.
    pub fn job_ended(&self, id: Uuid, ended: JobState, outcome: Option<String>) {
        let mut state = self.lock();
        let id = id.to_string();
        let now = now_ms();
        let Some(job) = state.jobs.iter_mut().find(|job| job.id == id) else {
            // One that ended before the feed saw it run: the line of the
            // call that moved there, now or when its result comes.
            for (agent, item) in state.lines_naming(&id) {
                self.end_moved_call(&mut state, &agent, item, ended, None, Some(now));
                self.end_monitor_line(&mut state, &agent, item, ended);
            }
            state.unseen_ends.push_back((id, ended, now));
            if state.unseen_ends.len() > ENDED_JOBS {
                state.unseen_ends.pop_front();
            }
            return;
        };
        if !job.state.is_running() {
            return;
        }
        job.state = ended;
        job.ended_at = Some(now);
        job.outcome = outcome;
        let job = job.clone();
        self.send(Update::Job(job.clone()));
        if job.kind == JobKind::Monitor {
            let mut lines = state.lines_naming(&id);
            if let Some(item) = job.call {
                lines.push((job.agent.clone(), item));
            }
            for (agent, item) in lines {
                self.follow_monitor(&mut state, &agent, item, &job);
            }
            return;
        }
        // A moved call's line says how it ended, and so does a restored
        // line of it. A command terminal_start started has its own result
        // already.
        let mut lines = state.lines_naming(&id);
        if let Some(item) = job.call {
            lines.push((job.agent.clone(), item));
        }
        for (agent, item) in lines {
            self.end_moved_call(
                &mut state,
                &agent,
                item,
                ended,
                Some(job.started_at),
                Some(now),
            );
        }
    }

    /// The monitor `id` alerted `alerts` times so far and ends at
    /// `expires_at` (Unix milliseconds). Its summary goes out, and the line
    /// of the call that started it says so.
    pub fn job_changed(&self, id: Uuid, alerts: u32, expires_at: i64) {
        let mut state = self.lock();
        let id = id.to_string();
        let Some(job) = state.jobs.iter_mut().find(|job| job.id == id) else {
            return;
        };
        if !job.state.is_running()
            || (job.alerts == Some(alerts) && job.expires_at == Some(expires_at))
        {
            return;
        }
        job.alerts = Some(alerts);
        job.expires_at = Some(expires_at);
        let job = job.clone();
        self.send(Update::Job(job.clone()));
        let mut lines = state.lines_naming(&id);
        if let Some(item) = job.call {
            lines.push((job.agent.clone(), item));
        }
        for (agent, item) in lines {
            self.follow_monitor(&mut state, &agent, item, &job);
        }
    }

    /// The call `item` of `agent`, if its line says it started the monitor
    /// `job` and is monitoring, says how the monitor is doing: "monitoring ·
    /// 3 alerts", "ended after 30 min · 4 alerts", "stopped", "command exited
    /// (exit 1)".
    fn follow_monitor(&self, state: &mut State, agent: &str, item: u64, job: &JobSummary) {
        let (lane, _) = state.lane(agent);
        let Some(call) = lane.find(item) else {
            return;
        };
        if !call
            .outcome
            .as_deref()
            .is_some_and(|outcome| outcome.starts_with(render::MONITORING))
        {
            return;
        }
        let (status, text) = monitor_outcome(job);
        if call.outcome.as_deref() == Some(text.as_str()) && call.status == Some(status) {
            return;
        }
        call.status = Some(status);
        call.outcome = Some(text);
        let call = call.clone();
        self.send(Update::Change(call));
    }

    /// The call `item` of `agent`, if its line says it moved to the
    /// background, says how its job ended: as `ended`, at `now`, having
    /// started at `started` (else with the call). With `now` unknown (a
    /// job that ended with an earlier Arko), the line says how only.
    fn end_moved_call(
        &self,
        state: &mut State,
        agent: &str,
        item: u64,
        ended: JobState,
        started: Option<i64>,
        now: Option<i64>,
    ) {
        let (lane, _) = state.lane(agent);
        let Some(call) = lane.find(item) else {
            return;
        };
        if !call
            .outcome
            .as_deref()
            .is_some_and(|outcome| outcome.starts_with(render::MOVED_OUTCOME))
        {
            return;
        }
        let ran = now.map(|now| {
            render::duration_words(Duration::from_millis(
                u64::try_from(now - started.unwrap_or(call.at)).unwrap_or(0),
            ))
        });
        let (status, text) = match ended {
            JobState::Finished => (
                CallStatus::Ok,
                match ran {
                    Some(ran) => format!("finished in the background · {ran}"),
                    None => "finished in the background".to_string(),
                },
            ),
            JobState::Failed => (CallStatus::Failed, "failed in the background".to_string()),
            JobState::Interrupted => (CallStatus::Failed, "interrupted".to_string()),
            _ => (CallStatus::Failed, "stopped".to_string()),
        };
        call.status = Some(status);
        call.outcome = Some(text);
        let call = call.clone();
        self.send(Update::Change(call));
    }

    /// The call `call`'s tool returned `images`: its item shows them (the
    /// clients get the change), or will once it comes.
    pub(crate) fn call_images(&self, call: Uuid, images: Vec<crate::images::ImageRef>) {
        let mut state = self.lock();
        let Some((_, agent, id)) = state
            .call_items
            .iter()
            .find(|(id, ..)| *id == call)
            .cloned()
        else {
            state.early_images.push_back((call, images));
            if state.early_images.len() > EARLY_IMAGES {
                state.early_images.pop_front();
            }
            return;
        };
        let (lane, _) = state.lane(&agent);
        if let Some(item) = lane.find(id) {
            item.images.extend(images);
            let item = item.clone();
            self.send(Update::Change(item));
        }
    }

    /// The user's message `id` is the main agent's message `index`: it can
    /// be edited (the clients get the change).
    pub(crate) fn mark_message(&self, id: u64, index: usize) {
        let mut state = self.lock();
        if let Some(item) = state.main.find(id) {
            item.message_index = Some(index);
            let item = item.clone();
            self.send(Update::Change(item));
        }
    }

    /// The user's message to `worker`, which went to it directly. It is
    /// the worker's item, so only the worker's own transcript shows it.
    pub fn to_worker(&self, worker: &str, text: &str, source: Source) {
        let mut item = Item::new(Kind::User, text.trim()).agent(worker);
        item.source = Some(source);
        self.add(item);
    }

    /// Items from before Arko started (a resumed session's conversation,
    /// from [`history`](super::history)), numbered from 1, with their turns
    /// from 1. They join the history as they are, numbered after the feed's
    /// own items and turns (a result keeps its call), and the next turn
    /// comes after theirs.
    pub fn restore(&self, items: Vec<Item>) {
        let mut state = self.lock();
        let base = state.next_id;
        let turns = state.turn.unwrap_or(0);
        let at = now_ms();
        for mut item in items {
            item.id += base;
            item.call = item.call.map(|call| call + base);
            item.turn = item.turn.map(|turn| turn + turns);
            item.at = at;
            state.next_id = state.next_id.max(item.id);
            state.turn = state.turn.max(item.turn);
            let (lane, budget) = state.lane(&item.agent);
            lane.push(item.clone(), budget);
            self.send(Update::Item(item));
        }
    }

    /// The conversation changes here, and `text` says to what ("New
    /// conversation", "Conversation 2 · ...").
    pub fn conversation(&self, text: &str) {
        let mut state = self.lock();
        self.forget_messages(&mut state);
        self.push(&mut state, Item::new(Kind::Conversation, text));
    }

    /// The main agent's conversation is no longer the one the user's
    /// messages so far are in (it was handed over or compacted, or the
    /// session went to another conversation): they can't be edited, and
    /// the clients get the change.
    fn forget_messages(&self, state: &mut State) {
        for item in state.main.items.iter_mut() {
            if item.message_index.take().is_some() {
                self.send(Update::Change(item.clone()));
            }
        }
    }

    /// Drops the main agent's items from `from` on (an edited message and
    /// what followed it), and tells the clients.
    pub fn truncate(&self, from: u64) {
        let mut state = self.lock();
        let lane = &mut state.main;
        lane.items.retain(|item| item.id < from);
        lane.bytes = lane.items.iter().map(Item::size).sum();
        lane.wholes = lane.items.iter().map(Item::kept_size).sum();
        state.open_text.remove(MAIN_AGENT);
        state.unread_notes.retain(|(_, id)| *id < from);
        self.send(Update::Truncate { from });
    }

    /// What Arko says about the session.
    pub fn notice(&self, level: Level, text: &str) {
        let text = text.trim();
        if !text.is_empty() {
            self.add(Item::new(Kind::Notice, text).level(level));
        }
    }

    /// A command's output.
    pub fn output(&self, text: &str) {
        self.output_at(text, None);
    }

    /// A command's output at `level`. A warning (or an error) keeps its
    /// colour in the clients.
    pub fn output_at(&self, text: &str, level: Option<Level>) {
        if !text.trim().is_empty() {
            let mut item = Item::new(Kind::Output, text.trim_end());
            item.level = level;
            self.add(item);
        }
    }

    /// The main agent's reply. A reply in plain text has streamed as the
    /// agent's text already, so that item becomes the reply (an
    /// [`Update::Change`]) and it shows once.
    pub fn reply(&self, text: &str) {
        let text = text.trim();
        let mut state = self.lock();
        if let Some(id) = Self::streamed_reply(&mut state, text)
            && let Some(item) = state.main.find(id)
        {
            let before = item.size();
            item.kind = Kind::Reply;
            item.text = text.to_string();
            let item = item.clone();
            state.main.bytes = (state.main.bytes + item.size()).saturating_sub(before);
            state.open_text.remove(MAIN_AGENT);
            self.send(Update::Change(item));
            return;
        }
        self.push(&mut state, Item::new(Kind::Reply, text));
    }

    /// The main agent's text item of its last response when it is `reply`:
    /// the latest of its items but for notices (an output limit reached,
    /// say), the workers' places and what went beside its text.
    fn streamed_reply(state: &mut State, reply: &str) -> Option<u64> {
        let last = state.main.items.iter().rev().find(|item| {
            !matches!(item.kind, Kind::Notice | Kind::WorkerLine | Kind::Thinking)
                && !item.is_aside()
        })?;
        (last.kind == Kind::Text && last.is_main() && last.text.trim() == reply).then_some(last.id)
    }

    /// A turn of the main agent begins, and the items from here on are its.
    pub fn turn_started(&self) {
        {
            let mut state = self.lock();
            if state.turn_opened {
                state.turn_opened = false;
            } else {
                state.turn = Some(state.turn.map_or(1, |turn| turn + 1));
            }
        }
        self.set_activity(|activity| {
            activity.working = true;
            activity.since = Some(now_ms());
        });
    }

    /// The turn ended, and `summary` is what it took (time, cost, context).
    pub fn turn_ended(&self, summary: &str, plan_usage: Option<&str>) {
        {
            let mut state = self.lock();
            state.turn_opened = false;
            self.interrupt_calls(&mut state, MAIN_AGENT);
            // The plan's usage is the detail, so the page can keep it whole
            // on a line of its own.
            let mut item = Item::new(Kind::Turn, summary);
            item.detail = plan_usage.map(str::to_string);
            self.push(&mut state, item);
        }
        self.set_activity(|activity| {
            activity.working = false;
            activity.since = None;
            activity.question = None;
        });
    }

    /// The main agent asks `text`. Returns the question's ID, which an
    /// answer from the page names.
    pub fn question(&self, text: &str) -> u64 {
        let id = {
            let mut state = self.lock();
            state.next_question += 1;
            let id = state.next_question;
            self.push(&mut state, Item::new(Kind::Question, text.trim()));
            id
        };
        self.set_activity(|activity| {
            activity.question = Some(Question {
                id,
                text: text.trim().to_string(),
            });
        });
        id
    }

    /// The question `id` was answered with `text`.
    pub fn answered(&self, id: u64, text: &str, source: Source) {
        self.user(text, source, Delivery::Answer);
        self.question_closed(id);
    }

    /// The question `id` is no longer asked.
    pub fn question_closed(&self, id: u64) {
        self.set_activity(|activity| {
            if activity.question.as_ref().is_some_and(|q| q.id == id) {
                activity.question = None;
            }
        });
    }

    // ----- workers -----

    /// The worker `id` with `task` changed (from the workflow's subagent
    /// events). Its summary follows, the main transcript gets its
    /// placeholder when it starts in a new turn, and its own transcript
    /// ends with its report, error or stop.
    pub fn worker(&self, id: &str, task: &str, change: WorkerChange<'_>) {
        let mut state = self.lock();
        self.known_worker(&mut state, id);
        let now = now_ms();
        let parent = state.parents.get(id).cloned();
        let Some(worker) = state.worker(id) else {
            return;
        };
        if worker.summary.title.is_empty() && !task.trim().is_empty() {
            worker.summary.title = title(task);
        }
        let summary = &mut worker.summary;
        let mut ended = None;
        match change {
            WorkerChange::Queued => {
                summary.stop_counting(now);
                summary.state = WorkerState::Queued;
                summary.started_at = None;
                summary.ended_at = None;
                summary.error = None;
            }
            WorkerChange::Started => {
                summary.stop_counting(now);
                summary.state = WorkerState::Running;
                summary.started_at = Some(now);
                summary.running_since = Some(now);
                summary.ended_at = None;
                summary.report = None;
                summary.error = None;
                // A worker that goes on after a stop isn't doing what it did
                // when it stopped.
                if summary
                    .last
                    .as_deref()
                    .is_some_and(|last| last.starts_with("stopped"))
                {
                    summary.last = None;
                }
            }
            WorkerChange::Finished { report } => {
                summary.stop_counting(now);
                summary.state = WorkerState::Finished;
                summary.ended_at = Some(now);
                let report = report.trim();
                summary.report = (!report.is_empty()).then(|| report.to_string());
                ended = summary
                    .report
                    .clone()
                    .map(|report| Item::new(Kind::Reply, report));
            }
            WorkerChange::Failed { error } => {
                summary.stop_counting(now);
                summary.state = WorkerState::Failed;
                summary.ended_at = Some(now);
                summary.last = Some(render::cut(error, 160));
                summary.error = Some(error.to_string());
                let error = crate::providers::explain_error(error);
                ended = Some(
                    Item::new(
                        Kind::Notice,
                        format!("failed ({})", render::cut(&error, 600)),
                    )
                    .level(Level::Error),
                );
            }
            WorkerChange::Stopped { reason } => {
                summary.stop_counting(now);
                summary.state = WorkerState::Stopped;
                summary.ended_at = Some(now);
                let reason = reason.trim();
                summary.last = Some(if reason.is_empty() {
                    "stopped".to_string()
                } else {
                    render::cut(&format!("stopped ({reason})"), 160)
                });
                let mut notice = Item::new(
                    Kind::Notice,
                    if reason.is_empty() {
                        "stopped".to_string()
                    } else {
                        format!("stopped ({})", render::cut(reason, 300))
                    },
                )
                .level(Level::Info);
                notice.parent = parent;
                notice.stop = true;
                ended = Some(notice);
            }
        }
        if matches!(change, WorkerChange::Queued | WorkerChange::Started) {
            self.place(&mut state, id);
        } else {
            self.interrupt_calls(&mut state, id);
        }
        if let Some(item) = ended {
            self.push(&mut state, item.agent(id));
        }
        self.worker_changed(&mut state, id, true);
        if let Some(worker) = state.worker(id)
            && !worker.summary.state.is_active()
        {
            self.send(Update::WorkerEnded {
                id: id.to_string(),
                outcome: worker.summary.state,
            });
        }
    }

    /// A worker of the session from before Arko started (a resumed
    /// session). Its summary goes in without a place in the transcript,
    /// with how long it ran before (`ran_ms`). A run it goes on with counts
    /// from when it starts again.
    pub fn worker_restored(
        &self,
        id: &str,
        task: &str,
        was: WorkerState,
        report: Option<&str>,
        since: i64,
        ran_ms: u64,
    ) {
        let mut state = self.lock();
        if state.worker(id).is_some() {
            return;
        }
        let summary = WorkerSummary {
            title: title(task),
            state: was,
            started_at: was.is_active().then_some(since),
            ended_at: (!was.is_active()).then_some(since),
            report: report
                .map(str::trim)
                .filter(|report| !report.is_empty())
                .map(str::to_string),
            ran_ms,
            ..WorkerSummary::new(id)
        };
        state.workers.push(Worker {
            summary,
            placed: None,
            sent: None,
            pending: false,
        });
        self.worker_changed(&mut state, id, true);
    }

    /// The worker `id` is started by the worker `parent`. This is how the feed
    /// learns a worker's parent, so call it before the worker's first event
    /// and before its [`worker_info`](Self::worker_info). The worker's place
    /// in the transcript is its parent's own (the main transcript holds the
    /// lines of the main agent's workers only), and its summary gets `parent`
    /// with the next `worker_info`.
    pub fn worker_under(&self, id: &str, parent: &str) {
        self.lock()
            .parents
            .insert(id.to_string(), parent.to_string());
    }

    /// Tells the feed what the worker `id` started with (its model, effort,
    /// profile, depth and whether it may start workers) and, when the session
    /// knows, what it has cost. The summary goes out when something changed,
    /// and so do those of the workers it started and the ones that started it
    /// when their family's cost changed. The worker's parent is the one
    /// [`worker_under`](Self::worker_under) named.
    pub fn worker_info(&self, id: &str, info: WorkerInfo) {
        let mut state = self.lock();
        self.known_worker(&mut state, id);
        let parent = state.parents.get(id).cloned();
        let Some(worker) = state.worker(id) else {
            return;
        };
        let summary = &mut worker.summary;
        let before = summary.clone();
        summary.model = info.model;
        summary.effort = info.effort;
        summary.profile = info.profile;
        summary.parent = parent;
        summary.depth = info.depth.max(1);
        summary.nested = info.nested;
        if let Some((billed, list)) = info.costs {
            summary.cost_billed = billed;
            summary.cost_list = list;
        }
        if *summary != before {
            self.worker_changed(&mut state, id, false);
        }
        self.add_up_families(&mut state);
    }

    /// Adds up what each worker and the workers under it cost, and sends
    /// the summaries whose totals changed.
    fn add_up_families(&self, state: &mut State) {
        let costs = family_costs(state.workers.iter().map(|worker| &worker.summary));
        let mut changed = Vec::new();
        for (worker, (billed, list)) in state.workers.iter_mut().zip(costs) {
            let summary = &mut worker.summary;
            if (summary.family_cost_billed - billed).abs() > 1e-12
                || (summary.family_cost_list - list).abs() > 1e-12
            {
                summary.family_cost_billed = billed;
                summary.family_cost_list = list;
                changed.push(summary.id.clone());
            }
        }
        for id in changed {
            self.worker_changed(state, &id, false);
        }
    }

    /// The summaries of all the workers, as the clients have them.
    pub fn worker_summaries(&self) -> Vec<WorkerSummary> {
        self.lock()
            .workers
            .iter()
            .map(|worker| worker.summary.clone())
            .collect()
    }

    /// The summary of the worker `id`, as the clients have it.
    pub fn worker_summary(&self, id: &str) -> Option<WorkerSummary> {
        self.lock().worker(id).map(|worker| worker.summary.clone())
    }

    /// Makes sure the worker `id` has a summary; true when it is new.
    fn known_worker(&self, state: &mut State, id: &str) -> bool {
        if state.worker(id).is_some() {
            return false;
        }
        state.workers.push(Worker {
            summary: WorkerSummary {
                state: WorkerState::Running,
                started_at: Some(now_ms()),
                running_since: Some(now_ms()),
                ..WorkerSummary::new(id)
            },
            placed: None,
            sent: None,
            pending: false,
        });
        true
    }

    /// A worker's event came: a worker not heard of yet runs, and has its
    /// place in the main transcript. (The workflow's subagent events and
    /// the workers' own events come by different ways, in no fixed order.)
    fn saw_worker(&self, state: &mut State, id: &str) {
        if self.known_worker(state, id) {
            self.place(state, id);
            self.worker_changed(state, id, true);
        }
    }

    /// Puts the worker's placeholder in the main transcript, unless it has
    /// one in this turn already.
    fn place(&self, state: &mut State, id: &str) {
        let turn = state.turn;
        let Some(worker) = state.worker(id) else {
            return;
        };
        if worker.placed == Some(turn) {
            return;
        }
        worker.placed = Some(turn);
        let text = if worker.summary.title.is_empty() {
            id.to_string()
        } else {
            worker.summary.title.clone()
        };
        let parent = state.parents.get(id).cloned();
        let mut item = Item::new(Kind::WorkerLine, text);
        item.worker = Some(id.to_string());
        // A worker's worker is placed in its parent's transcript, where the
        // parent started it.
        if let Some(parent) = parent {
            item.agent = parent;
        }
        self.push(state, item);
    }

    /// The worker's summary changed: it goes out now when `urgent` (its
    /// state changed) or when the last one went out long enough ago, else
    /// a little later.
    fn worker_changed(&self, state: &mut State, id: &str, urgent: bool) {
        let now = Instant::now();
        let Some(worker) = state.worker(id) else {
            return;
        };
        let due = worker.sent.is_none_or(|sent| now >= sent + WORKER_EVERY);
        if urgent || due {
            worker.sent = Some(now);
            worker.pending = false;
            let summary = worker.summary.clone();
            self.send(Update::Worker(summary));
            return;
        }
        if worker.pending {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // No timer to send it later with: it goes now.
            worker.sent = Some(now);
            let summary = worker.summary.clone();
            self.send(Update::Worker(summary));
            return;
        };
        worker.pending = true;
        let at = worker.sent.map_or(now, |sent| sent + WORKER_EVERY);
        let feed: Weak<Inner> = Arc::downgrade(&self.inner);
        let id = id.to_string();
        runtime.spawn(async move {
            tokio::time::sleep_until(at.into()).await;
            if let Some(inner) = feed.upgrade() {
                Feed { inner }.send_pending(&id);
            }
        });
    }

    /// Sends the worker's summary if a change of it waits.
    fn send_pending(&self, id: &str) {
        let mut state = self.lock();
        if let Some(worker) = state.worker(id)
            && worker.pending
        {
            worker.pending = false;
            worker.sent = Some(Instant::now());
            let summary = worker.summary.clone();
            self.send(Update::Worker(summary));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cut drops the main agent's items from where it is, for the
    /// history and the clients. A worker's stay, and a batch forgets what
    /// it had of the items cut.
    #[test]
    fn a_cut_drops_the_main_agents_items_from_there() {
        let feed = Feed::new("main");
        let (_, mut batches) = feed.subscribe_batched();
        let kept = feed.add(Item::new(Kind::User, "first").agent("main"));
        let cut = feed.add(Item::new(Kind::User, "second").agent("main"));
        let worker = feed.add(Item::new(Kind::Text, "working").agent("r1"));
        feed.add(Item::new(Kind::Text, "the reply").agent("main"));
        feed.conversation("New conversation");
        feed.truncate(cut);
        let ids: Vec<u64> = feed
            .subscribe()
            .0
            .items
            .iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(ids, [kept, worker]);
        let batch = batches.try_next().unwrap();
        let ids: Vec<Option<u64>> = batch
            .iter()
            .map(|update| match update {
                Update::Item(item) => Some(item.id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [Some(kept), Some(worker), None]);
        assert!(matches!(batch.last(), Some(Update::Truncate { from }) if *from == cut));
    }

    /// A cut in a batch drops what the batch had of the main agent's items
    /// from there, and keeps a worker's text added in the same batch.
    #[test]
    fn a_cut_keeps_a_workers_text_in_the_same_batch() {
        let main = |id: u64| {
            let mut item = Item::new(Kind::Text, "main");
            item.id = id;
            item
        };
        let merged = merged(vec![
            Update::Append {
                id: 2,
                text: " more".into(),
            },
            Update::Item(main(3)),
            Update::Append {
                id: 3,
                text: " cut".into(),
            },
            Update::Truncate { from: 2 },
        ]);
        assert!(matches!(
            merged.as_slice(),
            [Update::Append { id: 2, text }, Update::Truncate { from: 2 }] if text == " more"
        ));
    }

    /// A batched client misses nothing however far behind it falls: 10,000
    /// pieces of text it didn't read come as one item with all of them,
    /// where a broadcast client lagged. A worker's summaries come as the
    /// last, an item's change as the item, and the activity once.
    #[tokio::test]
    async fn batches_merge_appends_and_never_reset() {
        let feed = Feed::new("main");
        let (_, mut lagging) = feed.subscribe();
        let (snapshot, mut batches) = feed.subscribe_batched();
        assert!(snapshot.items.is_empty());
        assert!(batches.try_next().is_none());
        let id = feed.add(Item::new(Kind::Text, "0").agent("main"));
        for _ in 0..10_000 {
            let mut state = feed.lock();
            feed.append(&mut state, "main", Kind::Text, "x");
        }
        let batch = batches.next().await.unwrap();
        assert_eq!(batch.len(), 1, "one item");
        match &batch[0] {
            Update::Item(item) => {
                assert_eq!(item.id, id);
                assert_eq!(item.text.len(), 10_001);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            lagging.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));

        // Appended to an item from an earlier batch: one append.
        for _ in 0..3 {
            let mut state = feed.lock();
            feed.append(&mut state, "main", Kind::Text, "y");
        }
        let batch = batches.try_next().unwrap();
        assert!(
            matches!(batch.as_slice(), [Update::Append { id: appended, text }] if *appended == id && text == "yyy")
        );

        let summary = |state: WorkerState| WorkerSummary {
            title: "Count the files.".into(),
            state,
            ..WorkerSummary::new("r1")
        };
        let mut changed = Item::new(Kind::Text, "0x").agent("main");
        changed.id = id;
        let merged = merged(vec![
            Update::Worker(summary(WorkerState::Running)),
            Update::Activity,
            Update::Append {
                id,
                text: "z".into(),
            },
            Update::Change(changed.clone()),
            Update::Worker(summary(WorkerState::Finished)),
            Update::Activity,
        ]);
        assert!(matches!(merged.as_slice(), [
            Update::Change(item),
            Update::Worker(worker),
            Update::Activity,
        ] if item.text == "0x" && worker.state == WorkerState::Finished));
    }
    use crate::shell::SHELL_TOOL_NAME;
    use arko_agents::builtins::EDIT_FILE_TOOL_NAME;

    /// A call of a tool that isn't the shell, as the main agent makes it.
    fn lookup(call: Uuid) -> AgentEvent {
        AgentEvent::ToolCallFormatted {
            tool_call_id: call,
            tool_name: "mcp__search__lookup".into(),
            parameters: Default::default(),
        }
    }

    fn moved(job: Uuid) -> String {
        format!(
            "in the background as job {job}, 95.3 s\nThe user moved this call to the background."
        )
    }

    /// The call item of `call` in `feed`.
    fn call_item(feed: &Feed, name: &str) -> Item {
        feed.subscribe()
            .0
            .items
            .into_iter()
            .find(|item| item.kind == Kind::Call && item.text.contains(name))
            .unwrap()
    }

    /// A monitor's call says it is monitoring, and its line follows the
    /// monitor: its alerts, then how it ended. The monitor's summary says
    /// what it watches, its alerts and its end.
    #[test]
    fn a_monitors_line_follows_its_monitor() {
        let feed = Feed::new("main");
        let line = |feed: &Feed| call_item(feed, "monitor $ npm run dev");
        let start = |feed: &Feed, call: Uuid, job: Uuid, output: String| {
            let mut parameters = HashMap::new();
            parameters.insert("command".to_string(), serde_json::json!("npm run dev"));
            feed.event(
                &event(
                    "main",
                    AgentEvent::ToolCallFormatted {
                        tool_call_id: call,
                        tool_name: "monitor_start".into(),
                        parameters,
                    },
                ),
                None,
            );
            feed.event(
                &event(
                    "main",
                    AgentEvent::ToolResult {
                        tool_call_id: call,
                        tool_name: "monitor_start".into(),
                        output,
                    },
                ),
                None,
            );
            feed.job_started(
                JobStart::new(job, MAIN_AGENT, JobKind::Monitor, "npm run dev")
                    .made_by(call)
                    .started(now_ms() - 30 * 60_000)
                    .monitoring("lines that match error", now_ms()),
            );
        };
        let (call, job) = (Uuid::new_v4(), Uuid::new_v4());
        start(
            &feed,
            call,
            job,
            format!(
                "Monitor {job} started. It runs `npm run dev` and alerts you of lines that match `error`, until 12:00 UTC (30 min). It has printed nothing yet."
            ),
        );
        let item = line(&feed);
        assert_eq!(item.outcome.as_deref(), Some("monitoring"));
        assert_eq!(item.job, Some(job.to_string()));
        let summary = feed.subscribe().0.jobs[0].clone();
        assert_eq!(
            (summary.kind, summary.watching.as_deref(), summary.alerts),
            (JobKind::Monitor, Some("lines that match error"), Some(0))
        );

        feed.job_changed(job, 3, now_ms() + 60_000);
        assert_eq!(
            line(&feed).outcome.as_deref(),
            Some("monitoring · 3 alerts")
        );
        assert_eq!(feed.subscribe().0.jobs[0].alerts, Some(3));
        feed.job_ended(job, JobState::Expired, None);
        let ended = line(&feed);
        assert_eq!(
            (ended.status, ended.outcome.as_deref()),
            (Some(CallStatus::Ok), Some("ended after 30 min · 3 alerts"))
        );

        // A command that exited, and a monitor that ended at once.
        let feed = Feed::new("main");
        let (call, job) = (Uuid::new_v4(), Uuid::new_v4());
        start(
            &feed,
            call,
            job,
            format!("Monitor {job} started. It runs …"),
        );
        feed.job_ended(job, JobState::Failed, Some("exit 1".into()));
        let ended = line(&feed);
        assert_eq!(
            (ended.status, ended.outcome.as_deref()),
            (Some(CallStatus::Failed), Some("command exited (exit 1)"))
        );
        let feed = Feed::new("main");
        let (call, job) = (Uuid::new_v4(), Uuid::new_v4());
        start(
            &feed,
            call,
            job,
            format!(
                "Monitor {job} ended at once, because its command `npm run dev` exited with code 1. It printed nothing."
            ),
        );
        assert_eq!(line(&feed).outcome.as_deref(), Some("ended at once"));
        // Its later news doesn't change that.
        feed.job_ended(job, JobState::Failed, Some("exit 1".into()));
        assert_eq!(line(&feed).outcome.as_deref(), Some("ended at once"));
        // One restored from an earlier Arko says it ended.
        let feed = Feed::new("main");
        let (call, job) = (Uuid::new_v4(), Uuid::new_v4());
        start(
            &feed,
            call,
            job,
            format!("Monitor {job} started. It runs …"),
        );
        assert_eq!(feed.moved_jobs(), [job]);
        feed.job_ended_earlier(job, JobState::Interrupted);
        assert_eq!(line(&feed).outcome.as_deref(), Some("interrupted"));
    }

    /// Any tool's call that moved to the background says so on its line and
    /// names its job. The job's summary follows it, and when the job ends,
    /// the call's line says how.
    #[test]
    fn a_moved_calls_line_follows_its_job() {
        let feed = Feed::new("main");
        let (call, job) = (Uuid::new_v4(), Uuid::new_v4());
        feed.event(&event("main", lookup(call)), None);
        feed.event(
            &event(
                "main",
                AgentEvent::ToolResult {
                    tool_call_id: call,
                    tool_name: "mcp__search__lookup".into(),
                    output: moved(job),
                },
            ),
            None,
        );
        let item = call_item(&feed, "lookup");
        assert_eq!(item.status, Some(CallStatus::Ok));
        assert_eq!(
            item.outcome.as_deref(),
            Some(format!("in the background · job {}", &job.to_string()[..8]).as_str())
        );
        assert_eq!(item.job, Some(job.to_string()));

        let (_, mut receiver) = feed.subscribe();
        feed.job_started(JobStart {
            id: job,
            agent: MAIN_AGENT.into(),
            tool_call: Some(call),
            kind: JobKind::Call,
            label: "mcp__search__lookup".into(),
            started_at: now_ms() - 240_000,
            monitor: None,
        });
        // Twice: a job the feed has is left alone.
        feed.job_started(JobStart {
            id: job,
            agent: MAIN_AGENT.into(),
            tool_call: Some(call),
            kind: JobKind::Call,
            label: "again".into(),
            started_at: now_ms(),
            monitor: None,
        });
        let jobs = feed.subscribe().0.jobs;
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            (
                jobs[0].call,
                jobs[0].kind,
                jobs[0].state,
                jobs[0].label.as_str()
            ),
            (
                Some(item.id),
                JobKind::Call,
                JobState::Running,
                "mcp__search__lookup"
            )
        );
        feed.job_ended(job, JobState::Finished, None);
        let item = call_item(&feed, "lookup");
        assert_eq!(
            item.outcome.as_deref(),
            Some("finished in the background · 4 min")
        );
        let ended = &feed.subscribe().0.jobs[0];
        assert_eq!(ended.state, JobState::Finished);
        assert!(ended.ended_at.is_some());
        let updates = updates(&mut receiver);
        assert!(
            matches!(updates.as_slice(), [Update::Job(started), Update::Job(finished), Update::Change(changed)]
                if started.state == JobState::Running && finished.state == JobState::Finished && changed.id == item.id),
            "{updates:?}"
        );
        // An end comes once.
        feed.job_ended(job, JobState::Failed, None);
        assert_eq!(feed.subscribe().0.jobs[0].state, JobState::Finished);
    }

    /// A job that started before its call's result came still finds its
    /// call, and one that fails or is stopped says so on the call's line.
    #[test]
    fn a_jobs_call_is_found_in_either_order_and_its_end_is_shown() {
        for (ended, outcome, status) in [
            (
                JobState::Failed,
                "failed in the background",
                CallStatus::Failed,
            ),
            (JobState::Stopped, "stopped", CallStatus::Failed),
        ] {
            let feed = Feed::new("main");
            let (call, job) = (Uuid::new_v4(), Uuid::new_v4());
            feed.event(&event("main", lookup(call)), None);
            feed.job_started(JobStart {
                id: job,
                agent: MAIN_AGENT.into(),
                tool_call: Some(call),
                kind: JobKind::Call,
                label: "mcp__search__lookup".into(),
                started_at: now_ms(),
                monitor: None,
            });
            feed.event(
                &event(
                    "main",
                    AgentEvent::ToolResult {
                        tool_call_id: call,
                        tool_name: "mcp__search__lookup".into(),
                        output: moved(job),
                    },
                ),
                None,
            );
            let item = call_item(&feed, "lookup");
            assert_eq!(feed.subscribe().0.jobs[0].call, Some(item.id));
            feed.job_ended(job, ended, Some("stopped by the user".into()));
            let item = call_item(&feed, "lookup");
            assert_eq!(
                (item.outcome.as_deref(), item.status),
                (Some(outcome), Some(status))
            );
            assert_eq!(
                feed.subscribe().0.jobs[0].outcome.as_deref(),
                Some("stopped by the user")
            );
        }
    }

    /// A job that ended before its call's result came ends the call's line
    /// as the result comes, whether the feed saw the job run (a call of a
    /// parallel group, whose results come together) or not. One that ended
    /// unseen after the result ends the line at once.
    #[test]
    fn a_job_that_ended_first_ends_its_calls_line_when_the_result_comes() {
        let result = |feed: &Feed, call: Uuid, job: Uuid| {
            feed.event(
                &event(
                    "main",
                    AgentEvent::ToolResult {
                        tool_call_id: call,
                        tool_name: "mcp__search__lookup".into(),
                        output: moved(job),
                    },
                ),
                None,
            );
        };
        let outcome = |feed: &Feed| call_item(feed, "lookup").outcome;

        let feed = Feed::new("main");
        let (call, job) = (Uuid::new_v4(), Uuid::new_v4());
        feed.event(&event("main", lookup(call)), None);
        feed.job_started(JobStart {
            id: job,
            agent: MAIN_AGENT.into(),
            tool_call: Some(call),
            kind: JobKind::Call,
            label: "mcp__search__lookup".into(),
            started_at: now_ms() - 240_000,
            monitor: None,
        });
        feed.job_ended(job, JobState::Finished, None);
        result(&feed, call, job);
        assert_eq!(
            outcome(&feed).as_deref(),
            Some("finished in the background · 4 min")
        );

        let feed = Feed::new("main");
        let (call, job) = (Uuid::new_v4(), Uuid::new_v4());
        feed.event(&event("main", lookup(call)), None);
        feed.job_ended(job, JobState::Failed, None);
        result(&feed, call, job);
        assert_eq!(outcome(&feed).as_deref(), Some("failed in the background"));

        let feed = Feed::new("main");
        let (call, job) = (Uuid::new_v4(), Uuid::new_v4());
        feed.event(&event("main", lookup(call)), None);
        result(&feed, call, job);
        feed.job_ended(job, JobState::Stopped, None);
        assert_eq!(outcome(&feed).as_deref(), Some("stopped"));
        assert!(feed.subscribe().0.jobs.is_empty());
    }

    /// A restored line of a call that moved to the background says how its
    /// job ended with an earlier Arko, without a time it can't know.
    #[test]
    fn a_restored_moved_call_says_how_its_job_ended() {
        let feed = Feed::new("main");
        let job = Uuid::new_v4();
        let mut item = Item::new(Kind::Call, "lookup (search)");
        item.status = Some(CallStatus::Ok);
        item.outcome = Some(format!("in the background · job {}", &job.to_string()[..8]));
        item.job = Some(job.to_string());
        feed.restore(vec![item]);
        assert_eq!(feed.moved_jobs(), [job]);
        feed.job_ended_earlier(job, JobState::Finished);
        assert_eq!(
            call_item(&feed, "lookup").outcome.as_deref(),
            Some("finished in the background")
        );
        assert!(feed.moved_jobs().is_empty());
    }

    /// A worker that ends says so in an update of its own, after the summary
    /// with its report or its whole error (`last` has the start of the
    /// error), and that summary stays in a merged batch if the worker goes on.
    #[test]
    fn a_worker_s_end_is_an_update_after_its_summary_with_the_whole_error() {
        let feed = Feed::new("main");
        let (_, mut receiver) = feed.subscribe();
        let error = format!("the model refused {}", "twice ".repeat(60));
        feed.worker("r1", "Review", WorkerChange::Started);
        feed.worker("r1", "Review", WorkerChange::Failed { error: &error });
        let summary = feed.worker_summary("r1").unwrap();
        assert_eq!(summary.error.as_deref(), Some(error.as_str()));
        assert!(
            summary
                .last
                .as_deref()
                .is_some_and(|last| last.len() < error.len())
        );
        let seen = updates(&mut receiver);
        assert!(matches!(
            &seen[seen.len() - 2..],
            [Update::Worker(ended), Update::WorkerEnded { id, outcome }]
                if ended.state == WorkerState::Failed
                    && id == "r1"
                    && *outcome == WorkerState::Failed
        ));
        // (Starting is no end.)
        let ends = seen
            .iter()
            .filter(|update| matches!(update, Update::WorkerEnded { .. }))
            .count();
        assert_eq!(ends, 1);
        // Going on clears the error, and a batch that holds the end and the
        // next summary keeps both.
        feed.worker("r1", "Review", WorkerChange::Started);
        assert_eq!(feed.worker_summary("r1").unwrap().error, None);
        let again = feed.worker_summary("r1").unwrap();
        let batch = merged(vec![
            Update::Worker(summary),
            Update::WorkerEnded {
                id: "r1".into(),
                outcome: WorkerState::Failed,
            },
            Update::Worker(again),
        ]);
        assert!(matches!(
            batch.as_slice(),
            [Update::Worker(ended), Update::WorkerEnded { .. }, Update::Worker(running)]
                if ended.error.is_some() && running.error.is_none()
        ));
    }

    /// A worker's summary says what it started with, who started it and what
    /// its model calls cost. The cost adds up from the worker's events, a
    /// call a plan covered counting at its list price only, and a resumed
    /// session sets it from the saved runs.
    #[test]
    fn a_workers_summary_keeps_its_setup_and_what_its_calls_cost() {
        use arko_ai::{BillingKind, UsageInfo};
        let feed = Feed::new("main");
        feed.worker("r2", "Judge the render", WorkerChange::Started);
        feed.worker_under("r2", "r1");
        let mut info = WorkerInfo::new();
        info.model = Some("gpt-6.1-sol".into());
        info.effort = Some("high".into());
        info.profile = Some("3d-judge".into());
        info.depth = 2;
        feed.worker_info("r2", info);
        let summary = feed.worker_summary("r2").unwrap();
        assert_eq!(summary.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(summary.effort.as_deref(), Some("high"));
        assert_eq!(summary.profile.as_deref(), Some("3d-judge"));
        assert_eq!((summary.parent.as_deref(), summary.depth), (Some("r1"), 2));
        assert_eq!((summary.cost_billed, summary.cost_list), (0.0, 0.0));
        // A new summary starts with nothing known, one level down.
        let fresh = WorkerSummary::new("r9");
        assert_eq!((fresh.parent, fresh.depth, fresh.model), (None, 1, None));

        let usage = |billing, total, billed| {
            AgentEvent::UsageReported(UsageInfo {
                billing,
                total_cost: total,
                billed_cost: Some(billed),
                ..UsageInfo::default()
            })
        };
        let from = |agent: &str, usage| event(agent, usage);
        feed.event(
            &from("worker", usage(BillingKind::Metered, 0.25, 0.25)),
            Some("r2"),
        );
        feed.event(
            &from("worker", usage(BillingKind::Subscription, 0.5, 0.0)),
            Some("r2"),
        );
        // The main agent's usage is not a worker's.
        feed.event(&from("main", usage(BillingKind::Metered, 9.0, 9.0)), None);
        let summary = feed.worker_summary("r2").unwrap();
        assert!((summary.cost_billed - 0.25).abs() < 1e-9, "{summary:?}");
        assert!((summary.cost_list - 0.75).abs() < 1e-9, "{summary:?}");
        // The summary went out with the cost.
        let (snapshot, _) = feed.subscribe();
        assert!(
            snapshot
                .workers
                .iter()
                .any(|worker| worker.id == "r2" && worker.cost_list > 0.7)
        );
        // A session that resumes sets what the saved runs cost, and then
        // leaves it to the events.
        let mut info = WorkerInfo::new();
        info.costs = Some((1.0, 3.0));
        feed.worker_info("r2", info);
        let summary = feed.worker_summary("r2").unwrap();
        assert_eq!((summary.cost_billed, summary.cost_list), (1.0, 3.0));
        assert_eq!(summary.model, None, "the info is whole each time");
        assert!(feed.worker_summary("nobody").is_none());
    }

    /// A worker's stop notice is Info and says it is a stop, so a client that
    /// shows warnings only can choose to show it, and it names the worker that
    /// started the worker.
    #[test]
    fn a_stop_notice_says_its_level_and_who_started_the_worker() {
        let feed = Feed::new("main");
        feed.worker("r1", "Review", WorkerChange::Started);
        feed.worker_under("r2", "r1");
        feed.worker("r2", "Count", WorkerChange::Started);
        let reason = "its lead r1 was stopped";
        feed.worker("r2", "Count", WorkerChange::Stopped { reason });
        feed.worker("r1", "Review", WorkerChange::Stopped { reason: "" });
        let notices: Vec<(String, Option<Level>, Option<String>, bool)> = items(&feed)
            .into_iter()
            .filter(|item| item.kind == Kind::Notice)
            .map(|item| (item.text, item.level, item.parent, item.stop))
            .collect();
        assert_eq!(
            notices,
            [
                (
                    "stopped (its lead r1 was stopped)".to_string(),
                    Some(Level::Info),
                    Some("r1".to_string()),
                    true
                ),
                ("stopped".to_string(), Some(Level::Info), None, true),
            ]
        );
    }

    /// A worker that goes on after a stop (its lead messaged it) no longer
    /// shows the stop as what it does.
    #[test]
    fn a_worker_that_goes_on_after_a_stop_drops_the_stop_from_what_it_does() {
        let feed = Feed::new("main");
        feed.worker("r1", "Review", WorkerChange::Started);
        feed.event(&event("worker", shell(Uuid::new_v4(), "ls")), Some("r1"));
        feed.worker(
            "r1",
            "Review",
            WorkerChange::Stopped {
                reason: "its lead was stopped",
            },
        );
        assert_eq!(
            feed.worker_summary("r1").unwrap().last.as_deref(),
            Some("stopped (its lead was stopped)")
        );
        feed.worker("r1", "Review", WorkerChange::Started);
        assert_eq!(feed.worker_summary("r1").unwrap().last, None);
        // What it did in the run it goes on with is kept.
        feed.event(
            &event("worker", shell(Uuid::new_v4(), "cargo test")),
            Some("r1"),
        );
        let last = feed.worker_summary("r1").unwrap().last;
        assert!(last.is_some_and(|last| last.contains("cargo test")));
    }

    /// A worker that a worker started is placed in its parent's transcript
    /// and not the main one, from the start hook's word before its first
    /// event or from its summary's. The main agent's worker keeps its place.
    #[test]
    fn a_workers_worker_is_placed_in_its_parents_transcript() {
        let feed = Feed::new("main");
        feed.turn_started();
        feed.worker("r1", "Review the files", WorkerChange::Started);
        // The hook knows the parent before the worker's events come.
        feed.worker_under("r2", "r1");
        feed.event(&event("worker", shell(Uuid::new_v4(), "ls")), Some("r2"));
        feed.worker("r2", "Count the files", WorkerChange::Started);
        // Another is told its parent before its summary, which holds it.
        feed.worker_under("r3", "r1");
        let mut info = WorkerInfo::new();
        info.depth = 2;
        feed.worker_info("r3", info);
        assert_eq!(
            feed.worker_summary("r3").unwrap().parent.as_deref(),
            Some("r1")
        );
        feed.worker("r3", "Count the tests", WorkerChange::Started);
        let lines: Vec<(String, Option<String>)> = items(&feed)
            .into_iter()
            .filter(|item| item.kind == Kind::WorkerLine)
            .map(|item| (item.agent.clone(), item.worker.clone()))
            .collect();
        assert_eq!(
            lines,
            [
                ("main".to_string(), Some("r1".to_string())),
                ("r1".to_string(), Some("r2".to_string())),
                ("r1".to_string(), Some("r3".to_string())),
            ]
        );
    }

    /// The workers are listed with each one's workers after it, the first
    /// to start first, whatever order they started in.
    #[test]
    fn a_family_is_listed_with_each_workers_workers_after_it() {
        let worker = |id: &str, parent: Option<&str>| {
            let mut worker = WorkerSummary::new(id);
            worker.parent = parent.map(str::to_string);
            worker
        };
        let workers = [
            worker("r1", None),
            worker("r2", None),
            worker("r3", Some("r1")),
            worker("r4", Some("r3")),
            worker("r5", Some("r1")),
            worker("r6", Some("r2")),
            // Its parent is not among them, and it is 3 levels from the main
            // agent.
            WorkerSummary {
                depth: 3,
                ..worker("r7", Some("r9"))
            },
        ];
        let ids: Vec<&str> = family_order(&workers)
            .into_iter()
            .map(|at| workers[at].id.as_str())
            .collect();
        assert_eq!(ids, ["r1", "r3", "r4", "r5", "r2", "r6", "r7"]);
        assert!(family_order(&[]).is_empty());
        // A loop of parents leaves none out.
        let loops = [worker("r1", Some("r2")), worker("r2", Some("r1"))];
        assert_eq!(family_order(&loops).len(), 2);
        // The levels count from the roots listed, so r7, whose starter isn't
        // listed, is at level 1 whatever its depth says.
        let levels: Vec<(&str, u32)> = family_order_with_levels(&workers)
            .into_iter()
            .map(|(at, level)| (workers[at].id.as_str(), level))
            .collect();
        assert_eq!(
            levels,
            [
                ("r1", 1),
                ("r3", 2),
                ("r4", 3),
                ("r5", 2),
                ("r2", 1),
                ("r6", 2),
                ("r7", 1)
            ]
        );
        assert_eq!(family_order_with_levels(&loops), [(0, 1), (1, 1)]);
    }

    /// A worker's summary carries what it and its workers cost, and it goes
    /// out again when the cost of one of its workers changes.
    #[test]
    fn a_workers_summary_adds_up_what_its_family_cost() {
        use arko_ai::{BillingKind, UsageInfo};
        let feed = Feed::new("main");
        feed.worker("r1", "Review", WorkerChange::Started);
        feed.worker("r2", "Count", WorkerChange::Started);
        feed.worker("r3", "Count more", WorkerChange::Started);
        feed.worker_under("r2", "r1");
        feed.worker_under("r3", "r2");
        let mut info = WorkerInfo::new();
        info.depth = 2;
        feed.worker_info("r2", info.clone());
        info.depth = 3;
        feed.worker_info("r3", info);
        let usage = |total: f64, billed: f64| {
            AgentEvent::UsageReported(UsageInfo {
                billing: BillingKind::Metered,
                total_cost: total,
                billed_cost: Some(billed),
                ..UsageInfo::default()
            })
        };
        let (_, mut receiver) = feed.subscribe();
        feed.event(&event("worker", usage(0.5, 0.5)), Some("r1"));
        feed.event(&event("worker", usage(0.25, 0.25)), Some("r2"));
        feed.event(&event("worker", usage(1.0, 1.0)), Some("r3"));
        let family = |id: &str| {
            let summary = feed.worker_summary(id).unwrap();
            (
                summary.cost_billed,
                summary.family_cost_billed,
                summary.family_cost_list,
            )
        };
        // Each has its own, and its family's: r1 holds all three.
        assert_eq!(family("r3"), (1.0, 1.0, 1.0));
        assert_eq!(family("r2"), (0.25, 1.25, 1.25));
        assert_eq!(family("r1"), (0.5, 1.75, 1.75));
        // The summary of r1 went out with the total, also when only a
        // worker of its worker spent.
        let last_r1 = updates(&mut receiver)
            .into_iter()
            .filter_map(|update| match update {
                Update::Worker(summary) if summary.id == "r1" => Some(summary),
                _ => None,
            })
            .next_back()
            .unwrap();
        assert!(
            (last_r1.family_cost_billed - 1.75).abs() < 1e-9,
            "{last_r1:?}"
        );
        // A resumed session sets r2's saved cost, and the totals follow.
        let mut info = WorkerInfo::new();
        info.depth = 2;
        info.costs = Some((2.0, 2.0));
        feed.worker_info("r2", info);
        assert_eq!(family("r1").1, 3.5);
        assert_eq!(family("r2").1, 3.0);
        // The page reads the totals and the family from the summary's JSON.
        let json = serde_json::to_value(feed.worker_summary("r2").unwrap()).unwrap();
        assert_eq!(json["parent"], "r1");
        assert_eq!(json["depth"], 2);
        assert_eq!(json["family_cost_billed"], 3.0);
        assert!(json["family_cost_list"].is_number());
    }

    /// A job a command started (terminal_start) keeps its call's own line,
    /// and the call names it. A batch keeps a job's last summary.
    #[test]
    fn a_started_commands_job_names_its_call_and_batches_keep_the_last_summary() {
        let feed = Feed::new("main");
        let (call, job) = (Uuid::new_v4(), Uuid::new_v4());
        feed.event(&event("main", shell(call, "npm run dev")), None);
        feed.event(
            &event("main", result(call, "exit code 0, 0.1 s\nstarted")),
            None,
        );
        let before = call_item(&feed, "npm run dev");
        feed.job_started(JobStart {
            id: job,
            agent: MAIN_AGENT.into(),
            tool_call: Some(call),
            kind: JobKind::Command,
            label: "npm run dev".into(),
            started_at: now_ms(),
            monitor: None,
        });
        feed.job_ended(job, JobState::Failed, Some("exit 1".into()));
        let after = call_item(&feed, "npm run dev");
        assert_eq!(after.job, Some(job.to_string()));
        assert_eq!(after.outcome, before.outcome);
        let summary = |state| JobSummary {
            id: job.to_string(),
            agent: MAIN_AGENT.into(),
            call: None,
            kind: JobKind::Command,
            label: "npm run dev".into(),
            state,
            started_at: 0,
            ended_at: None,
            outcome: None,
            watching: None,
            alerts: None,
            expires_at: None,
        };
        let merged = merged(vec![
            Update::Job(summary(JobState::Running)),
            Update::Activity,
            Update::Job(summary(JobState::Finished)),
        ]);
        assert!(
            matches!(merged.as_slice(), [Update::Activity, Update::Job(last)] if last.state == JobState::Finished)
        );
    }

    fn event(agent: &str, event: AgentEvent) -> WorkflowEvent {
        WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: agent.into(),
            agent_run_id: Uuid::nil(),
            event,
        }
    }

    fn chunk(text: &str) -> AgentEvent {
        AgentEvent::AssistantResponseChunk(text.into())
    }

    fn shell(call: Uuid, command: &str) -> AgentEvent {
        AgentEvent::ToolCallFormatted {
            tool_call_id: call,
            tool_name: SHELL_TOOL_NAME.into(),
            parameters: [("command".to_string(), command.into())].into(),
        }
    }

    fn result(call: Uuid, output: &str) -> AgentEvent {
        AgentEvent::ToolResult {
            tool_call_id: call,
            tool_name: SHELL_TOOL_NAME.into(),
            output: output.into(),
        }
    }

    fn items(feed: &Feed) -> Vec<Item> {
        feed.subscribe().0.items
    }

    fn updates(receiver: &mut broadcast::Receiver<Update>) -> Vec<Update> {
        std::iter::from_fn(|| receiver.try_recv().ok()).collect()
    }

    /// The main agent's text streams into one item, which a call ends; the
    /// call's outcome comes as a change of it, with a command's output as a
    /// block; a worker's text and calls are its own items.
    #[test]
    fn events_become_items() {
        let feed = Feed::new("main");
        let (_, mut receiver) = feed.subscribe();
        feed.event(&event("main", chunk("Hel")), None);
        feed.event(&event("main", chunk("lo")), None);
        let call = Uuid::new_v4();
        feed.event(&event("main", shell(call, "cargo test")), None);
        feed.event(
            &event(
                "main",
                result(
                    call,
                    "exit code 0, 2.0 s\nrunning 3 tests\ntest result: ok. 3 passed; 0 failed",
                ),
            ),
            None,
        );
        let worker_call = Uuid::new_v4();
        feed.event(
            &event(
                "worker",
                AgentEvent::ToolCallFormatted {
                    tool_call_id: worker_call,
                    tool_name: "read_file".into(),
                    parameters: [("path".to_string(), "src/a.rs".into())].into(),
                },
            ),
            Some("r1"),
        );
        feed.event(&event("worker", chunk("Reading.")), Some("r1"));
        let items = items(&feed);
        let shown: Vec<(Kind, &str, &str)> = items
            .iter()
            .map(|item| (item.kind, item.agent.as_str(), item.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (Kind::Text, "main", "Hello"),
                (Kind::Call, "main", "$ cargo test"),
                (Kind::Result, "main", "exit code 0, 2.0 s"),
                (Kind::WorkerLine, "main", "r1"),
                (Kind::Call, "r1", "read src/a.rs"),
                (Kind::Text, "r1", "Reading."),
            ]
        );
        assert_eq!(items[1].status, Some(CallStatus::Ok));
        assert_eq!(items[1].outcome.as_deref(), Some("3 passed"));
        assert_eq!(items[2].level, None);
        assert_eq!(items[2].call, Some(items[1].id));
        assert_eq!(
            items[2].detail.as_deref(),
            Some("running 3 tests\ntest result: ok. 3 passed; 0 failed")
        );
        assert_eq!(items[3].worker.as_deref(), Some("r1"));
        assert_eq!(items[4].status, Some(CallStatus::Running));
        let updates = updates(&mut receiver);
        assert!(matches!(&updates[0], Update::Item(item) if item.text == "Hel"));
        assert!(matches!(&updates[1], Update::Append { id, text } if *id == 1 && text == "lo"));
        assert!(
            matches!(&updates[2], Update::Item(item) if item.status == Some(CallStatus::Running))
        );
        assert!(matches!(&updates[3], Update::Change(item)
            if item.id == 2 && item.status == Some(CallStatus::Ok)));
        let summaries: Vec<&WorkerSummary> = updates
            .iter()
            .filter_map(|update| match update {
                Update::Worker(summary) => Some(summary),
                _ => None,
            })
            .collect();
        assert_eq!(summaries[0].id, "r1");
        assert_eq!(summaries[0].state, WorkerState::Running);
        assert_eq!(feed.workers()[0].tool_calls, 1);
        assert_eq!(feed.workers()[0].last.as_deref(), Some("read src/a.rs"));
    }

    /// A wait for no run in particular waits for a message when the main
    /// agent has no runs going, and for the runs it has going otherwise, a
    /// few by name and many as a count. A run that ended isn't waited for,
    /// and a worker has no runs of its own. The worker's summary keeps the
    /// words as what it did last.
    #[test]
    fn a_wait_names_the_runs_the_agent_has_going() {
        let feed = Feed::new("main");
        let wait = |agent: &str, label: Option<&str>, parameters: serde_json::Value| {
            let parameters: HashMap<String, serde_json::Value> =
                serde_json::from_value(parameters).unwrap();
            feed.event(
                &event(
                    agent,
                    AgentEvent::ToolCallFormatted {
                        tool_call_id: Uuid::new_v4(),
                        tool_name: crate::crew::WAIT_FOR_AGENTS.into(),
                        parameters,
                    },
                ),
                label,
            );
            items(&feed)
                .into_iter()
                .rfind(|item| item.kind == Kind::Call)
                .unwrap()
                .text
        };
        let main = |parameters| wait("main", None, parameters);
        // No runs.
        assert_eq!(main(serde_json::json!({})), "waiting for a message");
        assert_eq!(
            main(serde_json::json!({"all": true, "timeout_seconds": 60})),
            "waiting for a message"
        );
        // One run, several, and many.
        feed.worker("r1", "Count the files.", WorkerChange::Started);
        assert_eq!(main(serde_json::json!({})), "waiting for r1");
        feed.worker("r2", "Read the docs.", WorkerChange::Queued);
        assert_eq!(main(serde_json::json!({})), "waiting for r1, r2");
        for id in ["r3", "r4", "r5"] {
            feed.worker(id, "More.", WorkerChange::Started);
        }
        assert_eq!(main(serde_json::json!({})), "waiting for r1, r2 and 3 more");
        // A run that ended isn't waited for, and the runs go by number.
        feed.worker("r1", "", WorkerChange::Finished { report: "Done." });
        feed.worker("r4", "", WorkerChange::Stopped { reason: "" });
        feed.worker("r5", "", WorkerChange::Failed { error: "oops" });
        assert_eq!(main(serde_json::json!({})), "waiting for r2, r3");
        // The ids a call names are what it waits for, also those that ended.
        assert_eq!(
            main(serde_json::json!({"runs": "r1, r4"})),
            "waiting for r1, r4"
        );
        assert_eq!(
            main(serde_json::json!({"runs": ["r1", "r2"]})),
            "waiting for r1, r2"
        );
        // A worker has no runs of its own, and the agent list shows its
        // words.
        assert_eq!(
            wait("worker", Some("r2"), serde_json::json!({})),
            "waiting for a message"
        );
        let r2 = feed.workers().into_iter().find(|w| w.id == "r2").unwrap();
        assert_eq!(r2.last.as_deref(), Some("waiting for a message"));
        feed.worker("r2", "", WorkerChange::Finished { report: "Done." });
        feed.worker("r3", "", WorkerChange::Finished { report: "Done." });
        assert_eq!(main(serde_json::json!({})), "waiting for a message");
    }

    /// A text item goes on with its own agent's chunks, whatever the other
    /// agents do in between.
    #[test]
    fn each_agent_has_its_own_streaming_text() {
        let feed = Feed::new("main");
        feed.event(&event("main", chunk("The plan")), None);
        feed.event(&event("worker", chunk("Looking")), Some("r1"));
        feed.event(&event("main", chunk(" is set.")), None);
        feed.event(&event("worker", chunk(" around.")), Some("r1"));
        let texts: Vec<(String, String)> = items(&feed)
            .into_iter()
            .filter(|item| item.kind == Kind::Text)
            .map(|item| (item.agent, item.text))
            .collect();
        assert_eq!(
            texts,
            [
                ("main".to_string(), "The plan is set.".to_string()),
                ("r1".to_string(), "Looking around.".to_string()),
            ]
        );
    }

    /// An agent's thinking streams into an item of its own, the main
    /// agent's and a worker's: text after it starts a text item, and
    /// thinking after text a thinking item, so the two never merge. A
    /// reply in text still takes over the text it streamed, after
    /// thinking. With the thinking hidden, there is none.
    #[test]
    fn thinking_streams_into_items_of_its_own() {
        let thinking = |text: &str| AgentEvent::ReasoningChunk(text.into());
        let feed = Feed::new("main");
        feed.event(&event("main", thinking("The user asks")), None);
        feed.event(&event("main", thinking(" about the tests.")), None);
        feed.event(&event("main", chunk("Running them.")), None);
        feed.event(&event("worker", thinking("Where is it?")), Some("r1"));
        feed.event(&event("main", thinking("Then report.")), None);
        feed.event(&event("main", chunk("All pass.")), None);
        feed.reply("All pass.");
        let shown: Vec<(Kind, String, String)> = items(&feed)
            .into_iter()
            .filter(|item| item.kind != Kind::WorkerLine)
            .map(|item| (item.kind, item.agent, item.text))
            .collect();
        let item = |kind, agent: &str, text: &str| (kind, agent.to_string(), text.to_string());
        assert_eq!(
            shown,
            [
                item(Kind::Thinking, "main", "The user asks about the tests."),
                item(Kind::Text, "main", "Running them."),
                item(Kind::Thinking, "r1", "Where is it?"),
                item(Kind::Thinking, "main", "Then report."),
                item(Kind::Reply, "main", "All pass."),
            ]
        );
        assert_eq!(
            serde_json::to_value(&items(&feed)[0]).unwrap()["kind"],
            "thinking"
        );

        let hidden = Feed::new("main");
        hidden.hide_thinking(true);
        hidden.event(&event("main", thinking("Hmm.")), None);
        hidden.event(&event("main", chunk("Done.")), None);
        let kinds: Vec<Kind> = items(&hidden).iter().map(|item| item.kind).collect();
        assert_eq!(kinds, [Kind::Text]);
    }

    /// A call the feed doesn't draw, such as `ask_user`, still ends the
    /// text before it: the text of the next response is an item of its own.
    #[test]
    fn a_call_not_drawn_still_ends_the_text_before_it() {
        let feed = Feed::new("main");
        feed.event(&event("main", chunk("Here are the questions:")), None);
        feed.event(
            &event(
                "main",
                AgentEvent::ToolCallFormatted {
                    tool_call_id: Uuid::new_v4(),
                    tool_name: arko_agents::builtins::ASK_USER_TOOL_NAME.into(),
                    parameters: [("questions".to_string(), "Which one?".into())].into(),
                },
            ),
            None,
        );
        feed.event(&event("main", chunk("I sent you the questions.")), None);
        let texts: Vec<String> = items(&feed)
            .into_iter()
            .filter(|item| item.kind == Kind::Text)
            .map(|item| item.text)
            .collect();
        assert_eq!(
            texts,
            ["Here are the questions:", "I sent you the questions."]
        );
    }

    fn read(seqs: &[u64]) -> AgentEvent {
        AgentEvent::ContextInjected {
            ids: seqs.iter().map(|seq| format!("note:{seq}")).collect(),
            bytes: 100,
        }
    }

    /// A note typed while the agent's reply streams waits below it, pending.
    /// The reply's later chunks join its text, and so does what the reply
    /// went on with before the agent read the note (a call). Once the agent
    /// reads it, it moves to where the agent read it, no longer pending,
    /// and what the agent says next comes after it.
    #[test]
    fn a_note_typed_mid_reply_waits_below_it_until_the_agent_reads_it() {
        let feed = Feed::new("main");
        let (_, mut receiver) = feed.subscribe();
        feed.event(&event("main", chunk("It turned out to be a kit f")), None);
        let note = feed.note("Keep the old name.", Source::Terminal, 4, Vec::new());
        feed.event(&event("main", chunk("ix, so that went to the kit.")), None);
        let call = Uuid::new_v4();
        feed.event(&event("main", shell(call, "cargo test")), None);
        feed.event(&event("main", result(call, "exit code 0, 1.0 s")), None);
        let shown = |feed: &Feed| -> Vec<(Kind, String, bool)> {
            items(feed)
                .into_iter()
                .map(|item| (item.kind, item.text, item.pending))
                .collect()
        };
        let reply = "It turned out to be a kit fix, so that went to the kit.";
        let entry = |kind, text: &str, pending| (kind, text.to_string(), pending);
        assert_eq!(
            shown(&feed),
            [
                entry(Kind::Text, reply, false),
                entry(Kind::User, "Keep the old name.", true),
                entry(Kind::Call, "$ cargo test", false),
            ]
        );
        assert!(matches!(
            &updates(&mut receiver)[2],
            Update::Append { text, .. } if text == "ix, so that went to the kit."
        ));

        // Another agent's update, and a note the agent hasn't got, change
        // nothing.
        feed.event(&event("worker", read(&[4])), Some("r1"));
        feed.event(&event("main", read(&[3])), None);
        assert!(items(&feed)[1].pending);

        feed.event(&event("main", read(&[4])), None);
        feed.event(&event("main", chunk("Renamed it back.")), None);
        assert_eq!(
            shown(&feed),
            [
                entry(Kind::Text, reply, false),
                entry(Kind::Call, "$ cargo test", false),
                entry(Kind::User, "Keep the old name.", false),
                entry(Kind::Text, "Renamed it back.", false),
            ]
        );
        let updates = updates(&mut receiver);
        assert!(matches!(updates[0], Update::Remove { id } if id == note));
        assert!(matches!(&updates[1], Update::Item(item)
            if item.text == "Keep the old name." && !item.pending && item.id > note));
        let json = |item: &Item| serde_json::to_value(item).unwrap();
        assert_eq!(json(&items(&feed)[2]).get("pending"), None);
        let mut pending = Item::new(Kind::User, "Wait.");
        pending.pending = true;
        assert_eq!(json(&pending)["pending"], true);
    }

    /// The reply the agent's text was takes it over with a note still
    /// waiting below it, and the note's item comes as read when the agent
    /// read it before the item came. A note that went with a message is
    /// read, and a cut drops the notes it cuts.
    #[test]
    fn notes_are_read_however_they_reach_the_agent() {
        let feed = Feed::new("main");
        feed.event(&event("main", chunk("All done.")), None);
        feed.note("And the docs?", Source::Web, 1, Vec::new());
        feed.reply("All done.");
        feed.turn_ended("2 s", None);
        let shown = |feed: &Feed| -> Vec<(Kind, String, bool)> {
            items(feed)
                .into_iter()
                .map(|item| (item.kind, item.text, item.pending))
                .collect()
        };
        let entry = |kind, text: &str, pending| (kind, text.to_string(), pending);
        assert_eq!(
            shown(&feed),
            [
                entry(Kind::Reply, "All done.", false),
                entry(Kind::User, "And the docs?", true),
                entry(Kind::Turn, "2 s", false),
            ]
        );
        // The next turn's message took it.
        feed.notes_sent(&["And the docs?".to_string()]);
        assert_eq!(
            shown(&feed)[1..],
            [
                entry(Kind::Turn, "2 s", false),
                entry(Kind::User, "And the docs?", false),
            ]
        );

        feed.event(&event("main", read(&[2])), None);
        feed.note("Use tabs.", Source::Terminal, 2, Vec::new());
        assert!(!items(&feed).last().unwrap().pending);

        let cut = feed.note("Use spaces.", Source::Terminal, 3, Vec::new());
        feed.truncate(cut);
        feed.notes_sent(&["Use spaces.".to_string()]);
        feed.event(&event("main", read(&[3])), None);
        assert_eq!(items(&feed).last().unwrap().text, "Use tabs.");
    }

    /// A call that fails or is refused says so on its line; one still
    /// running when its turn ends is marked as interrupted.
    #[test]
    fn calls_end_failed_refused_or_interrupted() {
        let feed = Feed::new("main");
        feed.turn_started();
        let (failed, refused, running) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        feed.event(&event("main", shell(failed, "make")), None);
        feed.event(
            &event(
                "main",
                AgentEvent::ErrorEncountered {
                    error_message: "Tool 'shell' execution failed: no such file".into(),
                    requires_approval_to_retry: false,
                    tool_call_id: Some(failed),
                    error_kind: None,
                },
            ),
            None,
        );
        feed.event(&event("main", shell(refused, "rm -rf /")), None);
        feed.event(
            &event(
                "main",
                AgentEvent::ToolRejected {
                    tool_call_id: refused,
                    tool_name: SHELL_TOOL_NAME.into(),
                    reason: Some("outside the workspace".into()),
                },
            ),
            None,
        );
        feed.event(&event("main", shell(running, "sleep 100")), None);
        let stopped = Uuid::new_v4();
        feed.event(&event("main", shell(stopped, "make test")), None);
        feed.event(
            &event(
                "main",
                AgentEvent::ToolRejected {
                    tool_call_id: stopped,
                    tool_name: SHELL_TOOL_NAME.into(),
                    reason: Some(
                        "The task was cancelled while this call was running, so the call was stopped before it finished.".into(),
                    ),
                },
            ),
            None,
        );
        feed.turn_ended("1 s", None);
        let outcomes: Vec<(Option<CallStatus>, Option<String>)> = items(&feed)
            .into_iter()
            .filter(|item| item.kind == Kind::Call)
            .map(|item| (item.status, item.outcome))
            .collect();
        assert_eq!(
            outcomes,
            [
                (Some(CallStatus::Failed), Some("no such file".into())),
                (
                    Some(CallStatus::Failed),
                    Some("refused (outside the workspace)".into())
                ),
                (Some(CallStatus::Failed), Some("interrupted".into())),
                (Some(CallStatus::Failed), Some("stopped".into())),
            ]
        );
        // An error that isn't a call's is a notice.
        feed.event(
            &event(
                "main",
                AgentEvent::ErrorEncountered {
                    error_message: "the model is overloaded".into(),
                    requires_approval_to_retry: false,
                    tool_call_id: None,
                    error_kind: None,
                },
            ),
            None,
        );
        let last = items(&feed).pop().unwrap();
        assert_eq!((last.kind, last.level), (Kind::Notice, Some(Level::Error)));
        // Claude Code turning down Claude Mythos 5.1 says what access it needs.
        feed.event(
            &event(
                "main",
                AgentEvent::ErrorEncountered {
                    error_message: "There's an issue with the selected model (claude-mythos-5-1). It may not exist or you may not have access to it.".into(),
                    requires_approval_to_retry: true,
                    tool_call_id: None,
                    error_kind: Some(arko_ai::ErrorKind::NotFound),
                },
            ),
            None,
        );
        let last = items(&feed).pop().unwrap();
        assert!(
            last.text
                .ends_with("needs access through Anthropic's Cyber Verification Program."),
            "{}",
            last.text
        );
    }

    /// `turn` is null before the first turn, and grows with each; the
    /// user's message that starts a turn counts in it.
    /// A reply the agent's text already streamed becomes that item, as a
    /// change, so it shows once: past a notice (an output limit reached)
    /// too. Another reply is an item of its own.
    #[test]
    fn a_reply_in_text_takes_over_its_streamed_text() {
        let feed = Feed::new("main");
        let (_, mut receiver) = feed.subscribe();
        feed.event(&event("main", chunk("The answer ")), None);
        feed.event(&event("main", chunk("is 391.\n")), None);
        feed.event(
            &event(
                "main",
                AgentEvent::OutputTokenLimitReached {
                    model: "m".into(),
                    output_tokens: 10,
                },
            ),
            None,
        );
        updates(&mut receiver);
        feed.reply("The answer is 391.");
        let shown: Vec<(Kind, String)> = items(&feed)
            .into_iter()
            .map(|item| (item.kind, item.text))
            .collect();
        assert_eq!(
            shown,
            [
                (Kind::Reply, "The answer is 391.".to_string()),
                (
                    Kind::Notice,
                    "the reply reached the model's output limit (10 tokens)".to_string()
                ),
            ]
        );
        let updates = updates(&mut receiver);
        assert!(
            matches!(updates.as_slice(), [Update::Change(item)] if item.kind == Kind::Reply),
            "{updates:?}"
        );
        // The next text starts an item of its own.
        feed.event(&event("main", chunk("More.")), None);
        assert_eq!(items(&feed).last().unwrap().kind, Kind::Text);

        // A reply that isn't the streamed text (a complete_task summary).
        feed.event(&event("main", shell(Uuid::new_v4(), "ls")), None);
        feed.reply("Listed the files.");
        let last: Vec<(Kind, String)> = items(&feed)
            .into_iter()
            .rev()
            .take(2)
            .map(|item| (item.kind, item.text))
            .collect();
        assert_eq!(last[0], (Kind::Reply, "Listed the files.".to_string()));
        assert_eq!(last[1].0, Kind::Call);
        assert!(
            items(&feed)
                .iter()
                .any(|item| item.text == "More." && item.kind == Kind::Text)
        );
    }

    #[test]
    fn items_carry_their_turn() {
        let feed = Feed::new("main");
        feed.notice(Level::Info, "Resumed.");
        feed.user("Fix it.", Source::Web, Delivery::Turn);
        feed.turn_started();
        feed.event(&event("main", chunk("Fixing.")), None);
        feed.user("Also the docs.", Source::Terminal, Delivery::Note);
        feed.turn_ended("2 s", None);
        // A turn Arko starts for the workers' news has no message.
        feed.turn_started();
        feed.reply("Done.");
        feed.turn_ended("1 s", None);
        let turns: Vec<(Kind, Option<u64>)> = items(&feed)
            .into_iter()
            .map(|item| (item.kind, item.turn))
            .collect();
        assert_eq!(
            turns,
            [
                (Kind::Notice, None),
                (Kind::User, Some(1)),
                (Kind::Text, Some(1)),
                (Kind::User, Some(1)),
                (Kind::Turn, Some(1)),
                (Kind::Reply, Some(2)),
                (Kind::Turn, Some(2)),
            ]
        );
    }

    /// Restored items keep their turns, after the feed's own, and the next
    /// turn comes after theirs.
    #[test]
    fn restored_items_keep_their_turns() {
        let feed = Feed::new("main");
        feed.notice(Level::Info, "Resumed.");
        let restored = |text: &str, turn: u64| {
            let mut item = Item::new(Kind::User, text);
            item.delivery = Some(Delivery::Turn);
            item.turn = Some(turn);
            item
        };
        let mut reply = Item::new(Kind::Reply, "Hi.");
        reply.turn = Some(1);
        let mut first = restored("Hello.", 1);
        first.id = 1;
        reply.id = 2;
        let mut second = restored("Bye.", 2);
        second.id = 3;
        feed.restore(vec![first.clone(), reply.clone(), second.clone()]);
        feed.user("Again.", Source::Web, Delivery::Turn);
        // Another conversation's, restored later: after the turns so far.
        feed.restore(vec![first]);
        let items = items(&feed);
        let turns: Vec<(&str, Option<u64>)> = items
            .iter()
            .map(|item| (item.text.as_str(), item.turn))
            .collect();
        assert_eq!(
            turns,
            [
                ("Resumed.", None),
                ("Hello.", Some(1)),
                ("Hi.", Some(1)),
                ("Bye.", Some(2)),
                ("Again.", Some(3)),
                ("Hello.", Some(4)),
            ]
        );
    }

    /// A question is the activity until it is answered, or the turn ends.
    #[test]
    fn questions_are_open_until_answered() {
        let feed = Feed::new("main");
        feed.turn_started();
        let id = feed.question("Tabs or spaces?");
        let since = feed.activity().since;
        assert!(since.is_some(), "the turn's start");
        assert_eq!(
            feed.activity(),
            Activity {
                working: true,
                since,
                question: Some(Question {
                    id,
                    text: "Tabs or spaces?".into()
                })
            }
        );
        feed.answered(id, "spaces", Source::Web);
        assert_eq!(feed.activity().question, None);
        let again = feed.question("Sure?");
        assert_ne!(again, id);
        feed.turn_ended("3 s", None);
        assert_eq!(feed.activity(), Activity::default());
    }

    /// A worker gets a placeholder in the main transcript where it starts
    /// (again in a later turn, when it goes on there), a summary that
    /// follows it, and its report as the end of its own transcript.
    #[test]
    fn workers_have_a_place_a_summary_and_a_report() {
        let feed = Feed::new("main");
        feed.turn_started();
        let task = "Fix the flaky test\n\nIt fails one run in ten.";
        feed.worker("r1", task, WorkerChange::Queued);
        feed.worker("r1", task, WorkerChange::Started);
        let (snapshot, mut receiver) = feed.subscribe();
        let lines: Vec<&Item> = snapshot
            .items
            .iter()
            .filter(|item| item.kind == Kind::WorkerLine)
            .collect();
        assert_eq!(lines.len(), 1, "one placeholder in a turn");
        assert_eq!(lines[0].text, "Fix the flaky test");
        assert_eq!(snapshot.workers[0].state, WorkerState::Running);
        assert!(snapshot.workers[0].started_at.is_some());

        let call = Uuid::new_v4();
        feed.event(&event("worker", shell(call, "cargo test")), Some("r1"));
        feed.worker(
            "r1",
            task,
            WorkerChange::Finished {
                report: "Fixed: a race in setup.",
            },
        );
        let summary = &feed.workers()[0];
        assert_eq!(summary.state, WorkerState::Finished);
        assert_eq!(summary.tool_calls, 1);
        assert_eq!(summary.report.as_deref(), Some("Fixed: a race in setup."));
        assert!(summary.ended_at.is_some());
        let kept = items(&feed);
        let own: Vec<(Kind, String)> = kept
            .iter()
            .filter(|item| item.agent == "r1")
            .map(|item| (item.kind, item.text.clone()))
            .collect();
        assert_eq!(
            own,
            [
                (Kind::Call, "$ cargo test".into()),
                (Kind::Reply, "Fixed: a race in setup.".into())
            ]
        );
        // Its call never got a result: its run ended.
        let call = kept.iter().find(|item| item.agent == "r1").unwrap();
        assert_eq!(call.outcome.as_deref(), Some("interrupted"));
        // The state changes went out at once.
        let states: Vec<WorkerState> = updates(&mut receiver)
            .into_iter()
            .filter_map(|update| match update {
                Update::Worker(summary) => Some(summary.state),
                _ => None,
            })
            .collect();
        assert_eq!(states.last(), Some(&WorkerState::Finished));

        feed.turn_ended("1 m", None);
        feed.user("Check it again.", Source::Web, Delivery::Turn);
        feed.turn_started();
        feed.worker("r1", task, WorkerChange::Started);
        let placed: Vec<Option<u64>> = items(&feed)
            .into_iter()
            .filter(|item| item.kind == Kind::WorkerLine)
            .map(|item| item.turn)
            .collect();
        assert_eq!(placed, [Some(1), Some(2)]);
        assert_eq!(feed.workers()[0].report, None);
        feed.worker(
            "r1",
            task,
            WorkerChange::Failed {
                error: "the model failed",
            },
        );
        assert_eq!(feed.workers()[0].state, WorkerState::Failed);
        assert_eq!(feed.workers()[0].last.as_deref(), Some("the model failed"));
    }

    /// A worker's run time is the time its runs ran, not the time it
    /// waited between them, and a running one counts its current run so
    /// far. One restored with a session goes on from what it ran before.
    #[test]
    fn a_workers_run_time_counts_its_runs() {
        let pause = |ms| std::thread::sleep(Duration::from_millis(ms));
        let feed = Feed::new("main");
        let begun = now_ms();
        feed.worker("r1", "Count", WorkerChange::Started);
        pause(40);
        feed.worker("r1", "Count", WorkerChange::Finished { report: "3." });
        let first = feed.workers()[0].ran_ms;
        assert!(first >= 40, "{first}");
        assert_eq!(feed.workers()[0].running_since, None);
        pause(200);
        feed.worker("r1", "Count", WorkerChange::Started);
        pause(40);
        let running = feed.workers()[0].clone();
        assert_eq!(running.ran_ms, first);
        assert!(running.run_time_ms(now_ms()) >= first + 40);
        feed.worker("r1", "Count", WorkerChange::Stopped { reason: "" });
        let total = feed.workers()[0].ran_ms;
        assert!(total >= first + 40);
        let wall = u64::try_from(now_ms() - begun).unwrap();
        assert!(
            total + 200 <= wall,
            "the wait doesn't count: {total} of {wall}"
        );
        assert_eq!(feed.worker_run_times(), [("r1".to_string(), total)]);
        let resumed = Feed::new("main");
        resumed.worker_restored("r1", "Count", WorkerState::Stopped, None, 0, total);
        assert_eq!(resumed.workers()[0].ran_ms, total);
        assert_eq!(resumed.workers()[0].run_time_ms(now_ms()), total);
    }

    /// The workers of a resumed session are listed, without a place in the
    /// transcript; a worker heard of first by its own events runs.
    #[test]
    fn workers_are_known_from_a_saved_session_or_their_events() {
        let feed = Feed::new("main");
        feed.worker_restored(
            "r1",
            "Review",
            WorkerState::Finished,
            Some("All good."),
            5,
            0,
        );
        feed.worker_restored("r1", "Again", WorkerState::Running, None, 6, 0);
        assert_eq!(feed.workers().len(), 1);
        assert_eq!(feed.workers()[0].ended_at, Some(5));
        assert!(items(&feed).is_empty());
        feed.event(&event("worker", chunk("Hi.")), Some("r2"));
        let workers = feed.workers();
        assert_eq!(
            (workers[1].id.as_str(), workers[1].state),
            ("r2", WorkerState::Running)
        );
        assert_eq!(items(&feed)[0].kind, Kind::WorkerLine);
    }

    /// A busy worker's summary goes out a few times a second, and its
    /// latest change always does.
    #[tokio::test]
    async fn worker_summaries_are_throttled() {
        let feed = Feed::new("main");
        feed.worker("r1", "Count", WorkerChange::Started);
        let (_, mut receiver) = feed.subscribe();
        for _ in 0..50 {
            feed.event(&event("worker", shell(Uuid::new_v4(), "ls")), Some("r1"));
        }
        let sent = |updates: Vec<Update>| -> Vec<u32> {
            updates
                .into_iter()
                .filter_map(|update| match update {
                    Update::Worker(summary) => Some(summary.tool_calls),
                    _ => None,
                })
                .collect()
        };
        assert!(sent(updates(&mut receiver)).is_empty(), "too soon");
        tokio::time::sleep(WORKER_EVERY * 2).await;
        assert_eq!(sent(updates(&mut receiver)), [50]);
    }

    /// A message the lead or a worker sends a worker shows in that
    /// worker's transcript once it is sent, with who sent it. One to the
    /// lead is only its call's line. A finished worker's report, the
    /// message that continues it and its new work come in that order.
    #[test]
    fn messages_reach_the_workers_that_get_them() {
        let feed = Feed::new("main");
        let send = |agent: &str, label: Option<&str>, to: &str, text: &str, output: &str| {
            let call = Uuid::new_v4();
            let tool_name = crate::crew::MESSAGE_AGENT.to_string();
            let parameters = [
                ("to".to_string(), to.into()),
                ("message".to_string(), text.into()),
            ]
            .into();
            feed.event(
                &event(
                    agent,
                    AgentEvent::ToolCallFormatted {
                        tool_call_id: call,
                        tool_name: tool_name.clone(),
                        parameters,
                    },
                ),
                label,
            );
            let output = output.into();
            feed.event(
                &event(
                    agent,
                    AgentEvent::ToolResult {
                        tool_call_id: call,
                        tool_name,
                        output,
                    },
                ),
                label,
            );
        };
        feed.worker("r1", "Fix it", WorkerChange::Started);
        feed.worker("r1", "Fix it", WorkerChange::Finished { report: "Fixed." });
        send(
            "main",
            None,
            "r1",
            "Also the docs.",
            "Sent: r1 continues its task with your message, with its context; its new report reaches you when it finishes.",
        );
        feed.worker("r1", "Fix it", WorkerChange::Started);
        feed.event(&event("worker", chunk("On it.")), Some("r1"));
        send("worker", Some("r1"), "r2", "a.rs is done.", "Sent to r2.");
        send(
            "worker",
            Some("r1"),
            "lead",
            "Halfway.",
            "Sent. Your lead's answer, if any, reaches you at your next step (or through wait_for_agents).",
        );
        send("main", None, "r9", "Hello?", "no agent run is called r9");
        let items = items(&feed);
        let of = |agent: &str| -> Vec<(Kind, Option<&str>, &str)> {
            items
                .iter()
                .filter(|item| item.agent == agent)
                .map(|item| (item.kind, item.from.as_deref(), item.text.as_str()))
                .collect()
        };
        assert_eq!(
            of("r1"),
            [
                (Kind::Reply, None, "Fixed."),
                (Kind::Message, Some("main"), "Also the docs."),
                (Kind::Text, None, "On it."),
                (Kind::Call, None, "message r2 “a.rs is done.”"),
                (Kind::Call, None, "message lead “Halfway.”"),
            ]
        );
        assert_eq!(of("r2"), [(Kind::Message, Some("r1"), "a.rs is done.")]);
        assert!(of("r9").is_empty());
        assert!(
            of("main")
                .iter()
                .all(|(kind, ..)| matches!(kind, Kind::Call | Kind::WorkerLine))
        );
    }

    /// A command's whole output stays on its block for the terminal's
    /// view, out of what the web page gets, and only the latest outputs
    /// keep theirs.
    #[test]
    fn whole_outputs_are_kept_apart_and_bounded() {
        let feed = Feed::new("main");
        let run = |output: &str| {
            let call = Uuid::new_v4();
            feed.event(&event("main", shell(call, "seq")), None);
            let output = format!("exit code 0, 0.1 s\n{output}");
            feed.event(&event("main", result(call, &output)), None);
        };
        let output = (1..=10).map(|n| n.to_string()).collect::<Vec<_>>();
        run(&output.join("\n"));
        let block = items(&feed).pop().unwrap();
        assert_eq!(block.detail.as_deref(), Some("1\n2\n3\n4\n… 6 more lines"));
        assert_eq!(block.whole.as_deref(), Some(output.join("\n").as_str()));
        let json = serde_json::to_value(&block).unwrap();
        assert!(json.get("whole").is_none(), "{json}");
        assert!(json.get("detail").is_some());
        // (A short output has nothing more to show.)
        run("1\n2");
        assert_eq!(items(&feed).pop().unwrap().whole, None);
        // About a quarter of the budget each: the oldest lose theirs.
        let long = "x\n".repeat(MAIN_BUDGET.wholes / 8);
        for _ in 0..6 {
            run(&long);
        }
        let wholes: Vec<usize> = items(&feed)
            .iter()
            .filter(|item| item.kind == Kind::Result)
            .map(Item::whole_size)
            .collect();
        assert_eq!(wholes.len(), 8, "the blocks stay");
        assert!(wholes.iter().sum::<usize>() <= MAIN_BUDGET.wholes);
        assert_eq!(wholes[0], 0);
        assert!(wholes[7] > 0);
    }

    /// A response that fails after it streamed takes back what it streamed:
    /// the items it started go, and what it added to an item goes from it.
    /// The retry streams anew, into the item the text was in.
    #[test]
    fn a_failed_responses_output_is_taken_back() {
        let feed = Feed::new("main");
        let (_, mut batches) = feed.subscribe_batched();
        let started = || AgentEvent::ModelRequestStarted {
            model: "gpt-6-luna".into(),
            subagent: None,
            sizes: None,
            bytes_per_token: 3.0,
        };
        feed.event(&event("main", chunk("Before. ")), None);
        let before = items(&feed)[0].id;
        feed.event(&event("main", started()), None);
        feed.event(&event("main", chunk("Broken")), None);
        feed.event(
            &event("main", AgentEvent::ReasoningChunk("Hmm".into())),
            None,
        );
        let discarded = AgentEvent::StreamedOutputDiscarded {
            text_bytes: 6,
            reasoning_bytes: 3,
        };
        feed.event(&event("main", discarded), None);
        assert_eq!(
            items(&feed)
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            ["Before. "]
        );
        feed.event(&event("main", started()), None);
        feed.event(&event("main", chunk("Fixed.")), None);
        let kept = items(&feed);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            (kept[0].id, kept[0].text.as_str()),
            (before, "Before. Fixed.")
        );
        // A client in this process sees the same.
        let batch = batches.try_next().unwrap();
        let mut shown: Vec<Item> = Vec::new();
        for update in batch {
            match update {
                Update::Item(item) => shown.push(item),
                Update::Change(item) => {
                    if let Some(old) = shown.iter_mut().find(|old| old.id == item.id) {
                        *old = item;
                    }
                }
                Update::Append { id, text } => {
                    if let Some(old) = shown.iter_mut().find(|old| old.id == id) {
                        old.text.push_str(&text);
                    }
                }
                Update::Remove { id } => shown.retain(|old| old.id != id),
                _ => {}
            }
        }
        assert_eq!(
            shown
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            ["Before. Fixed."]
        );
    }

    /// A call names its tool and keeps its parameters and whole result for
    /// an app's view of it, out of what the web page gets. The latest
    /// calls keep theirs.
    #[test]
    fn calls_keep_their_parameters_and_results_for_apps() {
        let feed = Feed::new("main");
        let (_, mut receiver) = feed.subscribe();
        let edit = |id: Uuid, new: &str| {
            let parameters = [
                ("path", "src/a.rs"),
                ("old_string", "one"),
                ("new_string", new),
            ]
            .map(|(name, value)| (name.to_string(), value.into()));
            feed.event(
                &event(
                    "main",
                    AgentEvent::ToolCallFormatted {
                        tool_call_id: id,
                        tool_name: EDIT_FILE_TOOL_NAME.into(),
                        parameters: parameters.into(),
                    },
                ),
                None,
            );
            let output = format!("Edited src/a.rs at lines 1-1:\n1 | {new}");
            let result = AgentEvent::ToolResult {
                tool_call_id: id,
                tool_name: EDIT_FILE_TOOL_NAME.into(),
                output,
            };
            feed.event(&event("main", result), None);
        };
        edit(Uuid::new_v4(), "two");
        let call = items(&feed).pop().unwrap();
        assert_eq!(call.tool.as_deref(), Some(EDIT_FILE_TOOL_NAME));
        assert_eq!(
            call.parameters.as_deref(),
            Some(
                "{\n  \"new_string\": \"two\",\n  \"old_string\": \"one\",\n  \"path\": \"src/a.rs\"\n}"
            )
        );
        assert_eq!(
            call.output.as_deref(),
            Some("Edited src/a.rs at lines 1-1:\n1 | two")
        );
        assert_eq!(call.outcome.as_deref(), Some("1 edit"));
        let updates = updates(&mut receiver);
        assert!(matches!(&updates[0], Update::Item(item)
            if item.parameters.is_some() && item.output.is_none()));
        assert!(matches!(&updates[1], Update::Change(item)
            if item.output.as_deref() == call.output.as_deref()));
        let json = serde_json::to_value(&call).unwrap();
        for part in ["tool", "parameters", "output"] {
            assert!(json.get(part).is_none(), "{json}");
        }
        // A command's output is its block's: the call has none.
        let command = Uuid::new_v4();
        feed.event(&event("main", shell(command, "true")), None);
        feed.event(
            &event("main", result(command, "exit code 0, 0.1 s\nok")),
            None,
        );
        let kept = items(&feed);
        let call = kept.iter().rfind(|item| item.kind == Kind::Call).unwrap();
        assert_eq!(call.tool.as_deref(), Some(SHELL_TOOL_NAME));
        assert_eq!(call.output, None);
        // A call that failed keeps why.
        let refused = Uuid::new_v4();
        feed.event(&event("main", shell(refused, "rm -rf /")), None);
        let rejection = AgentEvent::ToolRejected {
            tool_call_id: refused,
            tool_name: SHELL_TOOL_NAME.into(),
            reason: Some("Not that.".into()),
        };
        feed.event(&event("main", rejection), None);
        let call = items(&feed).pop().unwrap();
        assert_eq!(call.output.as_deref(), Some("refused (Not that.)"));
        // About a quarter of the budget each: the oldest calls lose theirs.
        let long = "x".repeat(MAIN_BUDGET.wholes / 4);
        for _ in 0..6 {
            edit(Uuid::new_v4(), &long);
        }
        let kept: Vec<usize> = items(&feed)
            .iter()
            .filter(|item| item.tool.as_deref() == Some(EDIT_FILE_TOOL_NAME))
            .map(Item::kept_size)
            .collect();
        assert_eq!(kept.len(), 7, "the calls stay");
        assert!(kept.iter().sum::<usize>() <= MAIN_BUDGET.wholes);
        assert_eq!(kept[0], 0);
        assert!(kept[6] > 0);
    }

    /// Each transcript keeps to its budget: the workers' items don't push
    /// the main agent's out.
    #[test]
    fn the_history_is_bounded() {
        let feed = Feed::new("main");
        for n in 0..MAIN_BUDGET.items + 10 {
            feed.notice(Level::Info, &format!("notice {n}"));
        }
        let kept = items(&feed);
        assert_eq!(kept.len(), MAIN_BUDGET.items);
        assert_eq!(kept[0].text, "notice 10");
        for n in 0..WORKER_BUDGET.items + 10 {
            feed.event(
                &event("worker", shell(Uuid::new_v4(), &format!("echo {n}"))),
                Some("r1"),
            );
        }
        let kept = items(&feed);
        let main = kept.iter().filter(|item| item.is_main()).count();
        assert_eq!(main, MAIN_BUDGET.items, "the main transcript is whole");
        assert_eq!(kept.len() - main, WORKER_BUDGET.items);
        assert!(kept.windows(2).all(|pair| pair[0].id < pair[1].id));
        let big = "x".repeat(MAIN_BUDGET.bytes / 4);
        for _ in 0..6 {
            feed.output(&big);
        }
        let main: usize = items(&feed)
            .iter()
            .filter(|item| item.is_main())
            .map(Item::size)
            .sum();
        assert!(main <= MAIN_BUDGET.bytes);
    }
}

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

// The types the web page reads too, which it compiles from the same file.
pub use super::protocol::{
    Activity, CallStatus, Delivery, JobKind, JobState, JobSummary, Kind, Level, Question, Source,
    WorkerState,
};

/// The message that keeps the prompt cache warm where the provider takes
/// no refresh out of band.
pub const REFRESH_MESSAGE: &str = "(Arko sends this message only to keep the prompt cache warm. Reply with \"Acknowledged\" and nothing else.)";

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
    /// For a `start_agent` call that started its worker at once, the
    /// worker's ID. A worker that waits for a place, a refused start and a
    /// call without its result yet have none. The quiet view leaves out the
    /// calls that have it, because the worker's own line stands for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
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
    /// Who sent a `message` the worker got, [`MAIN_AGENT`] (its lead) or
    /// another worker's ID. None for an agent's message to the user.
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
    /// Whether it is the line that opens a turn Arko started without a
    /// message from the user, naming who and what the turn is for (a worker's
    /// report, a job that finished). Clients can tell it from Arko's other
    /// notices, which say how the turn goes.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub opens_turn: bool,
    /// A question card's questions (`Kind::Question`), each with its id
    /// and what became of it, updated as that changes ([`Update::Change`]).
    /// `None` for a question asked before questions had ids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub questions: Option<Vec<crate::questions::AgentQuestion>>,
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
            started: None,
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
            opens_turn: false,
            questions: None,
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

    /// Marks it as the line that opens a turn Arko started.
    pub fn opens_turn(mut self) -> Self {
        self.opens_turn = true;
        self
    }

    /// Whether it is the main transcript's (or a worker's).
    pub fn is_main(&self) -> bool {
        self.agent == MAIN_AGENT
    }

    /// Whether it is an agent's message to the user (`message_user`). A
    /// worker's shows in its own transcript and in the main one, which
    /// names the worker. (A message a worker got has its sender in `from`.)
    pub fn is_to_user(&self) -> bool {
        self.kind == Kind::Message && self.from.is_none()
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
    /// The skills it is narrowed to, when it is (a worker that has what its
    /// starter has says nothing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// The MCP servers it is narrowed to, in the same way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<Vec<String>>,
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
    /// Once it ended, when its prompt cache expires or expired (Unix
    /// milliseconds): its last request plus the cache lifetime its run
    /// used, when that is known. Continuing it after then re-reads its
    /// conversation without the cache (see [`crate::worker_cache`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_expires_at: Option<i64>,
    /// What continuing it re-reads: its conversation's input at its last
    /// request, in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reread_tokens: Option<u64>,
    /// What re-reading them and writing them to the cache again costs at
    /// its model's price, in dollars, when it is billed per token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reread_dollars: Option<f64>,
    /// The plan that pays for that instead ("your Claude plan").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reread_plan: Option<String>,
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
            skills: None,
            mcp: None,
            parent: None,
            depth: 1,
            nested: false,
            cost_billed: 0.0,
            cost_list: 0.0,
            family_cost_billed: 0.0,
            family_cost_list: 0.0,
            cache_expires_at: None,
            reread_tokens: None,
            reread_dollars: None,
            reread_plan: None,
        }
    }

    /// Its prompt cache, once it ended, and what continuing it costs, as
    /// [`cache_expires_at`](Self::cache_expires_at) and the re-read fields
    /// say. `None` while it runs, and when they aren't known.
    pub fn cache(&self) -> Option<crate::worker_cache::WorkerCache> {
        if self.state.is_active() {
            return None;
        }
        Some(crate::worker_cache::WorkerCache {
            worker: self.id.clone(),
            expires_at: chrono::DateTime::from_timestamp_millis(self.cache_expires_at?)?,
            cost: crate::details::SwitchCost {
                tokens: usize::try_from(self.reread_tokens?).ok()?,
                dollars: self.reread_dollars,
                plan: match self.reread_plan.as_deref() {
                    Some("your Claude plan") => Some("your Claude plan"),
                    Some("your ChatGPT plan") => Some("your ChatGPT plan"),
                    _ => None,
                },
            },
        })
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
    /// The skills and MCP servers it is narrowed to, when it is.
    pub skills: Option<Vec<String>>,
    pub mcp: Option<Vec<String>>,
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
pub(crate) const ENDED_JOBS: usize = 100;

/// What changed, for the clients connected.
///
/// Each kind is one of three classes, which its documentation names.
///
/// - A state update is a change to what the [`Snapshot`] holds. A client
///   that applies it and one that takes a new snapshot end in the same
///   place, so a client that falls behind the broadcast ([`Feed::follow`])
///   starts over from a snapshot and loses nothing. Most carry their change.
///   Two are latest-value, where a client keeps only the newest.
///   [`Update::Worker`] carries a whole summary, sent a few times a second
///   at most, and [`Update::Activity`] carries nothing, as the client reads
///   [`Feed::activity`] when it comes.
/// - An edge is news, for a client that reacts once (a notification), that
///   something the snapshot holds changed. [`Feed::subscribe_batched`]
///   delivers it exactly once. A broadcast client that starts over compares
///   what it had with the snapshot instead.
/// - A per-client event is what one client asked for, such as a
///   recording's words, and never comes here. It goes on that client's own
///   channel.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Update {
    /// A new item. State, in [`Snapshot::items`].
    Item(Item),
    /// More text for the item `id`. State, in [`Snapshot::items`].
    Append { id: u64, text: String },
    /// The item with this one's `id` is now this one (a call's result
    /// came). State, in [`Snapshot::items`].
    Change(Item),
    /// A worker's summary, whole. State, latest-value, in
    /// [`Snapshot::workers`], which may be up to a quarter of a second newer
    /// than the latest of these until the next one comes.
    Worker(WorkerSummary),
    /// The worker `id` ended as `outcome` (finished, failed or stopped).
    /// Its [`Update::Worker`] with that state, its `report` or `error`
    /// comes right before, and before it in a merged batch. Edge, of the
    /// state in [`Snapshot::workers`].
    WorkerEnded { id: String, outcome: WorkerState },
    /// The activity changed, or the approval tray did, which a client reads
    /// in the session's status. State, latest-value, in
    /// [`Snapshot::activity`].
    Activity,
    /// The main agent's items from `from` on are gone (the conversation was
    /// cut back to an earlier message), and clients drop them too. State,
    /// in [`Snapshot::items`].
    Truncate { from: u64 },
    /// The item `id` is gone. It held text a model's response streamed
    /// before it failed, which its retry streams again, or a note the agent
    /// read, which comes again where it read it. State, in
    /// [`Snapshot::items`].
    Remove { id: u64 },
    /// The whole summary of a background job that started, changed or
    /// ended. State, in [`Snapshot::jobs`].
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

/// A broadcast client's updates, from [`Feed::follow`]. The broadcast keeps
/// a few thousand, and a client that falls further behind gets a new
/// snapshot to start over from in place of those it missed, whether it
/// waits for the next update or takes those that wait.
pub struct Following {
    feed: Feed,
    updates: broadcast::Receiver<Update>,
}

/// What a [`Following`] client gets next.
// (It is as large as an update, which comes by value anyway.)
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Received {
    /// The next update.
    Update(Update),
    /// The client fell behind, so it starts over from this snapshot, and
    /// the updates after it follow.
    Reset(Snapshot),
}

impl Following {
    /// The next update, after waiting for it, or a snapshot to start over
    /// from.
    pub async fn next(&mut self) -> Received {
        match self.updates.recv().await {
            Ok(update) => Received::Update(update),
            Err(broadcast::error::RecvError::Lagged(_)) => Received::Reset(self.restart()),
            // (It holds the feed, which holds the sender.)
            Err(broadcast::error::RecvError::Closed) => unreachable!("the feed is gone"),
        }
    }

    /// The next update if one waits, or a snapshot to start over from.
    pub fn try_next(&mut self) -> Option<Received> {
        match self.updates.try_recv() {
            Ok(update) => Some(Received::Update(update)),
            Err(broadcast::error::TryRecvError::Lagged(_)) => Some(Received::Reset(self.restart())),
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                None
            }
        }
    }

    /// How many updates wait.
    pub fn waiting(&self) -> usize {
        self.updates.len()
    }

    /// Starts over now: a new snapshot, which the updates after it follow.
    pub fn restart(&mut self) -> Snapshot {
        let (snapshot, updates) = self.feed.subscribe();
        self.updates = updates;
        snapshot
    }
}

/// `updates`, merged into fewer that leave a client where they all would:
/// text appended to an item goes into the item when it came in the same
/// batch (or into the batch's one append to it), an item's change replaces
/// what the batch had of it (it is the whole item), a worker's or a job's
/// summary only its last, in the place of its first (but the one a worker
/// ended with stays), and the activity is told once.
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
            // In the place of the batch's first summary of the worker, so
            // the workers that are new to the client come in the order they
            // started, as a snapshot has them.
            Update::Worker(summary) => match workers.get(&summary.id) {
                Some(&at) => out[at] = Some(Update::Worker(summary)),
                None => {
                    workers.insert(summary.id.clone(), out.len());
                    out.push(Some(Update::Worker(summary)));
                }
            },
            Update::WorkerEnded { id, outcome } => {
                // The summary it ended with stays, also if the worker goes
                // on in the same batch.
                workers.remove(&id);
                out.push(Some(Update::WorkerEnded { id, outcome }));
            }
            // In the place of its first, as a worker's summary.
            Update::Job(summary) => match jobs.get(&summary.id) {
                Some(&at) => out[at] = Some(Update::Job(summary)),
                None => {
                    jobs.insert(summary.id.clone(), out.len());
                    out.push(Some(Update::Job(summary)));
                }
            },
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
///
/// It holds what the feed keeps. The oldest items, the oldest items' whole
/// outputs and the oldest ended jobs go, without an update, to keep it
/// within a budget, so a client that followed every update may hold more
/// than a snapshot does.
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
    /// The feed it is of ([`Feed::id`]). A feed made again, as when Arko
    /// starts again and resumes the session, has another, though its items'
    /// IDs may be the same.
    pub feed: u64,
    /// How many updates the feed had sent when it was taken. Those a client
    /// gets after it follow on from there, so applied to the snapshot they
    /// leave it where a client that followed every update would be.
    pub seq: u64,
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
    /// Tells this feed from any other ([`Feed::id`]).
    id: u64,
    state: Mutex<State>,
    updates: broadcast::Sender<Update>,
    /// The batched clients' queues ([`Feed::subscribe_batched`]).
    batched: Mutex<Vec<mpsc::UnboundedSender<Update>>>,
    /// What a test runs where a change waits to go out, with the state
    /// locked ([`Feed::paused`]).
    #[cfg(test)]
    pause: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    clock: Clock,
}

/// The time by which the feed holds a worker's summary back, and the
/// timer that sends it later: the system's, or one a test moves.
enum Clock {
    System,
    #[cfg(any(test, feature = "testing"))]
    Manual(Arc<ManualClock>),
}

impl Clock {
    fn now(&self) -> Instant {
        match self {
            Clock::System => Instant::now(),
            #[cfg(any(test, feature = "testing"))]
            Clock::Manual(clock) => clock.lock().now,
        }
    }

    /// Runs `due` at `at`. False when it can't wait, with no runtime to
    /// wait on, and `due` never runs.
    fn at(&self, at: Instant, due: impl FnOnce() + Send + 'static) -> bool {
        match self {
            Clock::System => {
                let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                    return false;
                };
                runtime.spawn(async move {
                    tokio::time::sleep_until(at.into()).await;
                    due();
                });
                true
            }
            #[cfg(any(test, feature = "testing"))]
            Clock::Manual(clock) => {
                clock.lock().waiting.push((at, Box::new(due)));
                true
            }
        }
    }
}

/// A feed's clock that moves only when a test moves it, and runs what
/// waits for it then ([`Feed::with_clock`]).
#[cfg(any(test, feature = "testing"))]
pub(crate) struct ManualClock(Mutex<ManualTime>);

#[cfg(any(test, feature = "testing"))]
struct ManualTime {
    now: Instant,
    waiting: Vec<(Instant, Box<dyn FnOnce() + Send>)>,
}

#[cfg(any(test, feature = "testing"))]
impl ManualClock {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(ManualTime {
            now: Instant::now(),
            waiting: Vec::new(),
        })))
    }

    fn lock(&self) -> MutexGuard<'_, ManualTime> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Moves it on by `by`, running what waits until then at its own time,
    /// the earliest first.
    pub(crate) fn advance(&self, by: Duration) {
        let until = self.lock().now + by;
        loop {
            let due = {
                let mut time = self.lock();
                let next = time
                    .waiting
                    .iter()
                    .enumerate()
                    .filter(|(_, (at, _))| *at <= until)
                    .min_by_key(|(_, (at, _))| *at)
                    .map(|(next, _)| next);
                next.map(|next| {
                    let (at, due) = time.waiting.remove(next);
                    time.now = time.now.max(at);
                    due
                })
            };
            // (Run with the clock unlocked, as it locks the feed.)
            match due {
                Some(due) => due(),
                None => break,
            }
        }
        self.lock().now = until;
    }

    /// How many wait for it.
    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.lock().waiting.len()
    }
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
    /// The main agent's questions.
    questions: crate::questions::Book,
    /// The approval tray's requests.
    approvals: crate::approvals::Queue,
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
    /// The text of each agent's latest `ServiceWarning`, which the error that
    /// follows it doesn't repeat.
    warned: HashMap<String, String>,
    /// The notice that the turn is stopping, until it has stopped.
    stopping: Option<u64>,
    /// How many updates have gone out ([`Feed::publish`]).
    seq: u64,
}

impl State {
    /// The question card the transcript shows for all of `asked`, run
    /// `run`'s questions, if they share one.
    fn card_shown(&self, run: Uuid, asked: &[crate::questions::AskedQuestion]) -> Option<u64> {
        let item = self.questions.card_of(run, asked)?;
        let card = self.main.items.get(self.main.index(item)?)?;
        let shown = card.questions.as_ref()?;
        asked
            .iter()
            .all(|asked| shown.iter().any(|question| question.id == asked.id))
            .then_some(item)
    }

    /// `message` without the text of `agent`'s latest warning.
    fn unrepeated(&self, agent: &str, message: &str) -> String {
        match self.warned.get(agent) {
            Some(warning) => crate::ui::without_warning(message, warning),
            None => message.to_string(),
        }
    }

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

/// Puts `cache` (a worker's prompt cache once it ended, or none) in its
/// `summary`. Whether that changed it.
fn set_cache(
    summary: &mut WorkerSummary,
    cache: Option<&crate::worker_cache::WorkerCache>,
) -> bool {
    let before = (
        summary.cache_expires_at,
        summary.reread_tokens,
        summary.reread_dollars,
        summary.reread_plan.clone(),
    );
    summary.cache_expires_at = cache.map(|cache| cache.expires_at.timestamp_millis());
    summary.reread_tokens = cache.map(|cache| cache.cost.tokens as u64);
    summary.reread_dollars = cache.and_then(|cache| cache.cost.dollars);
    summary.reread_plan = cache.and_then(|cache| cache.cost.plan).map(str::to_string);
    before
        != (
            summary.cache_expires_at,
            summary.reread_tokens,
            summary.reread_dollars,
            summary.reread_plan.clone(),
        )
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
        Self::on(main, Clock::System)
    }

    /// A feed whose worker summaries go out by `clock`, which a test moves.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn with_clock(main: &str, clock: Arc<ManualClock>) -> Self {
        Self::on(main, Clock::Manual(clock))
    }

    fn on(main: &str, clock: Clock) -> Self {
        let (updates, _) = broadcast::channel(BACKLOG);
        Self {
            inner: Arc::new(Inner {
                main: main.to_string(),
                id: Uuid::new_v4().as_u64_pair().0,
                state: Mutex::new(State::default()),
                updates,
                batched: Mutex::default(),
                #[cfg(test)]
                pause: Mutex::default(),
                clock,
            }),
        }
    }

    /// Runs what a test set to run between a change and its update.
    #[cfg(test)]
    fn paused(&self) {
        let pause = self
            .inner
            .pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(pause) = pause {
            pause();
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The history, the workers and the activity now, and the updates
    /// from then on. The broadcast drops what a client leaves unread too
    /// long, so a client that keeps what the updates hold follows the feed
    /// instead ([`follow`](Self::follow)), which starts it over then. This
    /// is for one that only wakes when something changed.
    pub fn subscribe(&self) -> (Snapshot, broadcast::Receiver<Update>) {
        let state = self.lock();
        (self.snapshot(&state), self.inner.updates.subscribe())
    }

    /// The history, the workers and the activity now, and the updates from
    /// then on, through the broadcast. A client that falls behind gets a new
    /// snapshot in place of what it missed ([`Received::Reset`]).
    pub fn follow(&self) -> (Snapshot, Following) {
        let (snapshot, updates) = self.subscribe();
        let following = Following {
            feed: self.clone(),
            updates,
        };
        (snapshot, following)
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
        (self.snapshot(&state), Batches { updates })
    }

    /// A number that tells this feed from any other, made at random as it
    /// is made. Each snapshot carries it ([`Snapshot::feed`]), so a client
    /// that connects again can tell whether it is the feed it had.
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    fn snapshot(&self, state: &State) -> Snapshot {
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
            feed: self.inner.id,
            seq: state.seq,
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

    /// How many calls each worker has made, by its ID, to keep with the
    /// session beside [`worker_run_times`](Self::worker_run_times).
    pub(crate) fn worker_tool_calls(&self) -> Vec<(String, u32)> {
        self.lock()
            .workers
            .iter()
            .map(|worker| (worker.summary.id.clone(), worker.summary.tool_calls))
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

    /// Sends `update` to the clients. It takes the state locked, so an
    /// update always goes out with the change it reports and in the order
    /// of the changes, and a client that subscribes gets each change once,
    /// in its snapshot or as an update. It counts the update in `seq`.
    fn publish(&self, state: &mut State, update: Update) {
        state.seq += 1;
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
        self.publish(state, Update::Item(item));
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
            self.publish(
                state,
                Update::Append {
                    id,
                    text: chunk.to_string(),
                },
            );
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
            item.started = outcome.started;
            if let Some(job) = outcome.job {
                item.job = Some(job.to_string());
            }
            let item = item.clone();
            self.publish(state, Update::Change(item));
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
            self.publish(state, Update::Job(summary));
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
            self.publish(state, Update::Change(item));
        }
        for id in gone {
            self.publish(state, Update::Remove { id });
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
                    started: None,
                },
            );
        }
    }

    fn set_activity(&self, change: impl FnOnce(&mut Activity)) {
        let mut state = self.lock();
        let before = state.activity.clone();
        change(&mut state.activity);
        if state.activity != before {
            self.publish(&mut state, Update::Activity);
        }
    }

    /// A workflow's event. `label` names the worker whose run sent it (the
    /// runs of other agents than the main one are workers too).
    pub fn event(&self, event: &WorkflowEvent, label: Option<&str>) {
        // The main agent asked. The questions its task recorded get their
        // card as the ask's result comes, in the transcript's order, also
        // when the agent waits for them only once its turn ends.
        if let AgentEvent::ToolResult {
            tool_name, output, ..
        } = &event.event
            && tool_name == arko_agents::builtins::ASK_USER_TOOL_NAME
            && label.is_none()
            && event.agent_name == self.inner.main
        {
            self.card_recorded(event.agent_run_id, &crate::questions::ids_in_result(output));
        }
        // The main agent closed questions: their cards say so.
        if let AgentEvent::QuestionsClosed { ids, why } = &event.event {
            if label.is_none() && event.agent_name == self.inner.main {
                {
                    let mut state = self.lock();
                    let items = state.questions.close(event.agent_run_id, ids, why.clone());
                    state.questions.save();
                    for item in items {
                        self.refresh_card(&mut state, item);
                    }
                }
                self.question_activity();
            }
            return;
        }
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
                    let message = state.unrepeated(&agent, &message);
                    if !message.is_empty() {
                        let item = notice(render::cut(&message, 600), Level::Error);
                        self.push(&mut state, item);
                    }
                }
            },
            // Claude Code's own message about a failed request: one line, in
            // the warning colour, which the error after it doesn't repeat.
            AgentEvent::ServiceWarning { text } => {
                state.warned.insert(agent.clone(), text.clone());
                self.push(&mut state, notice(render::cut(text, 600), Level::Warn));
            }
            // The conversation's own requests (not Claude Code's subagents').
            AgentEvent::ModelRequestStarted { subagent: None, .. } => {
                // A failure's warning and its error come before the next request.
                state.warned.remove(&agent);
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
                let why = state.unrepeated(&agent, error_message);
                let text = match why.is_empty() {
                    true => "usage limit reached".to_string(),
                    false => format!("usage limit reached ({})", render::cut(&why, 300)),
                };
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
        self.publish(state, Update::Remove { id });
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
        self.publish(state, Update::Change(call));
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
                    self.publish(&mut state, Update::Change(found));
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
        self.publish(&mut state, Update::Job(summary));
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
        self.publish(&mut state, Update::Job(job.clone()));
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
        self.publish(&mut state, Update::Job(job.clone()));
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
        self.publish(state, Update::Change(call));
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
        self.publish(state, Update::Change(call));
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
            self.publish(&mut state, Update::Change(item));
        }
    }

    /// The user's message `id` is the main agent's message `index`: it can
    /// be edited (the clients get the change).
    pub(crate) fn mark_message(&self, id: u64, index: usize) {
        let mut state = self.lock();
        if let Some(item) = state.main.find(id) {
            item.message_index = Some(index);
            let item = item.clone();
            self.publish(&mut state, Update::Change(item));
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
            // The line of a worker the session has no record of would say
            // nothing, so its call shows (the quiet view leaves out the
            // calls that started a worker).
            if item.kind == Kind::WorkerLine
                && item
                    .worker
                    .as_deref()
                    .is_some_and(|worker| state.worker(worker).is_none())
            {
                continue;
            }
            if let Some(worker) = &item.started
                && state.worker(worker).is_none()
            {
                item.started = None;
            }
            item.id += base;
            item.call = item.call.map(|call| call + base);
            item.turn = item.turn.map(|turn| turn + turns);
            item.at = at;
            // A card's questions as the session kept them.
            if let Some(questions) = item.questions.take() {
                let ids = questions
                    .into_iter()
                    .map(|question| question.id)
                    .collect::<Vec<_>>();
                // (Questions the session no longer keeps show as text.)
                let kept = state.questions.restored(&ids, item.id);
                if !kept.is_empty() {
                    item.text = crate::questions::card_line(&kept);
                    item.questions = Some(kept);
                }
            }
            state.next_id = state.next_id.max(item.id);
            state.turn = state.turn.max(item.turn);
            let (lane, budget) = state.lane(&item.agent);
            lane.push(item.clone(), budget);
            self.publish(&mut state, Update::Item(item));
        }
        drop(state);
        self.question_activity();
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
        let forgotten: Vec<Item> = state
            .main
            .items
            .iter_mut()
            .filter_map(|item| item.message_index.take().map(|_| item.clone()))
            .collect();
        for item in forgotten {
            self.publish(state, Update::Change(item));
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
        self.publish(&mut state, Update::Truncate { from });
    }

    /// `message` without the text of the main agent's latest warning (an
    /// error that carries it).
    pub(crate) fn unrepeated(&self, message: &str) -> String {
        self.lock().unrepeated(MAIN_AGENT, message)
    }

    /// What Arko says about the session.
    pub fn notice(&self, level: Level, text: &str) {
        let text = text.trim();
        if !text.is_empty() {
            self.add(Item::new(Kind::Notice, text).level(level));
        }
    }

    /// The line that opens a turn Arko started without a message from the
    /// user, naming who and what it is for.
    pub fn turn_opening(&self, text: &str) {
        let text = text.trim();
        if !text.is_empty() {
            self.add(
                Item::new(Kind::Notice, text)
                    .level(Level::Info)
                    .opens_turn(),
            );
        }
    }

    /// The user stopped the main agent's turn: a warning that says so,
    /// which [`turn_stopped`](Self::turn_stopped) changes in its place.
    pub fn turn_stopping(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let mut state = self.lock();
        let item = Item::new(Kind::Notice, text).level(Level::Warn);
        // The model may still send some of its response: it goes on in the
        // text it was in, above this line, as the turn ends.
        let open = state.open_text.get(&item.agent).copied();
        let agent = item.agent.clone();
        let id = self.push(&mut state, item);
        if let Some(open) = open {
            state.open_text.insert(agent, open);
        }
        state.stopping = Some(id);
    }

    /// The turn stopped: the warning that it was stopping says `text` now,
    /// in its place, or a new one does once it's gone.
    pub fn turn_stopped(&self, text: &str) {
        let text = text.trim();
        let mut state = self.lock();
        if let Some(id) = state.stopping.take()
            && let Some(item) = state.main.find(id)
        {
            let before = item.size();
            item.text = text.to_string();
            let item = item.clone();
            state.main.bytes = (state.main.bytes + item.size()).saturating_sub(before);
            self.publish(&mut state, Update::Change(item));
            return;
        }
        self.push(&mut state, Item::new(Kind::Notice, text).level(Level::Warn));
    }

    /// Whether the user stopped the turn and it hasn't ended yet.
    pub(crate) fn turn_is_stopping(&self) -> bool {
        self.lock().stopping.is_some()
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
            self.publish(&mut state, Update::Change(item));
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
            state.warned.remove(MAIN_AGENT);
            state.stopping = None;
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

    /// The main agent works on a report for a new conversation (`true`), or
    /// it is done: the clients show it working meanwhile, and their stop
    /// skips it.
    pub(crate) fn reporting(&self, working: bool) {
        self.set_activity(|activity| {
            activity.working = working;
            activity.reporting = working;
            activity.since = working.then(now_ms);
        });
        if !working {
            self.question_activity();
        }
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
        });
        self.question_activity();
    }

    // ----- questions -----

    /// Keeps the main agent's questions in `path` (in the session's
    /// folder), and reads those it kept.
    pub(crate) fn keep_questions(&self, path: std::path::PathBuf) {
        self.lock().questions.keep_in(path);
    }

    /// The main agent's conversation is now run `run`'s (`None`: a new one
    /// that hasn't started): its questions are the ones shown.
    pub(crate) fn questions_run(&self, run: Option<Uuid>) {
        {
            let mut state = self.lock();
            if state.questions.run() == run {
                return;
            }
            state.questions.set_run(run);
        }
        self.question_activity();
    }

    /// Run `run` of the main agent asks `asked` (`waits`: it waits for the
    /// answer). Their card goes in the transcript, and its item is
    /// returned. Questions that share a card already keep it.
    pub(crate) fn ask(
        &self,
        run: Uuid,
        asked: &[crate::questions::AskedQuestion],
        waits: bool,
    ) -> u64 {
        let item = {
            let mut state = self.lock();
            if state.questions.run().is_none() {
                state.questions.set_run(Some(run));
            }
            let questions: Vec<_> = asked
                .iter()
                .map(|asked| crate::questions::AgentQuestion::of(asked, waits, 0))
                .collect();
            // Questions that have their card already (their ask's result
            // gave them one before the wait came) keep it.
            let (item, key) = match state.card_shown(run, asked) {
                Some(item) => (item, None),
                None => (
                    self.push(
                        &mut state,
                        Item::new(Kind::Question, crate::questions::card_line(&questions)),
                    ),
                    Some(Uuid::new_v4()),
                ),
            };
            state.questions.ask(
                run,
                questions
                    .into_iter()
                    .map(|mut question| {
                        question.item = item;
                        question
                    })
                    .collect(),
                key,
            );
            state.questions.number_card(item);
            state.questions.save();
            self.refresh_card(&mut state, item);
            item
        };
        self.question_activity();
        item
    }

    /// The main agent asks `asked` (`waits`: it waits for the answer), in
    /// the current conversation, as when it calls `ask_user`: their card
    /// goes in the transcript, and its item is returned. For an app's
    /// tests.
    #[doc(hidden)]
    pub fn ask_questions(&self, asked: &[crate::questions::AskedQuestion], waits: bool) -> u64 {
        let run = self.lock().questions.run().unwrap_or_else(Uuid::nil);
        self.ask(run, asked, waits)
    }

    /// Sets the current conversation's question `id` to `state`, as an
    /// answer, a dismissal or the agent's closing does: its card and the
    /// activity follow. For an app's tests.
    #[doc(hidden)]
    pub fn set_question_state(&self, id: &str, state: crate::questions::QuestionState) {
        {
            let mut state_now = self.lock();
            if let Some(item) = state_now.questions.set_state(id, state) {
                state_now.questions.save();
                self.refresh_card(&mut state_now, item);
            }
        }
        self.question_activity();
    }

    /// The main agent's questions of the current conversation, in the
    /// order asked, whatever became of them.
    pub fn questions(&self) -> Vec<crate::questions::AgentQuestion> {
        self.lock().questions.current()
    }

    /// The current conversation's questions that wait for the user.
    pub fn open_questions(&self) -> Vec<crate::questions::AgentQuestion> {
        self.lock().questions.open()
    }

    /// Run `run`'s open questions.
    pub(crate) fn open_questions_of(&self, run: Uuid) -> Vec<crate::questions::AgentQuestion> {
        self.lock().questions.open_of(Some(run))
    }

    /// The conversation whose questions are shown.
    pub(crate) fn questions_conversation(&self) -> Option<Uuid> {
        self.lock().questions.run()
    }

    /// The number of the first of `count` questions the main agent asks
    /// now, saved before it is used, so no other ask gets it.
    pub(crate) fn reserve_question_numbers(&self, count: u32) -> u32 {
        let mut state = self.lock();
        let first = state.questions.reserve(count);
        state.questions.save();
        first
    }

    /// Run `run` asked `asked`, as its task records them. They are open at
    /// once, their card comes with the ask's result, and the agent waits for
    /// them once its wait comes.
    pub(crate) fn record_asked(&self, run: Uuid, asked: &[crate::questions::AskedQuestion]) {
        {
            let mut state = self.lock();
            state.questions.register(run, asked);
            state.questions.save();
        }
        self.question_activity();
    }

    /// The questions `ids` that run `run` recorded without a card get one,
    /// as their ask's result comes. (Those whose wait came first have it.)
    fn card_recorded(&self, run: Uuid, ids: &[String]) {
        let recorded = self.lock().questions.without_card(run, ids);
        if recorded.is_empty() {
            return;
        }
        let asked: Vec<_> = recorded
            .iter()
            .map(crate::questions::AgentQuestion::asked)
            .collect();
        self.ask(run, &asked, false);
    }

    /// Whether the numbers given out are to be counted from the tasks (an
    /// older or missing file).
    pub(crate) fn questions_need_count(&self) -> bool {
        self.lock().questions.needs_count()
    }

    /// The tasks gave out question numbers up to `last`.
    pub(crate) fn questions_counted(&self, last: u32) {
        let mut state = self.lock();
        state.questions.raise(last);
        state.questions.save();
    }

    /// The agent no longer waits on the questions `ids` (the turn that
    /// waited stopped): they stay open.
    pub(crate) fn questions_unwaited(&self, ids: &[String]) {
        {
            let mut state = self.lock();
            let items = state.questions.unwait(ids);
            state.questions.save();
            for item in items {
                self.refresh_card(&mut state, item);
            }
        }
        self.question_activity();
    }

    /// Run `run` asked `asked`, as its task says, though the session didn't
    /// keep them (its file was lost, or Arko stopped as they were asked):
    /// they are open again, without a card.
    pub(crate) fn questions_found(&self, run: Uuid, asked: &[crate::questions::AskedQuestion]) {
        {
            let mut state = self.lock();
            let missing: Vec<_> = asked
                .iter()
                .filter(|asked| !state.questions.knows(run, &asked.id))
                .map(|asked| crate::questions::AgentQuestion::of(asked, false, 0))
                .collect();
            if missing.is_empty() {
                return;
            }
            state.questions.ask(run, missing, None);
            state.questions.save();
        }
        self.question_activity();
    }

    /// The questions of the card `item`.
    pub(crate) fn card_questions(&self, item: u64) -> Vec<crate::questions::AgentQuestion> {
        self.lock().questions.of_item(item)
    }

    /// The open questions among `ids` (of run `run`, else of the current
    /// conversation) become what `to` gives each, and their cards change.
    /// Returns them as they were.
    pub(crate) fn settle_questions(
        &self,
        run: Option<Uuid>,
        ids: &[String],
        to: impl FnMut(&crate::questions::AgentQuestion) -> crate::questions::QuestionState,
    ) -> Vec<crate::questions::AgentQuestion> {
        self.settle_noting(run, ids, to, |_, _| None).0
    }

    /// [`settle_questions`](Self::settle_questions), and the note `note`
    /// writes of the questions it settled (and those still open), about
    /// some ids, which is kept for the agent with them (saved at once) until
    /// [`ack_notes`](Self::ack_notes).
    pub(crate) fn settle_noting(
        &self,
        run: Option<Uuid>,
        ids: &[String],
        to: impl FnMut(&crate::questions::AgentQuestion) -> crate::questions::QuestionState,
        note: impl FnOnce(
            &[crate::questions::AgentQuestion],
            &[crate::questions::AgentQuestion],
        ) -> Option<(Vec<String>, String)>,
    ) -> (
        Vec<crate::questions::AgentQuestion>,
        Option<crate::questions::Pending>,
    ) {
        let (settled, pending) = {
            let mut state = self.lock();
            let run = run.or(state.questions.run());
            let (settled, items) = state.questions.settle(run, ids, to);
            let open = state.questions.open_of(run);
            let pending = match (run, note(&settled, &open)) {
                (Some(run), Some((ids, note))) => Some(state.questions.queue(run, ids, note)),
                _ => None,
            };
            if !settled.is_empty() || pending.is_some() {
                state.questions.save();
            }
            for item in items {
                self.refresh_card(&mut state, item);
            }
            (settled, pending)
        };
        if !settled.is_empty() {
            self.question_activity();
        }
        (settled, pending)
    }

    /// Run `run`'s kept notes for its agent, oldest first.
    pub(crate) fn queued_notes(&self, run: Uuid) -> Vec<crate::questions::Pending> {
        self.lock().questions.queued(run)
    }

    /// Every kept note, oldest first.
    pub(crate) fn kept_notes(&self) -> Vec<crate::questions::Pending> {
        self.lock().questions.all_queued()
    }

    /// The kept note `id` travels as the workflow note `posted` (its id and
    /// number), or, with `None`, is to be posted again.
    pub(crate) fn note_posted(&self, id: Uuid, posted: Option<(Uuid, u64)>) {
        let mut state = self.lock();
        state.questions.posted(id, posted);
        state.questions.save();
    }

    /// The runs have been given the kept notes `ids`.
    pub(crate) fn ack_notes(&self, ids: &[Uuid]) {
        let mut state = self.lock();
        if state.questions.ack(ids) {
            state.questions.save();
        }
    }

    /// The card `item` shows its questions as they are now.
    fn refresh_card(&self, state: &mut State, item: u64) {
        let questions = state.questions.of_item(item);
        if let Some(card) = state.main.find(item) {
            card.text = crate::questions::card_line(&questions);
            card.questions = Some(questions);
            let card = card.clone();
            self.publish(state, Update::Change(card));
        }
    }

    /// The activity's question is the latest card with questions open that
    /// the agent waits on, with those questions as the user reads them.
    fn question_activity(&self) {
        let latest = {
            let state = self.lock();
            let open = state.questions.open();
            open.iter()
                .rev()
                .find(|question| question.item != 0 && question.waits)
                .map(|waited| Question {
                    id: waited.item,
                    text: open
                        .iter()
                        .filter(|question| question.item == waited.item)
                        .map(crate::questions::AgentQuestion::numbered)
                        .collect::<Vec<_>>()
                        .join("\n"),
                })
        };
        self.set_activity(|activity| activity.question = latest);
    }

    // ----- approvals -----

    /// Puts `approval` in the approval tray for run `run` (its call `call`),
    /// where it waits with the others: its id, and where its answer comes.
    /// See [`crate::approvals`].
    pub(crate) fn request_approval(
        &self,
        run: Uuid,
        call: Option<String>,
        approval: crate::approvals::Approval,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<crate::approvals::Answer>,
    ) {
        let mut state = self.lock();
        let added = state.approvals.add(approval);
        log::debug!("approval {} asked for run {run} (call {call:?})", added.0);
        #[cfg(test)]
        self.paused();
        self.publish(&mut state, Update::Activity);
        added
    }

    /// The request `id` leaves the tray unanswered (its tool stopped
    /// waiting).
    pub(crate) fn withdraw_approval(&self, id: &str) {
        let mut state = self.lock();
        if state.approvals.withdraw(id) {
            self.publish(&mut state, Update::Activity);
        }
    }

    /// The requests in the approval tray, in the order they came.
    pub fn approvals(&self) -> Vec<crate::approvals::ApprovalRequest> {
        self.lock().approvals.requests()
    }

    /// Gives the requests `answers` names their answers. Returns those
    /// answered, which leave the tray.
    pub(crate) fn answer_approvals(
        &self,
        answers: &crate::approvals::ApprovalAnswers,
    ) -> Vec<crate::approvals::ApprovalRequest> {
        let mut state = self.lock();
        let answered = state.approvals.answer(answers);
        if !answered.is_empty() {
            self.publish(&mut state, Update::Activity);
        }
        answered
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
            // The state's only reason text, so it keeps the warning's text.
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
        if let Some(outcome) = state
            .worker(id)
            .map(|worker| worker.summary.state)
            .filter(|state| !state.is_active())
        {
            self.publish(
                &mut state,
                Update::WorkerEnded {
                    id: id.to_string(),
                    outcome,
                },
            );
        }
    }

    /// A worker of the session from before Arko started (a resumed
    /// session). Its summary goes in without a place in the transcript,
    /// with how long it ran before (`ran_ms`) and the calls it made
    /// (`tool_calls`). A run it goes on with adds to both. `error` is the
    /// saved reason it ended, which a worker that failed keeps as its
    /// `error`.
    #[allow(clippy::too_many_arguments)]
    pub fn worker_restored(
        &self,
        id: &str,
        task: &str,
        was: WorkerState,
        report: Option<&str>,
        error: Option<&str>,
        since: i64,
        ran_ms: u64,
        tool_calls: u32,
    ) {
        self.worker_restored_with_cache(
            id, task, was, report, error, since, ran_ms, tool_calls, None,
        );
    }

    /// [`worker_restored`](Self::worker_restored), with the prompt cache it
    /// ended with in the same update, so no client sees it ended without
    /// its cache.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn worker_restored_with_cache(
        &self,
        id: &str,
        task: &str,
        was: WorkerState,
        report: Option<&str>,
        error: Option<&str>,
        since: i64,
        ran_ms: u64,
        tool_calls: u32,
        cache: Option<&crate::worker_cache::WorkerCache>,
    ) {
        let mut state = self.lock();
        if state.worker(id).is_some() {
            return;
        }
        let mut summary = WorkerSummary {
            title: title(task),
            state: was,
            started_at: was.is_active().then_some(since),
            ended_at: (!was.is_active()).then_some(since),
            report: report
                .map(str::trim)
                .filter(|report| !report.is_empty())
                .map(str::to_string),
            error: error
                .filter(|_| was == WorkerState::Failed)
                .map(str::to_string),
            ran_ms,
            tool_calls,
            ..WorkerSummary::new(id)
        };
        set_cache(&mut summary, cache);
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
        // (A worker heard of first here goes out whatever it started with.)
        let new = self.known_worker(&mut state, id);
        let parent = state.parents.get(id).cloned();
        let Some(worker) = state.worker(id) else {
            return;
        };
        let summary = &mut worker.summary;
        let before = summary.clone();
        summary.model = info.model;
        summary.effort = info.effort;
        summary.profile = info.profile;
        summary.skills = info.skills;
        summary.mcp = info.mcp;
        summary.parent = parent;
        summary.depth = info.depth.max(1);
        summary.nested = info.nested;
        if let Some((billed, list)) = info.costs {
            summary.cost_billed = billed;
            summary.cost_list = list;
        }
        if new || *summary != before {
            self.worker_changed(&mut state, id, new);
        }
        self.add_up_families(&mut state);
    }

    /// The prompt cache of worker `id` once it ended, and what continuing it
    /// costs (`None` while it runs again, or when that isn't known).
    pub(crate) fn worker_cache(&self, id: &str, cache: Option<&crate::worker_cache::WorkerCache>) {
        let mut state = self.lock();
        let Some(worker) = state.worker(id) else {
            return;
        };
        if set_cache(&mut worker.summary, cache) {
            self.worker_changed(&mut state, id, false);
        }
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

    /// The summary of the worker `handle` as the user may type it: `R3`
    /// finds `r3`, as `/message` does.
    pub fn worker_summary_named(&self, handle: &str) -> Option<WorkerSummary> {
        let handle = handle.trim();
        let state = self.lock();
        state
            .workers
            .iter()
            .find(|worker| worker.summary.id.eq_ignore_ascii_case(handle))
            .map(|worker| worker.summary.clone())
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
        let now = self.inner.clock.now();
        let Some(worker) = state.worker(id) else {
            return;
        };
        let due = worker.sent.is_none_or(|sent| now >= sent + WORKER_EVERY);
        if urgent || due {
            worker.sent = Some(now);
            worker.pending = false;
            let summary = worker.summary.clone();
            self.publish(state, Update::Worker(summary));
            return;
        }
        if worker.pending {
            return;
        }
        let at = worker.sent.map_or(now, |sent| sent + WORKER_EVERY);
        let feed: Weak<Inner> = Arc::downgrade(&self.inner);
        let owned = id.to_string();
        let waits = self.inner.clock.at(at, move || {
            if let Some(inner) = feed.upgrade() {
                Feed { inner }.send_pending(&owned);
            }
        });
        if waits {
            worker.pending = true;
            return;
        }
        // No timer to send it later with: it goes now.
        worker.sent = Some(now);
        let summary = worker.summary.clone();
        self.publish(state, Update::Worker(summary));
    }

    /// Sends the worker's summary if a change of it waits.
    fn send_pending(&self, id: &str) {
        let mut state = self.lock();
        if let Some(worker) = state.worker(id)
            && worker.pending
        {
            worker.pending = false;
            worker.sent = Some(self.inner.clock.now());
            let summary = worker.summary.clone();
            self.publish(&mut state, Update::Worker(summary));
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

    /// Workers new to a batched client come in the order they started, as a
    /// snapshot has them, though the first one's summary changed again
    /// later in the batch. So do jobs.
    #[test]
    fn a_batch_keeps_new_workers_and_jobs_in_the_order_they_started() {
        let feed = Feed::new("main");
        let (_, mut batches) = feed.subscribe_batched();
        feed.worker("r1", "One", WorkerChange::Started);
        feed.worker("r2", "Two", WorkerChange::Started);
        feed.worker("r1", "One", WorkerChange::Finished { report: "Done." });
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        feed.job_started(JobStart::new(a, MAIN_AGENT, JobKind::Command, "make"));
        feed.job_started(JobStart::new(b, MAIN_AGENT, JobKind::Command, "test"));
        feed.job_ended(a, JobState::Finished, None);
        let batch = batches.try_next().unwrap();
        let workers: Vec<(&str, WorkerState)> = batch
            .iter()
            .filter_map(|update| match update {
                Update::Worker(summary) => Some((summary.id.as_str(), summary.state)),
                _ => None,
            })
            .collect();
        assert_eq!(
            workers,
            [("r1", WorkerState::Finished), ("r2", WorkerState::Running)]
        );
        let jobs: Vec<(String, JobState)> = batch
            .iter()
            .filter_map(|update| match update {
                Update::Job(summary) => Some((summary.id.clone(), summary.state)),
                _ => None,
            })
            .collect();
        assert_eq!(
            jobs,
            [
                (a.to_string(), JobState::Finished),
                (b.to_string(), JobState::Running)
            ]
        );
        let snapshot = feed.subscribe().0;
        let order: Vec<&str> = snapshot.workers.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(order, ["r1", "r2"]);
    }

    /// A worker the feed first hears of in what it started with goes out at
    /// once, though that tells nothing new, as a snapshot has it.
    #[test]
    fn a_worker_first_heard_of_in_its_info_goes_out() {
        let feed = Feed::new("main");
        let (_, mut receiver) = feed.subscribe();
        feed.worker_info("r1", WorkerInfo::new());
        assert!(matches!(
            updates(&mut receiver).as_slice(),
            [Update::Worker(summary)] if summary.id == "r1"
        ));
        assert_eq!(feed.subscribe().0.workers.len(), 1);
    }

    /// A snapshot names its feed, which another feed doesn't share, and
    /// counts the updates sent before it. A client gets the updates sent
    /// after it, each once.
    #[test]
    fn a_snapshot_names_its_feed_and_counts_the_updates_before_it() {
        let feed = Feed::new("main");
        assert_ne!(feed.id(), Feed::new("main").id());
        let (empty, mut first) = feed.subscribe();
        assert_eq!((empty.feed, empty.seq), (feed.id(), 0));
        feed.event(&event("main", chunk("Hel")), None);
        feed.event(&event("main", chunk("lo")), None);
        let (snapshot, mut second) = feed.subscribe();
        assert_eq!(snapshot.seq, 2, "the item and its append");
        feed.notice(Level::Info, "Done.");
        assert_eq!(updates(&mut first).len(), 3);
        assert_eq!(updates(&mut second).len(), 1);
        assert_eq!(feed.subscribe().0.seq, 3);
    }

    /// A client that falls behind the broadcast gets a snapshot to start
    /// over from in place of what it missed, whether it waits for the next
    /// update or takes those that wait, and the updates after the snapshot
    /// follow.
    #[test]
    fn a_follower_that_falls_behind_gets_a_snapshot_in_place_of_what_it_missed() {
        use futures_util::FutureExt;
        let feed = Feed::new("main");
        let (_, mut waits) = feed.follow();
        let (_, mut takes) = feed.follow();
        feed.notice(Level::Info, "first");
        assert!(matches!(
            takes.try_next(),
            Some(Received::Update(Update::Item(item))) if item.text == "first"
        ));
        // More than the broadcast keeps, for both.
        for n in 0..=BACKLOG {
            feed.notice(Level::Info, &format!("note {n}"));
        }
        let Some(Received::Reset(taken)) = takes.try_next() else {
            panic!("behind")
        };
        let Some(Received::Reset(waited)) = waits.next().now_or_never() else {
            panic!("behind")
        };
        for snapshot in [&taken, &waited] {
            assert_eq!(snapshot.seq, BACKLOG as u64 + 2);
            assert_eq!(
                snapshot.items.last().unwrap().text,
                format!("note {BACKLOG}")
            );
        }
        assert!(takes.try_next().is_none() && waits.waiting() == 0);
        feed.notice(Level::Info, "after");
        for following in [&mut takes, &mut waits] {
            assert!(matches!(
                following.try_next(),
                Some(Received::Update(Update::Item(item))) if item.text == "after"
            ));
        }
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
            Update::Worker(worker),
            Update::Change(item),
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

    /// The images a call's tool returned show on its item, whether they came
    /// before the call's event or after its result, and a client that
    /// connects later gets them with the item.
    #[test]
    fn a_calls_images_show_on_its_item_whichever_comes_first() {
        use crate::images::ImageRef;
        let feed = Feed::new("main");
        let image = |id: u64| ImageRef {
            id,
            mime: "image/png".into(),
            label: format!("{id}.png"),
            bytes: 4,
        };
        let (early, late) = (Uuid::new_v4(), Uuid::new_v4());
        feed.call_images(early, vec![image(1)]);
        feed.event(&event("main", lookup(early)), None);
        feed.event(&event("main", lookup(late)), None);
        let result = AgentEvent::ToolResult {
            tool_call_id: late,
            tool_name: "mcp__search__lookup".into(),
            output: "found".into(),
        };
        feed.event(&event("main", result), None);
        let (_, mut updates) = feed.subscribe();
        feed.call_images(late, vec![image(2)]);
        assert!(
            matches!(updates.try_recv(), Ok(Update::Change(item)) if item.images == [image(2)])
        );
        let shown: Vec<Vec<ImageRef>> = feed
            .subscribe()
            .0
            .items
            .into_iter()
            .filter(|item| item.kind == Kind::Call)
            .map(|item| item.images)
            .collect();
        assert_eq!(shown, [vec![image(1)], vec![image(2)]]);
    }

    /// A `start_agent` call names its worker as started once its result says
    /// the worker began. Until then, and for a start that queued or was
    /// refused, it names none.
    #[test]
    fn a_start_call_names_the_worker_it_started() {
        let feed = Feed::new("main");
        let start = |feed: &Feed, task: &str| {
            let call = Uuid::new_v4();
            let parameters = [("task".to_string(), serde_json::json!(task))].into();
            feed.event(
                &event(
                    "main",
                    AgentEvent::ToolCallFormatted {
                        tool_call_id: call,
                        tool_name: crate::crew::START_AGENT.into(),
                        parameters,
                    },
                ),
                None,
            );
            call
        };
        let result = |feed: &Feed, call: Uuid, output: &str| {
            feed.event(
                &event(
                    "main",
                    AgentEvent::ToolResult {
                        tool_call_id: call,
                        tool_name: crate::crew::START_AGENT.into(),
                        output: output.into(),
                    },
                ),
                None,
            );
        };
        let started_of = |feed: &Feed, task: &str| call_item(feed, task).started;
        let first = start(&feed, "Fix the flaky test");
        assert_eq!(started_of(&feed, "Fix the flaky"), None, "still running");
        result(&feed, first, "Started r1 (agent `worker`).");
        let item = call_item(&feed, "Fix the flaky");
        assert_eq!(
            (item.started.as_deref(), item.status),
            (Some("r1"), Some(CallStatus::Ok))
        );
        let queued = start(&feed, "Review the docs");
        result(
            &feed,
            queued,
            "Queued r2 (agent `worker`): 4 agent runs are running, the most this workflow allows, so it starts when one ends.",
        );
        assert_eq!(started_of(&feed, "Review the docs"), None);
        let refused = start(&feed, "Run the migration");
        feed.event(
            &event(
                "main",
                AgentEvent::ErrorEncountered {
                    error_message: "The worker's billing needs your answer.".into(),
                    requires_approval_to_retry: false,
                    tool_call_id: Some(refused),
                    error_kind: None,
                },
            ),
            None,
        );
        let item = call_item(&feed, "Run the migration");
        assert_eq!(
            (item.started, item.status),
            (None, Some(CallStatus::Failed))
        );
        // A client that connects later gets it with the item.
        let sent = serde_json::to_value(call_item(&feed, "Fix the flaky")).unwrap();
        assert_eq!(sent["started"], "r1");
        let sent = serde_json::to_value(call_item(&feed, "Review the docs")).unwrap();
        assert!(sent.get("started").is_none());
    }

    /// A reopened session keeps the line of a worker it still has, and a
    /// call that started a worker it lost shows alone.
    #[test]
    fn a_reopened_session_keeps_the_lines_of_the_workers_it_has() {
        let feed = Feed::new("main");
        feed.worker_restored("r1", "Count", WorkerState::Finished, None, None, 5, 0, 0);
        let mut items_of_history = Vec::new();
        for (number, worker) in [(1, "r1"), (3, "r2")] {
            let mut call = Item::new(Kind::Call, "start worker");
            call.id = number;
            call.started = Some(worker.to_string());
            let mut line = Item::new(Kind::WorkerLine, worker);
            line.id = number + 1;
            line.worker = Some(worker.to_string());
            items_of_history.extend([call, line]);
        }
        feed.restore(items_of_history);
        let shown: Vec<(Kind, Option<String>, Option<String>)> = items(&feed)
            .into_iter()
            .map(|item| (item.kind, item.started, item.worker))
            .collect();
        assert_eq!(
            shown,
            [
                (Kind::Call, Some("r1".to_string()), None),
                (Kind::WorkerLine, None, Some("r1".to_string())),
                (Kind::Call, None, None),
            ]
        );
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
    /// and the call names it. A batch keeps a job's last summary, in the
    /// place of its first.
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
            matches!(merged.as_slice(), [Update::Job(last), Update::Activity] if last.state == JobState::Finished)
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

    /// Claude Code's message about a request that failed mid-response is one
    /// warning notice for the page, and the agent's text stays what it said
    /// before. The error that carries the same text adds no notice, and a
    /// usage limit announces itself without it.
    #[test]
    fn a_failed_requests_message_is_one_warning_and_not_reply_text() {
        let note = "API Error: Connection lost mid-response. The response above may be incomplete.";
        let limit = "You've hit your limit. It resets at 3pm.";
        let failed = |text: &str| AgentEvent::ErrorEncountered {
            error_message: text.into(),
            requires_approval_to_retry: true,
            tool_call_id: None,
            error_kind: None,
        };
        let feed = Feed::new("main");
        feed.turn_started();
        feed.event(&event("main", chunk("The answer is")), None);
        feed.event(
            &event("main", AgentEvent::ServiceWarning { text: note.into() }),
            None,
        );
        feed.event(&event("main", failed(note)), None);
        feed.event(
            &event("main", AgentEvent::ServiceWarning { text: limit.into() }),
            None,
        );
        feed.event(
            &event(
                "main",
                AgentEvent::UsageLimitReached {
                    resets_at_unix_ms: None,
                    waiting_until_unix_ms: None,
                    error_message: limit.into(),
                },
            ),
            None,
        );
        // Another failure's error keeps its text.
        feed.event(&event("main", failed("the model is overloaded")), None);
        let sent: Vec<serde_json::Value> = items(&feed)
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect();
        let shown: Vec<(&str, &str, &str)> = sent
            .iter()
            .map(|item| {
                let text = |name: &str| item[name].as_str().unwrap_or_default();
                (text("kind"), text("level"), text("text"))
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("text", "", "The answer is"),
                ("notice", "warn", note),
                ("notice", "warn", limit),
                ("notice", "warn", "usage limit reached"),
                ("notice", "error", "the model is overloaded"),
            ]
        );
    }

    /// A warning is remembered only until the agent's next request or the
    /// next turn, so an error with the same text later is shown. A worker's
    /// warning is an item of its own.
    #[test]
    fn an_errors_text_is_shown_again_after_the_next_request_or_turn() {
        let note = "API Error: Connection lost mid-response. The response above may be incomplete.";
        let warning = || AgentEvent::ServiceWarning { text: note.into() };
        let failed = || AgentEvent::ErrorEncountered {
            error_message: note.into(),
            requires_approval_to_retry: true,
            tool_call_id: None,
            error_kind: None,
        };
        let started = || AgentEvent::ModelRequestStarted {
            model: "claude-opus-5-5".into(),
            subagent: None,
            sizes: None,
            bytes_per_token: 3.0,
        };
        let feed = Feed::new("main");
        feed.turn_started();
        feed.event(&event("main", warning()), None);
        feed.event(&event("main", failed()), None);
        // The next request, then an error with the same text.
        feed.event(&event("main", started()), None);
        feed.event(&event("main", failed()), None);
        // The next turn, likewise.
        feed.event(&event("main", warning()), None);
        feed.turn_started();
        feed.event(&event("main", failed()), None);
        // A worker's warning and error are its own.
        feed.event(&event("w", warning()), Some("r1"));
        feed.event(&event("w", failed()), Some("r1"));
        let shown: Vec<(String, Option<Level>)> = items(&feed)
            .into_iter()
            .map(|item| (item.agent, item.level))
            .collect();
        assert_eq!(
            shown,
            [
                ("main".to_string(), Some(Level::Warn)),
                ("main".to_string(), Some(Level::Error)),
                ("main".to_string(), Some(Level::Warn)),
                ("main".to_string(), Some(Level::Error)),
                ("r1".to_string(), Some(Level::Warn)),
            ]
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

    /// The line that opens a turn Arko started is marked, so a client can
    /// tell it from Arko's other notices. The page's copy carries the mark
    /// only there.
    #[test]
    fn the_line_that_opens_a_turn_is_marked_and_other_notices_are_not() {
        let feed = Feed::new("main");
        feed.notice(Level::Info, "Resumed.");
        feed.turn_opening("  r1 reported");
        feed.turn_opening("  ");
        let marks: Vec<(String, bool)> = items(&feed)
            .into_iter()
            .map(|item| (item.text, item.opens_turn))
            .collect();
        assert_eq!(
            marks,
            [
                ("Resumed.".to_string(), false),
                ("r1 reported".to_string(), true)
            ]
        );
        let wire: Vec<serde_json::Value> = items(&feed)
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect();
        assert!(wire[0].get("opens_turn").is_none());
        assert_eq!(wire[1]["opens_turn"], true);
        assert_eq!(wire[1]["level"], "info");
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

    /// The warning that the turn is stopping says it stopped in its place,
    /// so a stopped turn shows one line. Without one (a new turn began), the
    /// stop is a line of its own.
    #[test]
    fn a_stopped_turn_says_so_where_it_said_it_was_stopping() {
        let feed = Feed::new("main");
        feed.turn_started();
        feed.turn_stopping("  Stopping the turn…");
        let (_, mut updates) = feed.subscribe();
        feed.notice(Level::Info, "Something between.");
        feed.turn_stopped("Stopped the turn.");
        let notices = |feed: &Feed| -> Vec<String> {
            let items = feed.subscribe().0.items;
            items
                .into_iter()
                .filter(|item| item.kind == Kind::Notice)
                .map(|item| item.text)
                .collect()
        };
        assert_eq!(notices(&feed), ["Stopped the turn.", "Something between."]);
        assert!(matches!(updates.try_recv(), Ok(Update::Item(_))));
        assert!(matches!(
            updates.try_recv(),
            Ok(Update::Change(item)) if item.text == "Stopped the turn." && item.level == Some(Level::Warn)
        ));
        feed.turn_started();
        feed.turn_stopped("Stopped the turn.");
        assert_eq!(
            notices(&feed),
            [
                "Stopped the turn.",
                "Something between.",
                "Stopped the turn."
            ]
        );
    }

    /// Text that streams in after the user stopped the turn still goes in
    /// the reply it belongs to, above the line that says the turn stopped.
    #[test]
    fn late_text_of_a_stopped_turn_joins_its_reply_above_the_stop() {
        let feed = Feed::new("main");
        feed.turn_started();
        feed.event(&event("main", chunk("Here it")), None);
        feed.turn_stopping("  Stopping the turn…");
        feed.event(&event("main", chunk(" is, so")), None);
        feed.turn_stopped("Stopped the turn.");
        let items = feed.subscribe().0.items;
        let shown: Vec<(Kind, &str)> = items
            .iter()
            .map(|item| (item.kind, item.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (Kind::Text, "Here it is, so"),
                (Kind::Notice, "Stopped the turn.")
            ]
        );
    }

    fn asked(id: &str, question: &str) -> crate::questions::AskedQuestion {
        crate::questions::AskedQuestion {
            id: id.into(),
            question: arko_agents::questions::Question {
                question: question.into(),
                default: Some("Spaces".into()),
                ..Default::default()
            },
        }
    }

    /// An ask's card holds its questions with their states, which change
    /// as the user answers, dismisses, or the agent closes them. A card the
    /// agent waits on is the activity's while it has questions open, and
    /// the open questions are the conversation's.
    #[test]
    fn question_cards_follow_each_questions_state() {
        use crate::questions::QuestionState;
        let feed = Feed::new("main");
        // The test events' run.
        let run = Uuid::nil();
        feed.questions_run(Some(run));
        feed.turn_started();
        let (_, mut updates) = feed.subscribe();
        let card = feed.ask(
            run,
            &[asked("q1", "Tabs or spaces?"), asked("q2", "Push now?")],
            true,
        );
        let since = feed.activity().since;
        assert_eq!(
            feed.activity(),
            Activity {
                reporting: false,
                working: true,
                since,
                question: Some(Question {
                    id: card,
                    text: "1. Tabs or spaces?\n2. Push now?".into()
                })
            }
        );
        // While they are open, the card is one line.
        assert_eq!(feed.main_item(card).unwrap().text, "Arko asked 2 questions");
        let card_now = |updates: &mut broadcast::Receiver<Update>| {
            let mut last = None;
            while let Ok(update) = updates.try_recv() {
                if let Update::Item(item) | Update::Change(item) = update
                    && item.id == card
                {
                    last = item.questions;
                }
            }
            last.unwrap()
        };
        assert!(card_now(&mut updates).iter().all(|q| q.state.is_open()));
        feed.settle_questions(None, &["q1".into()], |_| QuestionState::Answered {
            answer: "Spaces".into(),
        });
        let questions = card_now(&mut updates);
        assert_eq!(
            questions[0].state,
            QuestionState::Answered {
                answer: "Spaces".into()
            }
        );
        assert!(questions[1].state.is_open());
        assert_eq!(feed.open_questions().len(), 1);
        // Open after the turn, as its questions are.
        feed.turn_ended("3 s", None);
        assert_eq!(feed.activity().question.unwrap().id, card);
        // The agent closed the other.
        feed.event(
            &event(
                "main",
                AgentEvent::QuestionsClosed {
                    ids: vec!["q2".into()],
                    why: Some("Found it".into()),
                },
            ),
            None,
        );
        assert_eq!(
            card_now(&mut updates)[1].state,
            QuestionState::Closed {
                why: Some("Found it".into())
            }
        );
        assert_eq!(feed.activity(), Activity::default());
        assert!(feed.open_questions().is_empty());
        // Another conversation's questions aren't shown.
        feed.questions_run(Some(Uuid::new_v4()));
        assert!(feed.questions().is_empty());
    }

    /// Gives the feed the result `output` of run `run`'s ask.
    fn ask_result(feed: &Feed, run: Uuid, output: &str) {
        let mut result = event(
            "main",
            AgentEvent::ToolResult {
                tool_call_id: Uuid::new_v4(),
                tool_name: arko_agents::builtins::ASK_USER_TOOL_NAME.into(),
                output: output.into(),
            },
        );
        result.agent_run_id = run;
        feed.event(&result, None);
    }

    fn question_cards(feed: &Feed) -> Vec<Item> {
        let (snapshot, _) = feed.subscribe();
        snapshot
            .items
            .into_iter()
            .filter(|item| item.kind == Kind::Question)
            .collect()
    }

    /// Claude Code records an ask's questions as its call ends, and waits for
    /// them, if the agent waits, only when the turn ends. Their card comes
    /// with the ask's result, with or without the wait, before the turn
    /// ends, and the agent counts as waiting only once the wait comes. A
    /// question answered meanwhile keeps the card, with its answer, and the
    /// wait for the other keeps it too. The web page and the full-screen
    /// view are sent the card once.
    #[test]
    fn an_asks_card_comes_with_its_result_once() {
        use crate::questions::QuestionState;
        for (wait, output) in [
            (
                true,
                "Asked q1 and q2. End your turn now without calling tools, and the answer arrives as the next message.",
            ),
            (
                false,
                "Asked q1 and q2. Their answers reach you as a note. Until then, do only work that no answer could change.",
            ),
        ] {
            let feed = Feed::new("main");
            let run = Uuid::new_v4();
            feed.questions_run(Some(run));
            let (_, mut updates) = feed.subscribe();
            let both = [asked("q1", "Tabs or spaces?"), asked("q2", "Push now?")];
            feed.record_asked(run, &both);
            assert!(
                question_cards(&feed).is_empty(),
                "no card before the result"
            );
            ask_result(&feed, run, output);
            let cards = question_cards(&feed);
            assert_eq!(cards.len(), 1, "{wait}");
            assert_eq!(cards[0].text, "Arko asked 2 questions");
            assert_eq!(feed.open_questions()[1].item, cards[0].id);
            assert_eq!(feed.open_questions()[1].number, Some(2));
            // The agent works on until its wait comes.
            assert!(feed.open_questions().iter().all(|question| !question.waits));
            assert_eq!(feed.activity().question, None);
            feed.settle_questions(Some(run), &["q1".into()], |_| QuestionState::Answered {
                answer: "Spaces".into(),
            });
            // The wait for q2 as the turn ends, or the asker hearing of an
            // ask that doesn't wait.
            let later = if wait { &both[1..] } else { &both[..] };
            assert_eq!(feed.ask(run, later, wait), cards[0].id);
            let cards = question_cards(&feed);
            assert_eq!(cards.len(), 1, "{wait}");
            let questions = cards[0].questions.clone().unwrap();
            assert_eq!(
                questions
                    .iter()
                    .map(|question| (question.id.as_str(), question.state.clone()))
                    .collect::<Vec<_>>(),
                [
                    (
                        "q1",
                        QuestionState::Answered {
                            answer: "Spaces".into()
                        }
                    ),
                    ("q2", QuestionState::Open)
                ]
            );
            assert_eq!(questions[1].waits, wait);
            assert_eq!(
                feed.activity().question.map(|question| question.id),
                wait.then_some(cards[0].id)
            );
            let mut sent = 0;
            while let Ok(update) = updates.try_recv() {
                if matches!(update, Update::Item(item) if item.kind == Kind::Question) {
                    sent += 1;
                }
            }
            assert_eq!(sent, 1, "{wait}");
        }
    }

    /// After a restart, a wait for questions whose card the transcript no
    /// longer shows gets a card of its own.
    #[test]
    fn a_wait_after_a_restart_gets_a_card_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::questions::FILE);
        let run = Uuid::new_v4();
        let first = Feed::new("main");
        first.keep_questions(path.clone());
        first.questions_run(Some(run));
        first.ask(run, &[asked("q1", "Tabs or spaces?")], true);
        let again = Feed::new("main");
        again.keep_questions(path);
        again.questions_run(Some(run));
        again.ask(run, &[asked("q1", "Tabs or spaces?")], true);
        let cards = question_cards(&again);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].questions.as_ref().unwrap()[0].id, "q1");
    }

    /// An app's tests ask questions and set their states, and the card and
    /// the activity follow.
    #[test]
    fn apps_tests_ask_and_settle_questions() {
        use crate::questions::QuestionState;
        let feed = Feed::new("main");
        let card = feed.ask_questions(&[asked("q1", "Tabs or spaces?")], true);
        assert_eq!(feed.activity().question.unwrap().id, card);
        assert!(feed.open_questions()[0].waits);
        feed.set_question_state(
            "q1",
            QuestionState::Answered {
                answer: "Spaces".into(),
            },
        );
        assert!(feed.open_questions().is_empty());
        assert_eq!(feed.activity().question, None);
        let (snapshot, _) = feed.subscribe();
        let item = snapshot.items.iter().find(|item| item.id == card).unwrap();
        assert_eq!(
            item.questions.as_ref().unwrap()[0].state,
            QuestionState::Answered {
                answer: "Spaces".into()
            }
        );
    }

    /// A resumed session's question cards, made from its conversation,
    /// show what became of each question as the session kept it, and its
    /// open questions are open again. A card the session made as the ask's
    /// result came (as in Claude Code) restores as one made when the asker
    /// heard of the ask.
    #[test]
    fn a_resumed_sessions_cards_keep_their_questions_states() {
        use crate::questions::QuestionState;
        use arko_ai::{Content, Message, ToolCall, ToolResult};
        for recorded in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(crate::questions::FILE);
            let run = Uuid::new_v4();
            let first = Feed::new("main");
            first.keep_questions(path.clone());
            first.questions_run(Some(run));
            let both = [asked("q1", "Tabs or spaces?"), asked("q2", "Push now?")];
            if recorded {
                first.record_asked(run, &both);
                ask_result(
                    &first,
                    run,
                    "Asked q1 and q2. Their answers reach you as a note. Until then, do only work that no answer could change.",
                );
            } else {
                first.ask(run, &both, false);
            }
            assert_eq!(question_cards(&first).len(), 1);
            first.settle_questions(None, &["q1".into()], |_| QuestionState::Dismissed);

            for output in [
                "Asked q1 and q2. Their answers reach you as a note. Until then, do only work that no answer could change.",
                "Asked q1 and q2. Their answers reach you as a note, so go on with your work.",
            ] {
                let again = Feed::new("main");
                again.keep_questions(path.clone());
                again.questions_run(Some(run));
                let mut ask = Message::new_assistant("");
                ask.tool_calls = vec![
                    serde_json::from_value::<ToolCall>(serde_json::json!({
                        "id": "ask",
                        "name": "ask_user",
                        "arguments": {"questions": ["Tabs or spaces?", "Push now?"], "wait": false}
                    }))
                    .unwrap(),
                ];
                let messages =
                    vec![
                        Message::new_user("Start."),
                        ask,
                        Message::new_user_with_contents(vec![Content::ToolResult(
                            ToolResult::new(Some("ask".into()), "ask_user", output),
                        )]),
                    ];
                again.restore(crate::remote::history(
                    &messages,
                    false,
                    &crate::images::Images::default(),
                ));
                let (snapshot, _) = again.subscribe();
                let card = snapshot
                    .items
                    .iter()
                    .find(|item| item.kind == Kind::Question)
                    .unwrap();
                let states = card
                    .questions
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|question| question.state.clone())
                    .collect::<Vec<_>>();
                assert_eq!(states, [QuestionState::Dismissed, QuestionState::Open]);
                assert_eq!(card.text, "Arko asked 2 questions");
                assert_eq!(again.open_questions()[0].item, card.id);
                assert_eq!(again.open_questions()[0].number, Some(2));
                // (The agent doesn't wait on them.)
                assert_eq!(again.activity().question, None);
            }
        }
    }

    /// A session saved while approvals were questions in the answer panel
    /// opens with them closed, since their tools waited in a process that
    /// is gone, and its transcript shows the saves without their old cards.
    #[test]
    fn a_session_saved_with_approval_questions_opens_without_them() {
        use arko_ai::{Content, Message, ToolCall, ToolResult};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::questions::FILE);
        let run = Uuid::new_v4();
        // As 0.13 wrote it: an approval answered, one open, and a
        // question of the agent's.
        let old = serde_json::json!({
            "questions": [
                {
                    "run": run, "card": Uuid::new_v4(), "call": "b",
                    "id": "x1a2b3c4d", "question": "The main agent wants to save the worker profile reviewer in your Arko folder, for every project.",
                    "choices": ["Save", "Don't save"], "state": "answered", "answer": "Save",
                    "item": 4,
                    "approval": {"name": "reviewer", "model": "sol", "scope": "every project", "description": "Reviews"}
                },
                {
                    "run": run, "card": Uuid::new_v4(), "call": "c",
                    "id": "x5e6f7a8b", "question": "The main agent wants to save the worker profile tester in your Arko folder, for every project.",
                    "choices": ["Save", "Don't save"], "state": "open", "waits": true, "item": 4,
                    "approval": {"name": "tester", "model": "sol", "effort": "high", "scope": "every project", "description": "Tests"}
                },
                {"run": run, "id": "q1", "question": "Tabs?", "state": "open", "item": 5, "number": 1}
            ],
            "last": 1
        });
        std::fs::write(&path, old.to_string()).unwrap();
        let again = Feed::new("main");
        again.keep_questions(path);
        again.questions_run(Some(run));
        assert_eq!(
            again
                .open_questions()
                .iter()
                .map(|question| question.id.as_str())
                .collect::<Vec<_>>(),
            ["q1"]
        );
        assert!(again.approvals().is_empty());
        let save = |id: &str, name: &str| {
            serde_json::from_value::<ToolCall>(serde_json::json!({
                "id": id,
                "name": crate::crew::SAVE_WORKER_PROFILE,
                "arguments": {"name": name, "description": "Reviews"}
            }))
            .unwrap()
        };
        let mut saves = Message::new_assistant("");
        saves.tool_calls = vec![save("b", "reviewer"), save("c", "tester")];
        let results = ["b", "c"].map(|id| {
            Content::ToolResult(ToolResult::new(
                Some(id.into()),
                crate::crew::SAVE_WORKER_PROFILE,
                "Done.",
            ))
        });
        again.restore(crate::remote::history(
            &[
                Message::new_user("Start."),
                saves,
                Message::new_user_with_contents(results.to_vec()),
            ],
            false,
            &crate::images::Images::default(),
        ));
        let (snapshot, _) = again.subscribe();
        let kinds: Vec<Kind> = snapshot.items.iter().map(|item| item.kind).collect();
        assert!(!kinds.contains(&Kind::Question), "{kinds:?}");
        assert_eq!(kinds.iter().filter(|kind| **kind == Kind::Call).count(), 2);
    }

    /// Requests that come while others wait share their batch, and one
    /// that comes to an empty tray starts the next. Each change of the tray
    /// reaches the clients as a change of the activity, so their status
    /// shows it. An answer settles the requests it names, and a request
    /// whose tool stopped waiting leaves.
    #[test]
    fn the_approval_tray_groups_requests_and_settles_them() {
        use crate::approvals::{Answer, Approval, ApprovalAnswers, ApprovalChoice};
        let feed = Feed::new("main");
        let (_, mut updates) = feed.subscribe();
        let run = Uuid::new_v4();
        let profile = |name: &str| {
            Approval::Profile(crate::profiles::approval(
                &crate::profiles::Profile::new(name, "Reviews"),
                crate::profiles::Place::User,
            ))
        };
        let (a, mut answer_a) = feed.request_approval(run, None, profile("one"));
        let (b, mut answer_b) = feed.request_approval(run, None, profile("two"));
        assert!(matches!(updates.try_recv(), Ok(Update::Activity)));
        let requests = feed.approvals();
        assert_eq!(
            requests
                .iter()
                .map(|request| (request.id.as_str(), request.batch))
                .collect::<Vec<_>>(),
            [(a.as_str(), 1), (b.as_str(), 1)]
        );
        // Answering only the first leaves the second.
        let answered = feed.answer_approvals(&ApprovalAnswers {
            choices: vec![ApprovalChoice {
                id: a.clone(),
                approve: true,
                option: true,
            }],
            words: Some("  ".into()),
        });
        assert_eq!(answered.len(), 1);
        assert_eq!(
            answer_a.try_recv().unwrap(),
            Answer::Approved {
                words: None,
                option: false
            },
            "a profile has no option"
        );
        assert!(answer_b.try_recv().is_err());
        // One that comes while b waits joins its batch.
        let (c, answer_c) = feed.request_approval(run, None, profile("three"));
        assert_eq!(feed.approvals()[1].batch, 1);
        drop(answer_c);
        assert_eq!(feed.approvals().len(), 1, "c's tool stopped waiting");
        feed.withdraw_approval(&c);
        assert!(
            feed.answer_approvals(&ApprovalAnswers {
                choices: vec![ApprovalChoice {
                    id: c,
                    approve: true,
                    option: false,
                }],
                words: None,
            })
            .is_empty()
        );
        feed.withdraw_approval(&b);
        assert!(feed.approvals().is_empty());
        let (_, _d) = feed.request_approval(run, None, profile("four"));
        assert_eq!(feed.approvals()[0].batch, 2, "a new tray");
    }

    /// A client that subscribes while an approval request comes in either
    /// gets the request's update, or subscribes after it and gets none, as
    /// the request goes out with the state locked. It never sees the
    /// request in the tray and then gets its update too.
    #[test]
    fn a_request_and_its_update_reach_a_new_client_once() {
        use crate::approvals::Approval;
        let feed = Feed::new("main");
        let (sender, joined) = std::sync::mpsc::channel();
        let subscriber = feed.clone();
        *feed.inner.pause.lock().unwrap() = Some(Box::new(move || {
            // While the request waits to go out, a client subscribes.
            std::thread::spawn(move || {
                let (_, updates) = subscriber.subscribe();
                let saw = subscriber.approvals().len();
                let _ = sender.send((updates, saw));
            });
            std::thread::sleep(Duration::from_millis(100));
        }));
        let profile = Approval::Profile(crate::profiles::approval(
            &crate::profiles::Profile::new("one", "Reviews"),
            crate::profiles::Place::User,
        ));
        let (_, _answer) = feed.request_approval(Uuid::new_v4(), None, profile);
        let (mut updates, saw) = joined.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(saw, 1, "the client sees the request");
        assert!(
            matches!(
                updates.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "and gets no update for it"
        );
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
        resumed.worker_restored("r1", "Count", WorkerState::Stopped, None, None, 0, total, 0);
        assert_eq!(resumed.workers()[0].ran_ms, total);
        assert_eq!(resumed.workers()[0].run_time_ms(now_ms()), total);
    }

    /// A worker restored with a session has the calls it made before, and
    /// its next run adds to them and to its time.
    #[test]
    fn a_restored_worker_goes_on_from_its_calls_and_time() {
        let feed = Feed::new("main");
        feed.worker_restored("r1", "Count", WorkerState::Stopped, None, None, 0, 5_000, 7);
        assert_eq!(feed.worker_tool_calls(), [("r1".to_string(), 7)]);
        feed.worker("r1", "Count", WorkerChange::Started);
        feed.event(&event("worker", shell(Uuid::new_v4(), "ls")), Some("r1"));
        let worker = &feed.workers()[0];
        assert_eq!((worker.ran_ms, worker.tool_calls), (5_000, 8));
        assert!(worker.run_time_ms(now_ms()) >= 5_000);
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
            None,
            5,
            0,
            0,
        );
        feed.worker_restored("r1", "Again", WorkerState::Running, None, None, 6, 0, 0);
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

    /// A restored worker that failed has the saved reason as its error, and
    /// one that stopped, whose record holds why, does not.
    #[test]
    fn a_restored_worker_that_failed_keeps_its_saved_error() {
        let feed = Feed::new("main");
        let why = Some("the model refused");
        feed.worker_restored("r1", "Review", WorkerState::Failed, None, why, 5, 0, 0);
        feed.worker_restored("r2", "Count", WorkerState::Stopped, None, why, 5, 0, 0);
        let errors: Vec<Option<String>> = feed.workers().into_iter().map(|w| w.error).collect();
        assert_eq!(errors, [Some("the model refused".to_string()), None]);
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

    // ----- clients made from snapshots -----

    use crate::testing::{FeedMirror, RandomSession};

    /// The updates waiting for `receiver`. These tests read after each
    /// call, so a client that fell behind fails them.
    fn delivered(receiver: &mut broadcast::Receiver<Update>) -> Vec<Update> {
        let mut out = Vec::new();
        loop {
            match receiver.try_recv() {
                Ok(update) => out.push(update),
                Err(broadcast::error::TryRecvError::Empty) => return out,
                Err(other) => panic!("{other:?}"),
            }
        }
    }

    /// Whatever a session does, a client made from a snapshot taken at any
    /// moment, which then applies the updates after it, ends where a client
    /// made from the latest snapshot does, within what the feed keeps. So
    /// does a client that followed every update from the start, and a
    /// batched client whatever its batches. Each snapshot counts the
    /// updates sent before it. The activity is read as each update comes,
    /// as clients read it, so a change of it that went out without an
    /// update would show.
    #[test]
    fn a_client_made_from_any_snapshot_ends_where_the_feed_is() {
        for seed in 0..24 {
            let mut session = RandomSession::new(seed);
            let feed = session.feed().clone();
            let (start, mut updates) = feed.subscribe();
            let (_, mut batches) = feed.subscribe_batched();
            let mut clients = vec![FeedMirror::new(&start)];
            let mut batched = FeedMirror::new(&start);
            let mut count = start.seq;
            let steps = 300 + session.pick(1200);
            let mut done = Vec::new();
            for step in 0..steps {
                done.push(session.step());
                for update in delivered(&mut updates) {
                    count += 1;
                    for client in &mut clients {
                        client.apply(&update, || feed.activity());
                    }
                }
                let check = step + 1 == steps || session.pick(steps / 12 + 1) == 0;
                if (check || session.pick(5) == 0)
                    && let Some(batch) = batches.try_next()
                {
                    for update in &batch {
                        batched.apply(update, || feed.activity());
                    }
                }
                if !check {
                    continue;
                }
                let (snapshot, _) = feed.subscribe();
                assert_eq!(snapshot.seq, count, "seed {seed}, step {step}");
                let rebuilt = FeedMirror::new(&snapshot);
                for (at, client) in clients.iter().chain([&batched]).enumerate() {
                    if let Some(difference) = client.within(&snapshot).difference(&rebuilt) {
                        let last = &done[done.len().saturating_sub(10)..];
                        panic!(
                            "seed {seed}, step {step}, client {at}: {difference}\nlast calls {last:?}"
                        );
                    }
                }
                clients.push(rebuilt);
            }
        }
    }

    /// How `client` differs from a client made from `snapshot`, within what
    /// the snapshot keeps, as the tests of clients made from snapshots
    /// compare them.
    fn mismatch(client: &FeedMirror, snapshot: &Snapshot) -> Option<String> {
        client
            .within(snapshot)
            .difference(&FeedMirror::new(snapshot))
    }

    /// The comparison those tests make is no more forgiving than a client:
    /// a change of an item the client never got, a running job the feed
    /// no longer has, and a change the feed made without an update each
    /// show.
    #[test]
    fn a_client_made_from_a_snapshot_shows_what_it_missed() {
        // The call's item went by unseen, then its result came.
        let feed = Feed::new("main");
        let (start, mut updates) = feed.subscribe();
        let (mut client, mut seen) = (FeedMirror::new(&start), FeedMirror::new(&start));
        let call = Uuid::new_v4();
        feed.event(&event("main", shell(call, "ls")), None);
        feed.event(&event("main", result(call, "Exit code: 0\nfile")), None);
        for update in delivered(&mut updates) {
            if !matches!(update, Update::Item(_)) {
                client.apply(&update, || feed.activity());
            }
            seen.apply(&update, || feed.activity());
        }
        assert_eq!(mismatch(&seen, &feed.subscribe().0), None);
        assert!(mismatch(&client, &feed.subscribe().0).is_some(), "a change");

        // The first job's end went by unseen, and the feed dropped it once
        // more than it keeps had ended. A client that saw it end differs
        // only by the drop.
        let feed = Feed::new("main");
        let (start, mut updates) = feed.subscribe();
        let (mut client, mut seen) = (FeedMirror::new(&start), FeedMirror::new(&start));
        let first = Uuid::new_v4();
        feed.job_started(JobStart::new(first, MAIN_AGENT, JobKind::Command, "make"));
        feed.job_ended(first, JobState::Finished, None);
        for update in delivered(&mut updates) {
            if matches!(&update, Update::Job(job) if job.state.is_running()) {
                client.apply(&update, || feed.activity());
            }
            seen.apply(&update, || feed.activity());
        }
        for _ in 0..=ENDED_JOBS {
            let id = Uuid::new_v4();
            feed.job_started(JobStart::new(id, MAIN_AGENT, JobKind::Command, "test"));
            feed.job_ended(id, JobState::Finished, None);
        }
        for update in delivered(&mut updates) {
            client.apply(&update, || feed.activity());
            seen.apply(&update, || feed.activity());
        }
        let end = feed.subscribe().0;
        assert!(end.jobs.iter().all(|job| job.id != first.to_string()));
        assert_eq!(mismatch(&seen, &end), None);
        assert!(mismatch(&client, &end).is_some(), "a running job");

        // The feed changed an item and the activity without an update.
        for change in [
            (|state: &mut State| state.main.items.back_mut().unwrap().text.push('!'))
                as fn(&mut State),
            |state: &mut State| state.activity.working = true,
        ] {
            let feed = Feed::new("main");
            let (start, mut updates) = feed.subscribe();
            let mut client = FeedMirror::new(&start);
            feed.notice(Level::Info, "Done.");
            for update in delivered(&mut updates) {
                client.apply(&update, || feed.activity());
            }
            assert_eq!(mismatch(&client, &feed.subscribe().0), None);
            change(&mut feed.lock());
            assert!(delivered(&mut updates).is_empty());
            assert!(
                mismatch(&client, &feed.subscribe().0).is_some(),
                "no update"
            );
        }
    }

    /// A client that subscribes while a worker's summary waits to go out
    /// gets the newer summary in its snapshot. Once the summary goes out, at
    /// its time, it and a client that subscribed before both end where the
    /// feed is.
    #[test]
    fn a_snapshot_taken_while_a_summary_waits_ends_where_the_feed_is() {
        let clock = ManualClock::new();
        let feed = Feed::with_clock("main", clock.clone());
        feed.worker("r1", "Count", WorkerChange::Started);
        let (before, mut early) = feed.subscribe();
        let mut follower = FeedMirror::new(&before);
        // Within a quarter of a second of the last summary, so the new one
        // waits.
        clock.advance(WORKER_EVERY / 2);
        feed.event(&event("worker", shell(Uuid::new_v4(), "ls")), Some("r1"));
        assert_eq!(clock.waiting(), 1, "its timer");
        let (during, mut late) = feed.subscribe();
        let mut rebuilt = FeedMirror::new(&during);
        assert_eq!(during.workers[0].tool_calls, 1, "the snapshot has it");
        for update in delivered(&mut early) {
            follower.apply(&update, || feed.activity());
        }
        assert_eq!(follower.workers[0].tool_calls, 0, "its update waits");
        clock.advance(WORKER_EVERY / 2 - Duration::from_millis(1));
        assert!(delivered(&mut early).is_empty(), "until its time");
        clock.advance(Duration::from_millis(1));
        assert_eq!(clock.waiting(), 0);
        for update in delivered(&mut early) {
            follower.apply(&update, || feed.activity());
        }
        for update in delivered(&mut late) {
            rebuilt.apply(&update, || feed.activity());
        }
        let end = FeedMirror::new(&feed.subscribe().0);
        assert_eq!(follower.difference(&end), None);
        assert_eq!(rebuilt.difference(&end), None);
    }

    /// A client that fell behind while a note moved to where the agent read
    /// it and the conversation was cut back starts over from a snapshot,
    /// and with what follows ends where the feed is.
    #[test]
    fn a_client_behind_a_cut_and_a_moved_note_starts_over_where_the_feed_is() {
        let feed = Feed::new("main");
        let (_, mut updates) = feed.subscribe();
        let first = feed.user("First.", Source::Terminal, Delivery::Turn);
        let note = feed.note("Also this.", Source::Web, 1, Vec::new());
        // Enough text to fall behind, in few items.
        let mut cut = None;
        for at in 0..BACKLOG {
            feed.event(&event("main", chunk("Working. ")), None);
            if at == BACKLOG / 2 {
                let call = Uuid::new_v4();
                feed.event(&event("main", shell(call, "ls")), None);
                feed.event(&event("main", result(call, "Exit code: 0\nfile")), None);
                cut = Some(feed.user("Second.", Source::Terminal, Delivery::Turn));
            }
        }
        let read = AgentEvent::ContextInjected {
            ids: vec!["note:1".into()],
            bytes: 10,
        };
        feed.event(&event("main", read), None);
        let moved = feed.subscribe().0.items.last().unwrap().clone();
        assert!(moved.id > note && moved.text == "Also this.", "{moved:?}");
        let cut = cut.unwrap();
        feed.truncate(cut);
        assert!(matches!(
            updates.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        let (snapshot, mut fresh) = feed.subscribe();
        let mut client = FeedMirror::new(&snapshot);
        assert!(client.items.contains_key(&first));
        assert!(!client.items.contains_key(&note) && !client.items.contains_key(&moved.id));
        assert!(client.items.keys().all(|id| *id < cut));
        feed.user("Third.", Source::Terminal, Delivery::Turn);
        for update in delivered(&mut fresh) {
            client.apply(&update, || feed.activity());
        }
        let end = feed.subscribe().0;
        assert_eq!(end.seq, snapshot.seq + 1);
        assert_eq!(client.difference(&FeedMirror::new(&end)), None);
    }
}

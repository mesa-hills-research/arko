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
use tokio::sync::broadcast;
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
    /// `whole`), the latest. They don't count in `bytes`, so long outputs
    /// don't push items out. Past it, the oldest commands' blocks keep
    /// their first lines only.
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
/// Updates a client's stream may fall behind by before it starts over.
const BACKLOG: usize = 2048;
/// A worker's summary goes out at most this often while it works (four
/// times a second); a change of its state goes out at once.
const WORKER_EVERY: Duration = Duration::from_millis(250);

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The user's message, note, answer or command.
    User,
    /// An agent's text, as it streams.
    Text,
    /// The main agent's message to the user while it works, or a message a
    /// worker got (`from` says whose).
    Message,
    /// A tool call; its `status` and `outcome` follow as a [`Update::Change`].
    Call,
    /// A result that needs a block under its call: a command's output, a
    /// long error.
    Result,
    /// What Arko says: a stop, an error, a retry, a handover.
    Notice,
    /// A command's output.
    Output,
    /// The main agent's question.
    Question,
    /// The main agent's reply, which ends its turn; or a worker's report,
    /// which ends its run.
    Reply,
    /// The end of a turn: its time and cost.
    Turn,
    /// Where a worker started, in the main transcript: clients draw the
    /// worker's [`WorkerSummary`] there.
    WorkerLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// Where the user's text was typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Terminal,
    Web,
}

/// What became of the user's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// It started a turn.
    Turn,
    /// It reached the working agent as a note.
    Note,
    /// It answered the agent's question.
    Answer,
    /// It was a command.
    Command,
    /// It waits for a turn of its own.
    Queued,
}

/// How a tool call is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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
    /// Whose it is: [`MAIN_AGENT`] (the main agent, or Arko), or a worker's
    /// ID (`r1`).
    pub agent: String,
    /// The main agent's turn it came in: one more with each turn, `None`
    /// before the first.
    pub turn: Option<u64>,
    pub text: String,
    /// More, shown folded or under the text: a call's whole parameter, a
    /// command's first lines of output.
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
    /// A command's whole output, when its `detail` shows the first lines:
    /// the terminal's view opens it on a click. The web page doesn't get
    /// it, and its clones share it.
    #[serde(skip)]
    pub whole: Option<Arc<str>>,
    /// The worker a `worker_line` stands for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    /// Who sent a worker's `message`: [`MAIN_AGENT`] (its lead), or
    /// another worker's ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The call a `result` is of: clients show it under that call, which
    /// other items may have followed by the time the result came.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call: Option<u64>,
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
            worker: None,
            from: None,
            call: None,
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

    fn size(&self) -> usize {
        96 + self.text.len()
            + self.detail.as_ref().map_or(0, String::len)
            + self.outcome.as_ref().map_or(0, String::len)
    }

    /// The bytes of its whole output.
    pub fn whole_size(&self) -> usize {
        self.whole.as_ref().map_or(0, |whole| whole.len())
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
}

/// Where a worker is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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

/// A worker, as a line: `r2 · Fix flaky test · 18 tool calls · 2m · running`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkerSummary {
    /// Its ID: `r1`, `r2`, ... (as `/workers` says).
    pub id: String,
    /// Its task's first line, in brief.
    pub title: String,
    pub state: WorkerState,
    /// The calls it made, in all its runs.
    pub tool_calls: u32,
    /// When its latest run started (Unix milliseconds): a continued worker
    /// counts its time from there.
    pub started_at: Option<i64>,
    /// When its latest run ended.
    pub ended_at: Option<i64>,
    /// What it did last, in a line: its latest call, or why it stopped.
    pub last: Option<String>,
    /// Its report, once it finished.
    pub report: Option<String>,
}

/// What became of a worker: what the workflow says of its runs.
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

/// What changed, for the clients connected.
#[derive(Debug, Clone)]
pub enum Update {
    /// A new item.
    Item(Item),
    /// More text for the item `id`.
    Append { id: u64, text: String },
    /// The item with this one's `id` is now this one (a call's result came).
    Change(Item),
    /// A worker's summary, whole.
    Worker(WorkerSummary),
    /// The activity changed.
    Activity,
}

/// The history, the workers and the activity at one moment.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The main agent's items and the workers', in order (by `id`).
    pub items: Vec<Item>,
    /// The workers, in the order they started.
    pub workers: Vec<WorkerSummary>,
    pub activity: Activity,
}

/// The session's feed. Cheap to clone; every clone feeds the same history.
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
}

/// A transcript kept within its budget.
#[derive(Default)]
struct Lane {
    items: VecDeque<Item>,
    bytes: usize,
    /// The bytes of the items' whole outputs.
    wholes: usize,
}

impl Lane {
    fn push(&mut self, item: Item, budget: Budget) {
        self.bytes += item.size();
        self.wholes += item.whole_size();
        self.items.push_back(item);
        while self.items.len() > budget.items || self.bytes > budget.bytes {
            match self.items.pop_front() {
                Some(old) => {
                    self.bytes = self.bytes.saturating_sub(old.size());
                    self.wholes -= old.whole_size();
                }
                None => break,
            }
        }
        // The oldest whole outputs go first.
        for item in self.items.iter_mut() {
            if self.wholes <= budget.wholes {
                break;
            }
            self.wholes -= item.whole_size();
            item.whole = None;
        }
    }

    /// The item `id`, while it is kept.
    fn find(&mut self, id: u64) -> Option<&mut Item> {
        // IDs grow as items are pushed.
        let index = self.items.binary_search_by_key(&id, |item| item.id).ok()?;
        self.items.get_mut(index)
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
    /// The text item each agent's next chunk continues.
    open_text: HashMap<String, u64>,
    activity: Activity,
    next_question: u64,
    turn: Option<u64>,
    /// The user's message started the turn `turn`, which hasn't begun yet.
    turn_opened: bool,
    /// Names of the runs that came without a label, by run.
    runs: HashMap<Uuid, String>,
    /// Calls waiting for their results, by call.
    calls: HashMap<Uuid, Call>,
    workers: Vec<Worker>,
}

impl State {
    fn lane(&mut self, agent: &str) -> (&mut Lane, Budget) {
        if agent == MAIN_AGENT {
            (&mut self.main, MAIN_BUDGET)
        } else {
            (&mut self.workers_lane, WORKER_BUDGET)
        }
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
        let mut items: Vec<Item> = state
            .main
            .items
            .iter()
            .chain(&state.workers_lane.items)
            .cloned()
            .collect();
        // (Two runs in order: the sort merges them.)
        items.sort_by_key(|item| item.id);
        (
            Snapshot {
                items,
                workers: state.workers.iter().map(|w| w.summary.clone()).collect(),
                activity: state.activity.clone(),
            },
            self.inner.updates.subscribe(),
        )
    }

    pub fn activity(&self) -> Activity {
        self.lock().activity.clone()
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
        // No client may be connected.
        let _ = self.inner.updates.send(update);
    }

    fn push(&self, state: &mut State, mut item: Item) -> u64 {
        state.next_id += 1;
        item.id = state.next_id;
        item.at = now_ms();
        item.turn = state.turn;
        match item.kind {
            Kind::Text => {
                state.open_text.insert(item.agent.clone(), item.id);
            }
            // A worker's placeholder doesn't break the main agent's text.
            Kind::WorkerLine => {}
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

    /// An agent's text goes on.
    fn append(&self, state: &mut State, agent: &str, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        if let Some(id) = state.open_text.get(agent).copied()
            && state.lane(agent).0.append(id, chunk)
        {
            self.send(Update::Append {
                id,
                text: chunk.to_string(),
            });
            return;
        }
        self.push(state, Item::new(Kind::Text, chunk).agent(agent));
    }

    /// The call `id` ended with `outcome`: its line says so, and a block
    /// follows it when there is one.
    fn settle(&self, state: &mut State, id: &Uuid, outcome: Outcome) {
        let Some(call) = state.calls.remove(id) else {
            return;
        };
        if let Some(item) = state.lane(&call.agent).0.find(call.item) {
            item.status = Some(outcome.status);
            item.outcome = outcome.text;
            let item = item.clone();
            self.send(Update::Change(item));
        }
        if let Some(mut block) = outcome.block {
            block.call = Some(call.item);
            self.push(state, block.agent(call.agent));
        }
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

    /// A workflow's event: `label` names the worker whose run sent it (the
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
                self.append(&mut state, &agent, text);
            }
            AgentEvent::ToolCallFormatted {
                tool_call_id,
                tool_name,
                parameters,
            } => {
                let Some(mut item) = render::call(tool_name, parameters) else {
                    return;
                };
                if !main {
                    self.saw_worker(&mut state, &agent);
                }
                let call = item.kind == Kind::Call;
                if call {
                    item.status = Some(CallStatus::Running);
                }
                let line = item.text.clone();
                let id = self.push(&mut state, item.agent(agent.clone()));
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
                    format!("refused: {why}")
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
                    let item = notice(render::cut(error_message, 600), Level::Error);
                    self.push(&mut state, item);
                }
            },
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
            AgentEvent::UsageLimitReached { error_message, .. } => {
                let text = format!("usage limit reached: {}", render::cut(error_message, 300));
                self.push(&mut state, notice(text, Level::Warn));
            }
            AgentEvent::OutputTokenLimitReached { output_tokens, .. } if main => {
                let text =
                    format!("the reply reached the model's output limit ({output_tokens} tokens)");
                self.push(&mut state, notice(text, Level::Warn));
            }
            AgentEvent::HandoverRequested { input_tokens } if main => {
                let text = crate::handover::requested_text(event.agent_run_id, *input_tokens);
                self.push(&mut state, notice(text, Level::Info));
            }
            AgentEvent::ConversationHandedOver { .. } if main => {
                let text =
                    "handed over: the agent continues in a fresh conversation from its report";
                self.push(&mut state, notice(text.into(), Level::Info));
            }
            AgentEvent::ConversationCompacted {
                input_tokens,
                compacted_tokens,
            } if main => {
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
    pub fn user(&self, text: &str, source: Source, delivery: Delivery) {
        let mut item = Item::new(Kind::User, text.trim());
        item.source = Some(source);
        item.delivery = Some(delivery);
        let mut state = self.lock();
        if delivery == Delivery::Turn {
            state.turn = Some(state.turn.map_or(1, |turn| turn + 1));
            state.turn_opened = true;
        }
        self.push(&mut state, item);
    }

    /// The user's message to `worker`, which went to it directly: the
    /// worker's own transcript shows it, and the main one too.
    pub fn to_worker(&self, worker: &str, text: &str, source: Source) {
        let mut item = Item::new(Kind::User, text.trim()).agent(worker);
        item.source = Some(source);
        self.add(item);
    }

    /// Items from before Arko started (a resumed session's conversation,
    /// from [`history`](super::history)), numbered from 1: they join the
    /// history as they are, numbered after the feed's own (a result keeps
    /// its call).
    pub fn restore(&self, items: Vec<Item>) {
        let mut state = self.lock();
        let base = state.next_id;
        let at = now_ms();
        for mut item in items {
            item.id += base;
            item.call = item.call.map(|call| call + base);
            item.at = at;
            state.next_id = state.next_id.max(item.id);
            let (lane, budget) = state.lane(&item.agent);
            lane.push(item.clone(), budget);
            self.send(Update::Item(item));
        }
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

    /// A command's output at `level`: a warning (or an error) keeps its
    /// colour in the clients.
    pub fn output_at(&self, text: &str, level: Option<Level>) {
        if !text.trim().is_empty() {
            let mut item = Item::new(Kind::Output, text.trim_end());
            item.level = level;
            self.add(item);
        }
    }

    /// The main agent's reply. A reply in plain text has streamed as the
    /// agent's text already: that item becomes the reply (an
    /// [`Update::Change`]), so it shows once.
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
    /// say) and the workers' places.
    fn streamed_reply(state: &mut State, reply: &str) -> Option<u64> {
        let last = state
            .main
            .items
            .iter()
            .rev()
            .find(|item| !matches!(item.kind, Kind::Notice | Kind::WorkerLine))?;
        (last.kind == Kind::Text && last.is_main() && last.text.trim() == reply).then_some(last.id)
    }

    /// A turn of the main agent begins: the items from here on are its.
    pub fn turn_started(&self) {
        {
            let mut state = self.lock();
            if state.turn_opened {
                state.turn_opened = false;
            } else {
                state.turn = Some(state.turn.map_or(1, |turn| turn + 1));
            }
        }
        self.set_activity(|activity| activity.working = true);
    }

    /// The turn ended; `summary` is what it took (time, cost, context).
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
            activity.question = None;
        });
    }

    /// The main agent asks `text`; returns the question's ID, which an
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
    /// events): its summary follows, the main transcript gets its
    /// placeholder when it starts in a new turn, and its own transcript
    /// ends with its report, error or stop.
    pub fn worker(&self, id: &str, task: &str, change: WorkerChange<'_>) {
        let mut state = self.lock();
        self.known_worker(&mut state, id);
        let now = now_ms();
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
                summary.state = WorkerState::Queued;
                summary.started_at = None;
                summary.ended_at = None;
            }
            WorkerChange::Started => {
                summary.state = WorkerState::Running;
                summary.started_at = Some(now);
                summary.ended_at = None;
                summary.report = None;
            }
            WorkerChange::Finished { report } => {
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
                summary.state = WorkerState::Failed;
                summary.ended_at = Some(now);
                summary.last = Some(render::cut(error, 160));
                ended = Some(
                    Item::new(Kind::Notice, format!("failed: {}", render::cut(error, 600)))
                        .level(Level::Error),
                );
            }
            WorkerChange::Stopped { reason } => {
                summary.state = WorkerState::Stopped;
                summary.ended_at = Some(now);
                let reason = reason.trim();
                summary.last = Some(if reason.is_empty() {
                    "stopped".to_string()
                } else {
                    render::cut(&format!("stopped: {reason}"), 160)
                });
                ended = Some(Item::new(
                    Kind::Notice,
                    if reason.is_empty() {
                        "stopped".to_string()
                    } else {
                        format!("stopped: {}", render::cut(reason, 300))
                    },
                ));
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
    }

    /// A worker of the session from before Arko started (a resumed
    /// session): its summary, without a place in the transcript.
    pub fn worker_restored(
        &self,
        id: &str,
        task: &str,
        was: WorkerState,
        report: Option<&str>,
        since: i64,
    ) {
        let mut state = self.lock();
        if state.worker(id).is_some() {
            return;
        }
        let summary = WorkerSummary {
            id: id.to_string(),
            title: title(task),
            state: was,
            tool_calls: 0,
            started_at: was.is_active().then_some(since),
            ended_at: (!was.is_active()).then_some(since),
            last: None,
            report: report
                .map(str::trim)
                .filter(|report| !report.is_empty())
                .map(str::to_string),
        };
        state.workers.push(Worker {
            summary,
            placed: None,
            sent: None,
            pending: false,
        });
        self.worker_changed(&mut state, id, true);
    }

    /// Makes sure the worker `id` has a summary; true when it is new.
    fn known_worker(&self, state: &mut State, id: &str) -> bool {
        if state.worker(id).is_some() {
            return false;
        }
        state.workers.push(Worker {
            summary: WorkerSummary {
                id: id.to_string(),
                title: String::new(),
                state: WorkerState::Running,
                tool_calls: 0,
                started_at: Some(now_ms()),
                ended_at: None,
                last: None,
                report: None,
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
        let mut item = Item::new(Kind::WorkerLine, text);
        item.worker = Some(id.to_string());
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
    use crate::shell::SHELL_TOOL_NAME;

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
                    Some("refused: outside the workspace".into())
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

    /// A question is the activity until it is answered, or the turn ends.
    #[test]
    fn questions_are_open_until_answered() {
        let feed = Feed::new("main");
        feed.turn_started();
        let id = feed.question("Tabs or spaces?");
        assert_eq!(
            feed.activity(),
            Activity {
                working: true,
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

    /// The workers of a resumed session are listed, without a place in the
    /// transcript; a worker heard of first by its own events runs.
    #[test]
    fn workers_are_known_from_a_saved_session_or_their_events() {
        let feed = Feed::new("main");
        feed.worker_restored("r1", "Review", WorkerState::Finished, Some("All good."), 5);
        feed.worker_restored("r1", "Again", WorkerState::Running, None, 6);
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
                (Kind::Call, None, "message r2: a.rs is done."),
                (Kind::Call, None, "message lead: Halfway."),
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

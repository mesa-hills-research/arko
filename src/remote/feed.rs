//! The session as the page shows it: a bounded history of items and the
//! updates that follow it, made from the workflows' events and from what
//! fin says of the turns (the user's messages, the agent's questions and
//! replies, stops and failures).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use arko_agents::{AgentEvent, TaskState};
use arko_workflows::WorkflowEvent;
use serde::Serialize;
use tokio::sync::broadcast;
use uuid::Uuid;

use super::render;

/// The history kept for pages that connect: at most this many items...
const MAX_ITEMS: usize = 3000;
/// ...and about this many bytes of text.
const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Updates a page's stream may fall behind by before it starts over.
const BACKLOG: usize = 2048;

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The user's message, note, answer or command.
    User,
    /// The main agent's text, as it streams.
    Text,
    /// The main agent's message to the user while it works.
    Message,
    /// A tool call of the main agent.
    Call,
    /// A tool's result, in brief.
    Result,
    /// A worker's activity.
    Worker,
    /// What fin says: a stop, an error, a retry, a handover.
    Notice,
    /// A command's output.
    Output,
    /// The main agent's question.
    Question,
    /// The main agent's reply, which ends its turn.
    Reply,
    /// The end of a turn: its time and cost.
    Turn,
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

/// One entry of the page's log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub id: u64,
    /// When, in Unix milliseconds.
    pub at: i64,
    pub kind: Kind,
    /// The worker, for a worker's activity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub who: Option<String>,
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
}

impl Item {
    pub fn new(kind: Kind, text: impl Into<String>) -> Self {
        Self {
            id: 0,
            at: 0,
            kind,
            who: None,
            text: text.into(),
            detail: None,
            level: None,
            source: None,
            delivery: None,
        }
    }

    pub fn who(mut self, who: impl Into<String>) -> Self {
        self.who = Some(who.into());
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

    fn size(&self) -> usize {
        64 + self.text.len() + self.detail.as_ref().map_or(0, String::len)
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

/// What changed, for the pages connected.
#[derive(Debug, Clone)]
pub enum Update {
    /// A new item.
    Item(Item),
    /// More text for the item `id`.
    Append { id: u64, text: String },
    /// The activity changed.
    Activity,
}

/// The session's feed for the page. Cheap to clone; every clone feeds the
/// same history.
#[derive(Clone)]
pub struct Feed {
    inner: Arc<Inner>,
}

struct Inner {
    /// The main agent's name: its events are the conversation; the others'
    /// are workers' activity.
    main: String,
    state: Mutex<State>,
    updates: broadcast::Sender<Update>,
}

#[derive(Default)]
struct State {
    items: VecDeque<Item>,
    bytes: usize,
    next_id: u64,
    /// The text item the main agent's next chunk continues.
    open_text: Option<u64>,
    activity: Activity,
    next_question: u64,
    /// Labels of the runs that came without one, by run.
    runs: HashMap<Uuid, String>,
    /// The main agent's calls whose results are shown: tool, by call.
    calls: HashMap<Uuid, String>,
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

    /// The history and the activity now, and the updates from then on.
    pub fn subscribe(&self) -> (Vec<Item>, Activity, broadcast::Receiver<Update>) {
        let state = self.lock();
        (
            state.items.iter().cloned().collect(),
            state.activity.clone(),
            self.inner.updates.subscribe(),
        )
    }

    pub fn activity(&self) -> Activity {
        self.lock().activity.clone()
    }

    fn send(&self, update: Update) {
        // No page may be connected.
        let _ = self.inner.updates.send(update);
    }

    fn push(&self, state: &mut State, mut item: Item) -> u64 {
        state.next_id += 1;
        item.id = state.next_id;
        item.at = chrono::Utc::now().timestamp_millis();
        state.open_text = (item.kind == Kind::Text).then_some(item.id);
        state.bytes += item.size();
        state.items.push_back(item.clone());
        while state.items.len() > MAX_ITEMS || state.bytes > MAX_BYTES {
            match state.items.pop_front() {
                Some(old) => state.bytes = state.bytes.saturating_sub(old.size()),
                None => break,
            }
        }
        let id = item.id;
        self.send(Update::Item(item));
        id
    }

    fn add(&self, item: Item) -> u64 {
        let mut state = self.lock();
        self.push(&mut state, item)
    }

    /// The main agent's text goes on.
    fn append(&self, state: &mut State, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        if let Some(id) = state.open_text
            && let Some(item) = state.items.back_mut()
            && item.id == id
        {
            item.text.push_str(chunk);
            state.bytes += chunk.len();
            self.send(Update::Append {
                id,
                text: chunk.to_string(),
            });
            return;
        }
        self.push(state, Item::new(Kind::Text, chunk));
    }

    fn set_activity(&self, change: impl FnOnce(&mut Activity)) {
        let mut state = self.lock();
        let before = state.activity.clone();
        change(&mut state.activity);
        if state.activity != before {
            self.send(Update::Activity);
        }
    }

    /// A workflow's event: `label` names the worker whose workflow sent
    /// it (the runs of other agents than the main one are workers too).
    pub fn event(&self, event: &WorkflowEvent, label: Option<&str>) {
        let main = label.is_none() && event.agent_name == self.inner.main;
        let mut state = self.lock();
        let who = |state: &mut State| match label {
            Some(label) => label.to_string(),
            None => {
                let count = state.runs.len() + 1;
                state
                    .runs
                    .entry(event.agent_run_id)
                    .or_insert_with(|| format!("worker {count}"))
                    .clone()
            }
        };
        match &event.event {
            AgentEvent::AssistantResponseChunk(text) if main => self.append(&mut state, text),
            AgentEvent::ToolCallFormatted {
                tool_call_id,
                tool_name,
                parameters,
            } => {
                if main {
                    if let Some(item) = render::call(tool_name, parameters) {
                        self.push(&mut state, item);
                    }
                    if render::shows_result(tool_name) {
                        state.calls.insert(*tool_call_id, tool_name.clone());
                    }
                } else if let Some((text, detail)) = render::worker_call(tool_name, parameters) {
                    let who = who(&mut state);
                    let item = Item::new(Kind::Worker, text).who(who);
                    let item = match detail {
                        Some(detail) => item.detail(detail),
                        None => item,
                    };
                    self.push(&mut state, item);
                }
            }
            AgentEvent::ToolResult {
                tool_call_id,
                tool_name,
                output,
            } if main => {
                if state.calls.remove(tool_call_id).is_some()
                    && let Some(item) = render::result(tool_name, output)
                {
                    self.push(&mut state, item);
                }
            }
            AgentEvent::ErrorEncountered {
                error_message,
                tool_call_id,
                ..
            } => {
                if let Some(id) = tool_call_id {
                    state.calls.remove(id);
                }
                let text = render::cut(error_message, 600);
                let item = if main {
                    Item::new(Kind::Notice, text)
                } else {
                    Item::new(Kind::Worker, text).who(who(&mut state))
                };
                self.push(&mut state, item.level(Level::Error));
            }
            AgentEvent::RetryScheduled {
                attempt,
                delay_ms,
                rate_limit,
                ..
            } => {
                let why = if *rate_limit { "rate limited; " } else { "" };
                let text = format!(
                    "{why}retrying in {:.0} s (attempt {attempt})",
                    *delay_ms as f64 / 1000.0
                );
                let item = if main {
                    Item::new(Kind::Notice, text)
                } else {
                    Item::new(Kind::Worker, text).who(who(&mut state))
                };
                self.push(&mut state, item.level(Level::Warn));
            }
            AgentEvent::UsageLimitReached { error_message, .. } => {
                let text = format!("usage limit reached: {}", render::cut(error_message, 300));
                self.push(&mut state, Item::new(Kind::Notice, text).level(Level::Warn));
            }
            AgentEvent::OutputTokenLimitReached { output_tokens, .. } if main => {
                let text =
                    format!("the reply reached the model's output limit ({output_tokens} tokens)");
                self.push(&mut state, Item::new(Kind::Notice, text).level(Level::Warn));
            }
            AgentEvent::HandoverRequested { input_tokens } if main => {
                let text = format!(
                    "the conversation reached {}k tokens; the agent is writing a handover report",
                    input_tokens / 1000
                );
                self.push(&mut state, Item::new(Kind::Notice, text));
            }
            AgentEvent::ConversationHandedOver { .. } if main => {
                let text =
                    "handed over: the agent continues in a fresh conversation from its report";
                self.push(&mut state, Item::new(Kind::Notice, text));
            }
            AgentEvent::RefusalReceived(refusal) => {
                let text = match &refusal.category {
                    Some(category) => format!("the model refused ({category})"),
                    None => "the model refused".to_string(),
                };
                self.push(&mut state, Item::new(Kind::Notice, text).level(Level::Warn));
            }
            AgentEvent::TaskStateChanged(task) if !main => {
                let item = match task {
                    TaskState::Completed { summary } => {
                        Item::new(Kind::Worker, "finished").detail(summary.trim())
                    }
                    TaskState::Failed { reason, .. } => Item::new(
                        Kind::Worker,
                        format!("failed: {}", render::cut(reason, 300)),
                    )
                    .level(Level::Error),
                    TaskState::Cancelled { .. } => Item::new(Kind::Worker, "stopped"),
                    _ => return,
                };
                let who = who(&mut state);
                self.push(&mut state, item.who(who));
            }
            _ => {}
        }
    }

    /// A workflow's events were missed (the printer fell behind).
    pub fn missed(&self, count: u64) {
        self.notice(Level::Warn, &format!("({count} events not shown)"));
    }

    /// The user's text, and what became of it.
    pub fn user(&self, text: &str, source: Source, delivery: Delivery) {
        let mut item = Item::new(Kind::User, text.trim());
        item.source = Some(source);
        item.delivery = Some(delivery);
        self.add(item);
    }

    /// What fin says about the session.
    pub fn notice(&self, level: Level, text: &str) {
        let text = text.trim();
        if !text.is_empty() {
            self.add(Item::new(Kind::Notice, text).level(level));
        }
    }

    /// A command's output.
    pub fn output(&self, text: &str) {
        if !text.trim().is_empty() {
            self.add(Item::new(Kind::Output, text.trim_end()));
        }
    }

    /// The main agent's reply.
    pub fn reply(&self, text: &str) {
        self.add(Item::new(Kind::Reply, text.trim()));
    }

    pub fn turn_started(&self) {
        self.set_activity(|activity| activity.working = true);
    }

    /// The turn ended; `summary` is what it took (time, cost, context).
    pub fn turn_ended(&self, summary: &str) {
        self.add(Item::new(Kind::Turn, summary));
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(agent: &str, event: AgentEvent) -> WorkflowEvent {
        WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: agent.into(),
            agent_run_id: Uuid::nil(),
            event,
        }
    }

    /// The main agent's text streams into one item, which a call ends; a
    /// shell command's result is shown in brief; a worker's call is its
    /// activity.
    #[test]
    fn events_become_items() {
        let feed = Feed::new("main");
        let (_, _, mut updates) = feed.subscribe();
        feed.event(
            &event("main", AgentEvent::AssistantResponseChunk("Hel".into())),
            None,
        );
        feed.event(
            &event("main", AgentEvent::AssistantResponseChunk("lo".into())),
            None,
        );
        let call = Uuid::new_v4();
        feed.event(
            &event(
                "main",
                AgentEvent::ToolCallFormatted {
                    tool_call_id: call,
                    tool_name: crate::shell::SHELL_TOOL_NAME.into(),
                    parameters: [("command".to_string(), "cargo test".into())].into(),
                },
            ),
            None,
        );
        feed.event(
            &event(
                "main",
                AgentEvent::ToolResult {
                    tool_call_id: call,
                    tool_name: crate::shell::SHELL_TOOL_NAME.into(),
                    output: "exit code 1, 2.0 s\nerror: failed".into(),
                },
            ),
            None,
        );
        feed.event(
            &event(
                "worker",
                AgentEvent::ToolCallFormatted {
                    tool_call_id: Uuid::new_v4(),
                    tool_name: "read_file".into(),
                    parameters: [("path".to_string(), "src/a.rs".into())].into(),
                },
            ),
            Some("w1"),
        );
        // A worker's text isn't shown.
        feed.event(
            &event("worker", AgentEvent::AssistantResponseChunk("x".into())),
            Some("w1"),
        );
        let (items, _, _) = feed.subscribe();
        let shown: Vec<(Kind, &str, Option<&str>)> = items
            .iter()
            .map(|item| (item.kind, item.text.as_str(), item.who.as_deref()))
            .collect();
        assert_eq!(
            shown,
            [
                (Kind::Text, "Hello", None),
                (Kind::Call, "$ cargo test", None),
                (Kind::Result, "exit code 1, 2.0 s", None),
                (Kind::Worker, "read src/a.rs", Some("w1")),
            ]
        );
        assert_eq!(items[2].level, Some(Level::Warn));
        assert_eq!(items[2].detail.as_deref(), Some("error: failed"));
        assert!(matches!(updates.try_recv(), Ok(Update::Item(item)) if item.text == "Hel"));
        assert!(
            matches!(updates.try_recv(), Ok(Update::Append { id, text }) if id == 1 && text == "lo")
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
        feed.turn_ended("3 s");
        assert_eq!(feed.activity(), Activity::default());
    }

    #[test]
    fn the_history_is_bounded() {
        let feed = Feed::new("main");
        for n in 0..MAX_ITEMS + 10 {
            feed.notice(Level::Info, &format!("notice {n}"));
        }
        let (items, _, _) = feed.subscribe();
        assert_eq!(items.len(), MAX_ITEMS);
        assert_eq!(items[0].text, "notice 10");
        let big = "x".repeat(MAX_BYTES / 4);
        for _ in 0..6 {
            feed.output(&big);
        }
        let (items, _, _) = feed.subscribe();
        assert!(items.iter().map(Item::size).sum::<usize>() <= MAX_BYTES);
    }
}

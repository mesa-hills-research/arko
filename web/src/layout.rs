//! How the transcript is laid out for each view, and what shows that the
//! agent is under way.

use std::collections::{HashMap, HashSet};

use crate::protocol::{CallStatus, Delivery, ItemData, Kind, Level, StatusData, WorkerState};
use crate::store::{MAIN, Store};

/// The transcript's views.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// Everything, with the workers' calls inline.
    Full,
    /// Each worker as one line that updates in place.
    Quiet,
    /// Just the messages, with a count of each turn's tool calls.
    Messages,
}

impl View {
    pub const ALL: [View; 3] = [View::Full, View::Quiet, View::Messages];

    pub fn name(self) -> &'static str {
        match self {
            View::Full => "Full",
            View::Quiet => "Quiet",
            View::Messages => "Messages",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            View::Full => "Everything, with the workers’ calls inline.",
            View::Quiet => "Each worker as one line that updates in place.",
            View::Messages => "Just the messages, with a count of each turn’s tool calls.",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            View::Full => "full",
            View::Quiet => "quiet",
            View::Messages => "messages",
        }
    }

    pub fn from_key(key: &str) -> Option<View> {
        View::ALL.into_iter().find(|view| view.key() == key)
    }
}

/// A turn's tool calls, folded into one line (Messages).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Count {
    pub calls: u32,
    pub failed: u32,
    pub running: u32,
}

/// A row of a view.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Item(u64),
    /// The count's first call's ID, and the count.
    Count {
        turn: Option<u64>,
        id: u64,
        count: Count,
    },
    /// The agent waits on its model.
    Working,
}

impl Row {
    /// The row's identity, kept across layouts.
    pub fn key(&self) -> String {
        match self {
            Row::Item(id) => format!("i{id}"),
            Row::Count { turn, .. } => format!(
                "c{}",
                turn.map_or("none".to_string(), |turn| turn.to_string())
            ),
            Row::Working => "working".into(),
        }
    }

    /// The item it stands for (a count's first call).
    pub fn id(&self) -> Option<u64> {
        match self {
            Row::Item(id) | Row::Count { id, .. } => Some(*id),
            Row::Working => None,
        }
    }
}

/// Whether a notice (or a failed call) is a warning or an error.
pub fn serious(item: &ItemData) -> bool {
    matches!(item.level, Some(Level::Warn | Level::Error))
}

/// What the Messages view keeps of the main transcript.
fn shown_in_messages(item: &ItemData) -> bool {
    match item.kind {
        Kind::User
        | Kind::Text
        | Kind::Message
        | Kind::Reply
        | Kind::Question
        | Kind::Output
        | Kind::Turn
        | Kind::WorkerLine => true,
        Kind::Notice => serious(item),
        _ => false,
    }
}

/// `items` with each note the agent hasn't read yet at the end of its
/// conversation, below the agent's output, which goes on above it.
fn pending_last(items: &[ItemData]) -> Vec<&ItemData> {
    let mut out = Vec::with_capacity(items.len());
    let mut waiting = Vec::new();
    for item in items {
        if item.pending {
            waiting.push(item);
            continue;
        }
        if item.kind == Kind::Conversation {
            out.append(&mut waiting);
        }
        out.push(item);
    }
    out.extend(waiting);
    out
}

/// The rows `view` shows, in order. `worker` picks a worker's own
/// transcript instead of the main one.
pub fn layout(all: &[ItemData], view: View, worker: Option<&str>) -> Vec<Row> {
    let items = pending_last(all);
    // A result shows under its call, which other items may have followed
    // by the time it came.
    let mut results: HashMap<u64, Vec<u64>> = HashMap::new();
    for item in &items {
        if item.kind == Kind::Result
            && let Some(call) = item.call
        {
            results.entry(call).or_default().push(item.id);
        }
    }
    let placed: HashSet<u64> = results.values().flatten().copied().collect();
    let mut rows = Vec::new();
    let push = |rows: &mut Vec<Row>, item: &ItemData| {
        rows.push(Row::Item(item.id));
        for &result in results.get(&item.id).into_iter().flatten() {
            rows.push(Row::Item(result));
        }
    };
    let ids: HashSet<u64> = items.iter().map(|item| item.id).collect();
    // A result whose call is gone shows where it came.
    let orphan = |item: &ItemData| item.call.is_some_and(|call| !ids.contains(&call));
    if let Some(worker) = worker {
        for item in &items {
            if item.agent == worker && (!placed.contains(&item.id) || orphan(item)) {
                push(&mut rows, item);
            }
        }
        return rows;
    }
    let mut counts: HashMap<Option<u64>, usize> = HashMap::new();
    for item in &items {
        if placed.contains(&item.id) && !orphan(item) {
            continue;
        }
        let main = item.agent == MAIN;
        match view {
            View::Full => {
                // The workers' calls and troubles inline. Their text, their
                // reports and what the user wrote to them are in their own
                // views.
                if main {
                    push(&mut rows, item);
                } else if item.kind == Kind::Call || (item.kind == Kind::Notice && serious(item)) {
                    rows.push(Row::Item(item.id));
                }
            }
            View::Quiet => {
                if main {
                    push(&mut rows, item);
                }
            }
            View::Messages => {
                if !main {
                    continue;
                }
                if shown_in_messages(item) {
                    rows.push(Row::Item(item.id));
                    continue;
                }
                if item.kind != Kind::Call {
                    continue;
                }
                // The turn's calls, as one line where the first one was.
                let at = *counts.entry(item.turn).or_insert_with(|| {
                    rows.push(Row::Count {
                        turn: item.turn,
                        id: item.id,
                        count: Count::default(),
                    });
                    rows.len() - 1
                });
                if let Row::Count { count, .. } = &mut rows[at] {
                    count.calls += 1;
                    match item.status {
                        Some(CallStatus::Failed) => count.failed += 1,
                        Some(CallStatus::Running) => count.running += 1,
                        _ => {}
                    }
                }
            }
        }
    }
    rows
}

/// The first line of an agent's thinking, for its folded row: the first
/// one with text, without Markdown's bold marks.
pub fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .replace("**", "")
}

/// Text that hasn't grown for this long, while its agent works, no longer
/// shows that the agent is under way (it may be writing a long call).
pub const QUIET_MS: f64 = 2000.0;

/// What an agent that works is under way with, as its latest items say.
#[derive(Debug, PartialEq)]
enum Busy {
    /// It waits on its model, and nothing the model sent shows it yet.
    Model,
    /// Its thinking or text streams.
    Thinking(u64),
    Text(u64),
    /// A call of its runs.
    Tool,
    /// It replied.
    Done,
}

fn busy(items: &[ItemData], agent: &str, since: i64) -> Busy {
    // The calls of one response may run side by side, so one that ended
    // doesn't mean the others did.
    let mut after_call = false;
    for item in items.iter().rev() {
        if item.agent != agent || item.pending {
            continue;
        }
        match item.kind {
            Kind::Call => {
                if item.status == Some(CallStatus::Running) {
                    return Busy::Tool;
                }
                after_call = true;
                continue;
            }
            Kind::Result => {
                after_call = true;
                continue;
            }
            // Arko's lines, a command's output and the workers' places.
            Kind::Notice | Kind::Output | Kind::WorkerLine => continue,
            // What the user typed beside the agent's work.
            Kind::User if matches!(item.delivery, Some(Delivery::Command | Delivery::Queued)) => {
                continue;
            }
            _ => {}
        }
        if after_call {
            return Busy::Model;
        }
        return match item.kind {
            Kind::Thinking => Busy::Thinking(item.id),
            Kind::Text => Busy::Text(item.id),
            Kind::Reply => Busy::Done,
            // The end of a turn before this one began, or of this one.
            Kind::Turn if item.at < since => Busy::Model,
            Kind::Turn => Busy::Done,
            _ => Busy::Model,
        };
    }
    Busy::Model
}

/// What shows that the agent of a view is under way, besides its calls:
/// the ID of its thinking that streams, whose label moves, and whether a
/// "Working…" row stands at the end while it waits on its model with
/// nothing else to show. `now` is this device's time, as the store's
/// `grown` has it.
pub fn motion(
    store: &Store,
    status: &StatusData,
    view: View,
    worker: Option<&str>,
    now: f64,
) -> (Option<u64>, bool) {
    let still = (None, false);
    let mut since = i64::MIN;
    if let Some(worker) = worker {
        if store
            .workers
            .get(worker)
            .is_none_or(|summary| summary.state != WorkerState::Running)
        {
            return still;
        }
    } else {
        // While the agent asks, it waits on the user, not on its model.
        if !status.working || status.question.is_some() {
            return still;
        }
        if let Some(at) = status.since {
            since = at;
        }
    }
    match busy(&store.items, worker.unwrap_or(MAIN), since) {
        Busy::Model => (None, true),
        Busy::Text(id) => {
            let grown = store.grown.get(&id).copied().unwrap_or(now);
            (None, now - grown >= QUIET_MS)
        }
        // The Messages view leaves the agent's thinking out.
        Busy::Thinking(_) if worker.is_none() && view == View::Messages => (None, true),
        Busy::Thinking(id) => (Some(id), false),
        Busy::Tool | Busy::Done => still,
    }
}

/// `rows` with the "Working…" row, when `working`, where the agent's
/// output comes next: at the end, above the notes it hasn't read.
pub fn with_working(mut rows: Vec<Row>, working: bool, store: &Store) -> Vec<Row> {
    if !working {
        return rows;
    }
    let mut at = rows.len();
    while at > 0
        && let Row::Item(id) = rows[at - 1]
        && store.get(id).is_some_and(|item| item.pending)
    {
        at -= 1;
    }
    rows.insert(at, Row::Working);
    rows
}

/// How many of `rows` came after the item `seen` ("Working…" isn't new).
pub fn unseen(rows: &[Row], seen: u64) -> usize {
    rows.iter()
        .filter(|row| row.id().is_some_and(|id| id > seen))
        .count()
}

/// Whether the text of one of the items `grown` shows in `rows`. A count
/// row shows no text, and "Working…" isn't an item.
fn grew(rows: &[Row], grown: &HashSet<u64>) -> bool {
    rows.iter()
        .any(|row| matches!(row, Row::Item(id) if grown.contains(id)))
}

/// What Latest says. It counts the rows that came after the item `seen`
/// and, with none, says when text grew in a row of the view. Both read the
/// same `rows`, so they agree with what the view shows.
pub fn jump_label(rows: &[Row], seen: u64, grown: &HashSet<u64>) -> String {
    let fresh = unseen(rows, seen);
    if fresh > 0 {
        format!("{fresh} new")
    } else if grew(rows, grown) {
        "New text".into()
    } else {
        "Latest".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{item, worker};

    fn ids(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::key).collect()
    }

    #[test]
    fn views_keep_what_they_say_they_keep() {
        let mut call = item(3, Kind::Call, MAIN, "ls");
        call.status = Some(CallStatus::Failed);
        let mut worker_call = item(5, Kind::Call, "r1", "cat");
        worker_call.status = Some(CallStatus::Ok);
        let items = vec![
            item(1, Kind::User, MAIN, "hi"),
            item(2, Kind::Thinking, MAIN, "hmm"),
            call,
            item(4, Kind::Text, "r1", "worker text"),
            worker_call,
            item(6, Kind::Reply, MAIN, "done"),
        ];
        assert_eq!(
            ids(&layout(&items, View::Full, None)),
            ["i1", "i2", "i3", "i5", "i6"]
        );
        assert_eq!(
            ids(&layout(&items, View::Quiet, None)),
            ["i1", "i2", "i3", "i6"]
        );
        let messages = layout(&items, View::Messages, None);
        assert_eq!(ids(&messages), ["i1", "c1", "i6"]);
        assert_eq!(
            messages[1],
            Row::Count {
                turn: Some(1),
                id: 3,
                count: Count {
                    calls: 1,
                    failed: 1,
                    running: 0
                }
            }
        );
        assert_eq!(ids(&layout(&items, View::Full, Some("r1"))), ["i4", "i5"]);
    }

    #[test]
    fn a_result_shows_under_its_call_whatever_came_between() {
        let mut result = item(4, Kind::Result, MAIN, "output");
        result.call = Some(2);
        let items = vec![
            item(1, Kind::User, MAIN, "go"),
            item(2, Kind::Call, MAIN, "make"),
            item(3, Kind::Text, MAIN, "meanwhile"),
            result,
        ];
        assert_eq!(
            ids(&layout(&items, View::Full, None)),
            ["i1", "i2", "i4", "i3"]
        );
    }

    #[test]
    fn a_note_the_agent_hasnt_read_waits_at_the_end_of_its_conversation() {
        let mut note = item(2, Kind::User, MAIN, "also this");
        note.pending = true;
        note.delivery = Some(Delivery::Note);
        let items = vec![
            item(1, Kind::User, MAIN, "start"),
            note,
            item(3, Kind::Text, MAIN, "working on it"),
            item(4, Kind::Conversation, MAIN, "New conversation"),
            item(5, Kind::User, MAIN, "next"),
        ];
        assert_eq!(
            ids(&layout(&items, View::Full, None)),
            ["i1", "i3", "i2", "i4", "i5"]
        );
    }

    #[test]
    fn working_shows_while_the_agent_waits_on_its_model_and_its_thinking_moves_as_it_streams() {
        let mut store = Store::default();
        let status = StatusData {
            working: true,
            since: Some(0),
            ..StatusData::default()
        };
        store.add(item(1, Kind::User, MAIN, "go"), 0.0);
        assert_eq!(motion(&store, &status, View::Full, None, 0.0), (None, true));
        store.add(item(2, Kind::Thinking, MAIN, "hmm"), 0.0);
        assert_eq!(
            motion(&store, &status, View::Full, None, 0.0),
            (Some(2), false)
        );
        assert_eq!(
            motion(&store, &status, View::Messages, None, 0.0),
            (None, true)
        );
        store.add(item(3, Kind::Text, MAIN, "So"), 100.0);
        assert_eq!(
            motion(&store, &status, View::Full, None, 500.0),
            (None, false)
        );
        assert_eq!(
            motion(&store, &status, View::Full, None, 100.0 + QUIET_MS),
            (None, true)
        );
        let mut call = item(4, Kind::Call, MAIN, "ls");
        call.status = Some(CallStatus::Running);
        store.add(call.clone(), 0.0);
        assert_eq!(
            motion(&store, &status, View::Full, None, 0.0),
            (None, false)
        );
        call.status = Some(CallStatus::Ok);
        store.change(call, 0.0);
        assert_eq!(motion(&store, &status, View::Full, None, 0.0), (None, true));
        store.add(item(5, Kind::Reply, MAIN, "done"), 0.0);
        assert_eq!(
            motion(&store, &status, View::Full, None, 0.0),
            (None, false)
        );
        // While it asks, it waits on the user.
        let asking = StatusData {
            question: Some(crate::protocol::Question {
                id: 1,
                text: "?".into(),
            }),
            ..status
        };
        assert_eq!(
            motion(&store, &asking, View::Full, None, 0.0),
            (None, false)
        );
    }

    #[test]
    fn a_workers_own_view_says_it_works_while_it_runs() {
        let mut store = Store::default();
        store.worker(worker("r1"));
        store.add(item(1, Kind::User, "r1", "task"), 0.0);
        let status = StatusData::default();
        assert_eq!(
            motion(&store, &status, View::Full, Some("r1"), 0.0),
            (None, true)
        );
        store.worker(crate::protocol::WorkerData {
            state: WorkerState::Finished,
            ..worker("r1")
        });
        assert_eq!(
            motion(&store, &status, View::Full, Some("r1"), 0.0),
            (None, false)
        );
    }

    #[test]
    fn working_stands_above_the_notes_the_agent_hasnt_read_and_isnt_new() {
        let mut store = Store::default();
        store.add(item(1, Kind::User, MAIN, "go"), 0.0);
        let mut note = item(2, Kind::User, MAIN, "and this");
        note.pending = true;
        store.add(note, 0.0);
        let rows = with_working(layout(&store.items, View::Full, None), true, &store);
        assert_eq!(ids(&rows), ["i1", "working", "i2"]);
        assert_eq!(unseen(&rows, 1), 1);
    }

    #[test]
    fn thinking_folds_to_its_first_line_without_bold_marks() {
        assert_eq!(
            first_line("\n  **Planning** the fix\nmore"),
            "Planning the fix"
        );
    }

    /// A conversation with the main agent's thinking and reply, and a worker
    /// that streams text and makes a call.
    fn streaming() -> Vec<ItemData> {
        let mut worker_call = item(5, Kind::Call, "r1", "cat");
        worker_call.status = Some(CallStatus::Ok);
        vec![
            item(1, Kind::User, MAIN, "hi"),
            item(2, Kind::Thinking, MAIN, "hmm"),
            item(3, Kind::Text, "r1", "worker text"),
            item(4, Kind::Text, MAIN, "an answer"),
            worker_call,
        ]
    }

    fn label(items: &[ItemData], view: View, worker: Option<&str>, grown: &[u64]) -> String {
        let grown = grown.iter().copied().collect();
        jump_label(&layout(items, view, worker), 5, &grown)
    }

    #[test]
    fn a_workers_text_isnt_new_text_in_the_main_view() {
        let items = streaming();
        for view in View::ALL {
            assert_eq!(label(&items, view, None, &[3]), "Latest", "{view:?}");
        }
        // It is in the worker's own view.
        assert_eq!(label(&items, View::Full, Some("r1"), &[3]), "New text");
    }

    #[test]
    fn text_a_view_hides_isnt_new_text_in_it() {
        let items = streaming();
        // Messages leaves the agent's thinking out.
        assert_eq!(label(&items, View::Messages, None, &[2]), "Latest");
        assert_eq!(label(&items, View::Full, None, &[2]), "New text");
        assert_eq!(label(&items, View::Quiet, None, &[2]), "New text");
        // The main agent's text isn't in a worker's view.
        assert_eq!(label(&items, View::Full, Some("r1"), &[4]), "Latest");
    }

    #[test]
    fn a_visible_reply_that_grows_is_new_text_in_every_view() {
        let items = streaming();
        for view in View::ALL {
            assert_eq!(label(&items, view, None, &[4]), "New text", "{view:?}");
        }
        // A worker's text beside it changes nothing.
        assert_eq!(label(&items, View::Full, None, &[3, 4]), "New text");
    }

    #[test]
    fn latest_counts_new_rows_before_it_says_text_grew() {
        let mut items = streaming();
        let grown: HashSet<u64> = [4].into();
        let rows = layout(&items, View::Full, None);
        assert_eq!(jump_label(&rows, 4, &grown), "1 new");
        assert_eq!(jump_label(&rows, 5, &grown), "New text");
        // A worker's text that came since isn't new in the main view.
        items.push(item(6, Kind::Text, "r1", "more"));
        let rows = layout(&items, View::Full, None);
        assert_eq!(jump_label(&rows, 5, &HashSet::new()), "Latest");
    }

    #[test]
    fn a_count_row_shows_no_text() {
        let mut call = item(3, Kind::Call, MAIN, "ls");
        call.status = Some(CallStatus::Ok);
        let items = vec![item(1, Kind::User, MAIN, "go"), call];
        let rows = layout(&items, View::Messages, None);
        assert_eq!(ids(&rows), ["i1", "c1"]);
        assert_eq!(jump_label(&rows, 3, &[3].into()), "Latest");
    }
}

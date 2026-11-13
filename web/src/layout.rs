//! How the transcript is laid out for each view, and what shows that the
//! agent is under way.

use std::collections::{HashMap, HashSet};

use crate::protocol::{CallStatus, Delivery, ItemData, Kind, Level, StatusData, WorkerState};
use crate::quiet::{self, Words};
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
            View::Quiet => "Each call in a few words, and each worker as one line.",
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

/// What a Quiet row shows ([`quiet::rows`]).
#[derive(Clone, Debug, PartialEq)]
pub struct QuietRow {
    pub words: Words,
    /// The rows of its calls and their results, and the thinking after
    /// them, whose content shows when it's open ([`quiet_members`]).
    pub calls: Vec<Row>,
    /// It holds several calls, each of which shows as its Full line when
    /// it's open.
    pub merged: bool,
    /// It has more to show than its words. One that hasn't has no fold.
    pub opens: bool,
}

/// A row of a view.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// An item, and its kind, which decides how its row is drawn.
    Item(u64, Kind),
    /// The count's first call's ID, and the count.
    Count {
        turn: Option<u64>,
        id: u64,
        count: Count,
    },
    /// Calls in a few words (Quiet), by the ID of the first, and what the
    /// row shows.
    Quiet { id: u64, row: QuietRow },
    /// A worker's report or message to its lead in the main transcript, as
    /// a row that opens to its words ([`quiet::WorkerRow`]).
    Status(u64),
    /// The agent waits on its model.
    Working,
}

impl Row {
    /// The row's identity, kept across layouts.
    pub fn key(&self) -> String {
        match self {
            Row::Item(id, _) => format!("i{id}"),
            // (Its first call too, since two workers' counts of the same turn
            // are other rows.)
            Row::Count { turn, id, .. } => format!(
                "c{}:{id}",
                turn.map_or("none".to_string(), |turn| turn.to_string())
            ),
            Row::Quiet { id, .. } => format!("q{id}"),
            Row::Status(id) => format!("s{id}"),
            Row::Working => "working".into(),
        }
    }
}

/// Whether a notice (or a failed call) is a warning or an error.
pub fn serious(item: &ItemData) -> bool {
    matches!(item.level, Some(Level::Warn | Level::Error))
}

/// Whether an item that changed from `old` to `new` moves the rows of
/// `view` or their keys, as it does when what [`layout`] and
/// [`with_working`] read of it changed. A change of its text, outcome or
/// images redraws its row in place.
pub fn moves_rows(old: &ItemData, new: &ItemData, view: View) -> bool {
    old.kind != new.kind
        || old.agent != new.agent
        || old.pending != new.pending
        || old.call != new.call
        || old.turn != new.turn
        || old.started.is_some() != new.started.is_some()
        || old.from.is_some() != new.from.is_some()
        || serious(old) != serious(new)
        // A call's status moves a Messages count, and what a call did and
        // how it went words its Quiet row, which a count that's open shows.
        || (view != View::Full
            && (old.status != new.status
                || old.act != new.act
                || old.outcome != new.outcome
                || old.job != new.job))
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
fn pending_last<'a>(items: impl IntoIterator<Item = &'a ItemData>) -> Vec<&'a ItemData> {
    let mut out = Vec::new();
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

/// What a view does with an item.
enum Place {
    Hidden,
    /// A row of its own.
    Row,
    /// Counted in its turn's tool activity (Messages).
    Count,
}

/// What `view` does with an item of the agent whose transcript it is, the
/// main agent's in the main transcript and a worker's in its own. One rule
/// serves both, so a worker's view can't differ from the selected one.
fn place(view: View, item: &ItemData) -> Place {
    match view {
        View::Full => Place::Row,
        // The worker's own line stands for the call that started it. A call
        // that is still running, was refused or queued its worker shows.
        View::Quiet if item.kind == Kind::Call && item.started.is_some() => Place::Hidden,
        View::Quiet => Place::Row,
        View::Messages if shown_in_messages(item) => Place::Row,
        // The turn's calls, as one line where the first one was.
        View::Messages if item.kind == Kind::Call => Place::Count,
        View::Messages => Place::Hidden,
    }
}

/// The rows `view` shows, in order. `worker` picks a worker's own
/// transcript instead of the main one.
#[cfg(test)]
pub fn layout(all: &[ItemData], view: View, worker: Option<&str>) -> Vec<Row> {
    layout_sealed(
        all,
        view,
        worker,
        &HashSet::new(),
        &HashMap::new(),
        &HashSet::new(),
    )
}

/// [`layout`], where `sealed` are the calls whose Quiet rows of one call
/// the reader opened, which keep their form ([`quiet::Call::sealed`]), and
/// the thoughts the reader opened on lines of their own, which stay there,
/// `leads` the workers that a worker started, by the worker that started
/// each, whose reports and messages go to that lead's view, and
/// `counts_open` the turns whose count of
/// calls the reader opened in Messages, by their first call. From that call
/// on, such a turn's items show as Quiet shows them, each where it came.
pub fn layout_sealed(
    all: &[ItemData],
    view: View,
    worker: Option<&str>,
    sealed: &HashSet<u64>,
    leads: &HashMap<String, String>,
    counts_open: &HashSet<u64>,
) -> Vec<Row> {
    // The agent whose view it is, and the lead of each worker, the main
    // agent unless a worker started it.
    let viewed = worker.unwrap_or(MAIN);
    let lead_of = |agent: &str| leads.get(agent).map_or(MAIN, String::as_str);
    // A worker of the agent shows as its reports and its messages to it, its
    // lead, so only those of the workers' many items are looked at here.
    let worker_row = |item: &ItemData| {
        quiet::WorkerRow::of(
            item.kind,
            item.act.as_ref(),
            item.status,
            item.outcome.as_deref(),
            viewed,
        )
        .is_some()
            && lead_of(&item.agent) == viewed
    };
    let items = pending_last(
        all.iter()
            .filter(|item| item.agent == viewed || worker_row(item)),
    );
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
    let ids: HashSet<u64> = items.iter().map(|item| item.id).collect();
    // A result whose call is gone shows where it came.
    let orphan = |item: &ItemData| item.call.is_some_and(|call| !ids.contains(&call));
    // Where each turn's count stands (Messages), at its first call, or at
    // the thinking that led to it with nothing Messages shows between them,
    // so open it shows the turn's work from its first thought, with that
    // first call, by which it's known.
    let mut began: HashMap<u64, u64> = HashMap::new();
    if view == View::Messages {
        let mut turns = HashSet::new();
        let mut thought = None;
        for item in &items {
            if item.agent != viewed || (placed.contains(&item.id) && !orphan(item)) {
                continue;
            }
            match place(view, item) {
                Place::Count => {
                    if turns.insert(item.turn) {
                        began.insert(thought.unwrap_or(item.id), item.id);
                    }
                    thought = None;
                }
                Place::Row => thought = None,
                Place::Hidden if item.kind == Kind::Thinking => {
                    thought.get_or_insert(item.id);
                }
                Place::Hidden => {}
            }
        }
    }
    let mut counts: HashMap<Option<u64>, usize> = HashMap::new();
    let mut opened: HashSet<Option<u64>> = HashSet::new();
    // Whether the turn so far has a row in this view. A turn Arko started
    // for news may end without a reply, and Messages leaves out its opening,
    // so its line would end a turn that shows nothing.
    let mut turn_shown = false;
    for item in &items {
        if placed.contains(&item.id) && !orphan(item) {
            continue;
        }
        // The agent's own items follow the view. In every view, a worker of
        // the agent shows as its reports and its messages to it, its lead,
        // each a row that opens to its words. Its text, its calls, its
        // conversation with the user and its workers are its own view's.
        if item.agent != viewed {
            turn_shown = true;
            rows.push(Row::Status(item.id));
            continue;
        }
        if let Some(&first) = began.get(&item.id) {
            turn_shown = true;
            rows.push(Row::Count {
                turn: item.turn,
                id: first,
                count: Count::default(),
            });
            counts.insert(item.turn, rows.len() - 1);
            if counts_open.contains(&first) {
                opened.insert(item.turn);
            }
        }
        let place = place(view, item);
        if let Place::Count = place
            && let Some(&at) = counts.get(&item.turn)
        {
            turn_shown = true;
            if let Row::Count { count, .. } = &mut rows[at] {
                count.calls += 1;
                match item.status {
                    Some(CallStatus::Failed) => count.failed += 1,
                    Some(CallStatus::Running) => count.running += 1,
                    _ => {}
                }
            }
        }
        let place = if opened.contains(&item.turn) {
            self::place(View::Quiet, item)
        } else {
            place
        };
        match place {
            Place::Row if matches!(item.kind, Kind::Turn | Kind::Conversation) => {
                if std::mem::take(&mut turn_shown) || item.kind == Kind::Conversation {
                    rows.push(Row::Item(item.id, item.kind));
                }
            }
            Place::Row => {
                turn_shown = true;
                rows.push(Row::Item(item.id, item.kind));
                for &result in results.get(&item.id).into_iter().flatten() {
                    rows.push(Row::Item(result, Kind::Result));
                }
            }
            Place::Hidden | Place::Count => {}
        }
    }
    if view == View::Quiet || !opened.is_empty() {
        rows = quiet_rows(rows, &items, sealed);
    }
    rows
}

/// The Quiet view's rows, with each run of the agent's calls as rows of a few
/// words ([`quiet::rows`], as the terminal makes them), and the rest as it
/// is. A call's results go in its row, and show under the row only when
/// the call failed alone.
fn quiet_rows(rows: Vec<Row>, items: &[&ItemData], sealed: &HashSet<u64>) -> Vec<Row> {
    let by_id: HashMap<u64, &ItemData> = items.iter().map(|item| (item.id, *item)).collect();
    // The rows, each with the results of its own.
    let mut shown: Vec<(Row, Vec<Row>)> = Vec::new();
    for row in rows {
        if let Row::Item(id, Kind::Result) = row
            && let Some((Row::Item(call, Kind::Call), results)) = shown.last_mut()
            && by_id.get(&id).and_then(|result| result.call) == Some(*call)
        {
            results.push(row);
            continue;
        }
        shown.push((row, Vec::new()));
    }
    let steps: Vec<quiet::Step> = shown
        .iter()
        .map(|(row, results)| match row {
            // (A note the agent hasn't read waits at the end.)
            Row::Item(id, _) if by_id.get(id).is_some_and(|item| item.pending) => {
                quiet::Step::Aside
            }
            Row::Item(id, Kind::Call) => match by_id.get(id) {
                Some(item) => quiet::Step::Call(quiet::Call {
                    id: item.id,
                    act: item.act.as_ref(),
                    status: item.status,
                    outcome: item.outcome.as_deref(),
                    job: item.job.is_some(),
                    turn: item.turn,
                    content: !results.is_empty(),
                    ended: item.ended,
                    sealed: sealed.contains(&item.id),
                }),
                None => quiet::Step::Other(*id),
            },
            // (One the reader opened on its own line stays there.)
            Row::Item(id, Kind::Thinking) if sealed.contains(id) => quiet::Step::Other(*id),
            Row::Item(_, Kind::Thinking) => quiet::Step::Thinking,
            Row::Item(id, _) | Row::Status(id) => quiet::Step::Other(*id),
            _ => quiet::Step::Other(0),
        })
        .collect();
    let mut out = Vec::with_capacity(shown.len());
    for piece in quiet::rows(&steps) {
        match piece {
            quiet::Piece::Step(at) => {
                let (row, results) = &shown[at];
                out.push(row.clone());
                out.extend(results.iter().cloned());
            }
            quiet::Piece::Row(row) => {
                let mut calls = Vec::new();
                for at in &row.members {
                    let (member, results) = &shown[*at];
                    calls.push(member.clone());
                    calls.extend(results.iter().cloned());
                }
                let id = match calls.first() {
                    Some(Row::Item(id, _)) => *id,
                    _ => continue,
                };
                out.push(Row::Quiet {
                    id,
                    row: QuietRow {
                        words: row.words,
                        calls,
                        merged: row.merged,
                        opens: row.opens,
                    },
                });
            }
        }
    }
    out
}

/// The calls of a Quiet row ([`QuietRow`]'s `calls`) as the open row
/// shows them, each with the rows of its results, which follow it there,
/// and the thinking between them by itself.
pub fn quiet_members(calls: &[Row]) -> Vec<(Row, Vec<Row>)> {
    let mut members: Vec<(Row, Vec<Row>)> = Vec::new();
    for row in calls {
        if let Row::Item(_, Kind::Result) = row
            && let Some((Row::Item(_, Kind::Call), results)) = members.last_mut()
        {
            results.push(row.clone());
            continue;
        }
        members.push((row.clone(), Vec::new()));
    }
    members
}

/// The first line of an agent's thinking, for its folded row, the first
/// one with text, without Markdown's bold marks, and whether lines with
/// text follow it.
pub fn first_line(text: &str) -> (String, bool) {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or("").replace("**", "");
    (first, lines.next().is_some())
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

/// What shows that the agent of a view is under way, besides its calls, namely
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
    let Some(since) = under_way(store, status, worker) else {
        return still;
    };
    match busy(&store.items, worker.unwrap_or(MAIN), since) {
        Busy::Model => (None, true),
        Busy::Text(id) => {
            let grown = store.grown.get(&id).copied().unwrap_or(now);
            (None, now - grown >= QUIET_MS)
        }
        // Messages shows the agent's thinking only where its turn's count
        // is open, so elsewhere the Working row stands in for it.
        Busy::Thinking(id) => (Some(id), view == View::Messages),
        // It counts calls, so the line at the end says what the agent's
        // call does while it runs (`working_label`).
        Busy::Tool if view == View::Messages => (None, true),
        Busy::Tool | Busy::Done => still,
    }
}

/// When the turn of the agent of a view began, while it works (the
/// earliest time for a worker's), else `None`.
fn under_way(store: &Store, status: &StatusData, worker: Option<&str>) -> Option<i64> {
    match worker {
        Some(worker) => store
            .workers
            .get(worker)
            .is_some_and(|summary| summary.state == WorkerState::Running)
            .then_some(i64::MIN),
        // While the agent asks, it waits on the user, not on its model.
        None if !status.working || status.question.is_some() => None,
        None => Some(status.since.unwrap_or(i64::MIN)),
    }
}

/// Whether the room the "Working…" row takes stays at the end, unseen,
/// because something else shows that the agent of a view works (a call
/// of its own that runs, its thinking or its text as they stream), so
/// nothing above moves as they come and go.
pub fn room(
    store: &Store,
    status: &StatusData,
    view: View,
    worker: Option<&str>,
    now: f64,
) -> bool {
    !motion(store, status, view, worker, now).1
        && under_way(store, status, worker).is_some_and(|since| {
            !matches!(
                busy(&store.items, worker.unwrap_or(MAIN), since),
                Busy::Done
            )
        })
}

/// What the "Working…" line says. While a call of the agent runs (which
/// the Messages view alone shows the line for) it says what the call does,
/// as its Quiet row would ("Waiting for r91…").
pub fn working_label(store: &Store, worker: Option<&str>) -> String {
    running(store, worker)
        .and_then(|item| item.act.as_ref())
        .map_or_else(
            || "Working…".to_string(),
            |act| format!("{}…", quiet::doing(act)),
        )
}

/// The latest call of the agent of a view (`worker`, or the main agent)
/// that runs.
fn running<'a>(store: &'a Store, worker: Option<&str>) -> Option<&'a ItemData> {
    let agent = worker.unwrap_or(MAIN);
    store.items.iter().rev().find(|item| {
        item.agent == agent
            && !item.pending
            && item.kind == Kind::Call
            && item.status == Some(CallStatus::Running)
    })
}

/// Whether the agent's call that runs has a row of its own among `rows`,
/// which shows that it runs, as it does in Messages where its turn's count
/// is open.
pub fn running_shown(rows: &[Row], store: &Store, worker: Option<&str>) -> bool {
    running(store, worker).is_some_and(|call| {
        in_quiet_row(rows, call.id) || rows.contains(&Row::Item(call.id, Kind::Call))
    })
}

/// Whether the item `id` is in a Quiet row of `rows` (a thinking after a
/// call is, from the start).
pub fn in_quiet_row(rows: &[Row], id: u64) -> bool {
    rows.iter().rev().any(|row| {
        matches!(row, Row::Quiet { row, .. }
            if row.calls.iter().any(|call| matches!(call, Row::Item(inner, _) if *inner == id)))
    })
}

/// `rows` with the "Working…" row, when `working`, where the agent's
/// output comes next, at the end, above the notes it hasn't read.
pub fn with_working(mut rows: Vec<Row>, working: bool, store: &Store) -> Vec<Row> {
    if !working {
        return rows;
    }
    let mut at = rows.len();
    while at > 0
        && let Row::Item(id, _) = rows[at - 1]
        && store.get(id).is_some_and(|item| item.pending)
    {
        at -= 1;
    }
    rows.insert(at, Row::Working);
    rows
}

/// Whether `item` is one thing to read, counted once by Latest. A call's
/// results and a command's output belong to what they follow. The end of a
/// turn, a new conversation and a worker's place are marks, and a notice
/// counts only when it opens a turn Arko started.
fn is_read(item: &ItemData) -> bool {
    match item.kind {
        Kind::User
        | Kind::Text
        | Kind::Thinking
        | Kind::Message
        | Kind::Call
        | Kind::Question
        | Kind::Reply => true,
        Kind::Notice => item.opens_turn,
        _ => false,
    }
}

/// How many of the things `rows` show came after the item `seen`. A
/// turn's calls folded into one line (Messages) are one thing, where the
/// first of them came, and "Working…" isn't new.
pub fn unseen(rows: &[Row], store: &Store, seen: u64) -> usize {
    rows.iter()
        .filter(|row| match row {
            Row::Item(id, _) => *id > seen && store.get(*id).is_some_and(is_read),
            Row::Count { id, .. } | Row::Status(id) => *id > seen,
            // As one thing, new when a call of it is.
            Row::Quiet { row, .. } => row
                .calls
                .iter()
                .any(|row| matches!(row, Row::Item(id, Kind::Call) if *id > seen)),
            Row::Working => false,
        })
        .count()
}

/// Whether the text of one of the items `grown` shows in `rows`. A count
/// row shows no text, and "Working…" isn't an item.
fn grew(rows: &[Row], grown: &HashSet<u64>) -> bool {
    rows.iter()
        .any(|row| matches!(row, Row::Item(id, _) if grown.contains(id)))
}

/// What Latest says. It counts the things that came after the item `seen`
/// and, with none, says when text grew in a row of the view. Both read the
/// same `rows`, so they agree with what the view shows.
pub fn jump_label(rows: &[Row], store: &Store, seen: u64, grown: &HashSet<u64>) -> String {
    let fresh = unseen(rows, store, seen);
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
        // (A worker's calls are its own view's.)
        assert_eq!(
            ids(&layout(&items, View::Full, None)),
            ["i1", "i2", "i3", "i6"]
        );
        assert_eq!(
            ids(&layout(&items, View::Quiet, None)),
            ["i1", "i2", "i3", "i6"]
        );
        let messages = layout(&items, View::Messages, None);
        assert_eq!(ids(&messages), ["i1", "c1:3", "i6"]);
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

    /// Quiet leaves out the call that started a worker, whose own line
    /// stands for it. A call still running, refused or queued shows, and
    /// Full, Messages and the worker's own view keep every call.
    #[test]
    fn quiet_leaves_out_a_call_that_started_its_worker() {
        let start = |id: u64, status: CallStatus, started: Option<&str>| ItemData {
            status: Some(status),
            started: started.map(str::to_string),
            ..item(id, Kind::Call, MAIN, "start worker")
        };
        let mut nested = start(8, CallStatus::Ok, Some("r2"));
        nested.agent = "r1".into();
        let items = vec![
            item(1, Kind::User, MAIN, "go"),
            start(2, CallStatus::Ok, Some("r1")),
            item(3, Kind::WorkerLine, MAIN, "r1"),
            // Queued, refused and still waiting for the billing answer.
            start(4, CallStatus::Ok, None),
            start(5, CallStatus::Failed, None),
            start(6, CallStatus::Running, None),
            // Several starts in a row, a line for each.
            start(7, CallStatus::Ok, Some("r3")),
            nested,
            item(9, Kind::WorkerLine, MAIN, "r3"),
        ];
        assert_eq!(
            ids(&layout(&items, View::Quiet, None)),
            ["i1", "i3", "i4", "i5", "i6", "i9"]
        );
        // (The call r1 made is r1's view's.)
        assert_eq!(
            ids(&layout(&items, View::Full, None)),
            ["i1", "i2", "i3", "i4", "i5", "i6", "i7", "i9"]
        );
        let messages = layout(&items, View::Messages, None);
        assert_eq!(ids(&messages), ["i1", "c1:2", "i3", "i9"]);
        assert!(matches!(
            messages[1],
            Row::Count {
                count: Count {
                    calls: 5,
                    failed: 1,
                    running: 1
                },
                ..
            }
        ));
        // The worker's own view follows the view too. In Quiet the line of
        // the worker it started stands for the call, and Full keeps both.
        let mut line = item(10, Kind::WorkerLine, "r1", "r2");
        line.turn = None;
        let mut items = items;
        items.push(line);
        assert_eq!(ids(&layout(&items, View::Quiet, Some("r1"))), ["i10"]);
        assert_eq!(ids(&layout(&items, View::Full, Some("r1"))), ["i8", "i10"]);
    }

    /// A worker's own view is the selected view over its own items, as the
    /// main view is over the main agent's, with Full everything, Quiet without
    /// the call that started a worker, Messages its text, messages,
    /// replies and what the user wrote to it, with its calls counted as one
    /// line a turn.
    #[test]
    fn a_workers_own_view_follows_the_selected_view() {
        let call = |id: u64, status: CallStatus| ItemData {
            status: Some(status),
            turn: Some(1),
            ..item(id, Kind::Call, "r1", "cargo test")
        };
        let mut result = item(6, Kind::Result, "r1", "ok");
        result.call = Some(5);
        let items = vec![
            item(1, Kind::User, MAIN, "go"),
            item(2, Kind::User, "r1", "use tabs"),
            item(3, Kind::Thinking, "r1", "hmm"),
            item(4, Kind::Text, "r1", "On it."),
            call(5, CallStatus::Ok),
            result,
            call(7, CallStatus::Failed),
            item(8, Kind::Message, "r1", "Found it."),
            ItemData {
                level: Some(Level::Info),
                ..item(9, Kind::Notice, "r1", "stopped")
            },
            ItemData {
                level: Some(Level::Error),
                ..item(10, Kind::Notice, "r1", "failed")
            },
            item(11, Kind::Reply, "r1", "Fixed."),
            item(12, Kind::Text, MAIN, "main text"),
        ];
        let own = |view| layout(&items, view, Some("r1"));
        assert_eq!(
            ids(&own(View::Full)),
            ["i2", "i3", "i4", "i5", "i6", "i7", "i8", "i9", "i10", "i11"]
        );
        assert_eq!(ids(&own(View::Quiet)), ids(&own(View::Full)));
        let messages = own(View::Messages);
        assert_eq!(ids(&messages), ["i2", "i4", "c1:5", "i8", "i10", "i11"]);
        assert_eq!(
            messages[2],
            Row::Count {
                turn: Some(1),
                id: 5,
                count: Count {
                    calls: 2,
                    failed: 1,
                    running: 0
                }
            }
        );
        // The same rule, over the main agent's items, keeps the same kinds.
        let main: Vec<ItemData> = items[1..11]
            .iter()
            .cloned()
            .map(|item| ItemData {
                agent: MAIN.into(),
                ..item
            })
            .collect();
        for view in View::ALL {
            let shown = |rows: Vec<Row>| rows.iter().map(Row::key).collect::<Vec<_>>();
            assert_eq!(
                shown(layout(&main, view, None)),
                shown(own(view)),
                "{view:?}"
            );
        }
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

    /// Another agent's conversation isn't the viewed agent's, so its note
    /// waits at the end of its own.
    #[test]
    fn a_note_waits_past_another_agents_conversation() {
        let mut note = item(2, Kind::User, MAIN, "also this");
        note.pending = true;
        note.delivery = Some(Delivery::Note);
        let items = vec![
            item(1, Kind::User, MAIN, "start"),
            note,
            item(3, Kind::Conversation, "r1", "New conversation"),
            item(4, Kind::Text, MAIN, "working on it"),
        ];
        for view in [View::Full, View::Quiet] {
            assert_eq!(
                ids(&layout(&items, view, None)),
                ["i1", "i4", "i2"],
                "{view:?}"
            );
        }
    }

    /// The Working row shows while the agent waits on its model, and its
    /// room stays, unseen, while something else shows that the agent works
    /// (its thinking or text as they stream, a call that runs), so nothing
    /// above moves as they come and go.
    #[test]
    fn working_shows_while_the_agent_waits_on_its_model_and_its_thinking_moves_as_it_streams() {
        let mut store = Store::default();
        let status = StatusData {
            working: true,
            since: Some(0),
            ..StatusData::default()
        };
        let room = |store: &Store, view, now| room(store, &status, view, None, now);
        store.add(item(1, Kind::User, MAIN, "go"), 0.0);
        assert_eq!(motion(&store, &status, View::Full, None, 0.0), (None, true));
        assert!(!room(&store, View::Full, 0.0));
        store.add(item(2, Kind::Thinking, MAIN, "hmm"), 0.0);
        assert_eq!(
            motion(&store, &status, View::Full, None, 0.0),
            (Some(2), false)
        );
        assert!(room(&store, View::Full, 0.0));
        assert!(!room(&store, View::Messages, 0.0));
        // (Messages shows the Working row unless its open count shows the
        // thinking, which the transcript's rows say.)
        assert_eq!(
            motion(&store, &status, View::Messages, None, 0.0),
            (Some(2), true)
        );
        store.add(item(3, Kind::Text, MAIN, "So"), 100.0);
        assert_eq!(
            motion(&store, &status, View::Full, None, 500.0),
            (None, false)
        );
        assert!(room(&store, View::Full, 500.0));
        assert_eq!(
            motion(&store, &status, View::Full, None, 100.0 + QUIET_MS),
            (None, true)
        );
        assert!(!room(&store, View::Full, 100.0 + QUIET_MS));
        let mut call = item(4, Kind::Call, MAIN, "ls");
        call.status = Some(CallStatus::Running);
        call.act = Some(crate::protocol::Act {
            kind: crate::protocol::ActKind::Command,
            target: "ls".into(),
            ..Default::default()
        });
        store.add(call.clone(), 0.0);
        assert_eq!(
            motion(&store, &status, View::Full, None, 0.0),
            (None, false)
        );
        assert!(room(&store, View::Full, 0.0));
        assert!(room(&store, View::Quiet, 0.0));
        // The Messages view counts calls, so its line at the end says what
        // the call that runs does.
        assert_eq!(
            motion(&store, &status, View::Messages, None, 0.0),
            (None, true)
        );
        assert_eq!(working_label(&store, None), "Running ls…");
        call.status = Some(CallStatus::Ok);
        store.change(call, 0.0);
        assert_eq!(motion(&store, &status, View::Full, None, 0.0), (None, true));
        assert_eq!(working_label(&store, None), "Working…");
        store.add(item(5, Kind::Reply, MAIN, "done"), 0.0);
        assert_eq!(
            motion(&store, &status, View::Full, None, 0.0),
            (None, false)
        );
        assert!(!room(&store, View::Full, 0.0), "its reply ended it");
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
        assert_eq!(unseen(&rows, &store, 1), 1);
    }

    /// A turn's count in Messages stands at the thinking that led to its
    /// first call, so open it shows the turn's work from its first thought,
    /// as Quiet does. Its key is still its first call's.
    #[test]
    fn a_turns_count_stands_at_its_first_thought() {
        let mut call = item(3, Kind::Call, MAIN, "ls");
        call.status = Some(CallStatus::Ok);
        let items = vec![
            item(1, Kind::User, MAIN, "go"),
            item(2, Kind::Thinking, MAIN, "First, the list."),
            call,
            item(4, Kind::Reply, MAIN, "Done."),
        ];
        let laid = |open: &[u64]| {
            let open: HashSet<u64> = open.iter().copied().collect();
            let (sealed, leads) = (HashSet::new(), HashMap::new());
            layout_sealed(&items, View::Messages, None, &sealed, &leads, &open)
        };
        assert_eq!(ids(&laid(&[])), ["i1", "c1:3", "i4"]);
        // (A call without an act shows as it is.)
        assert_eq!(ids(&laid(&[3])), ["i1", "c1:3", "i2", "i3", "i4"]);
    }

    /// A thought folds to its first line, without bold marks, which says
    /// whether more follows.
    #[test]
    fn thinking_folds_to_its_first_line_without_bold_marks() {
        assert_eq!(
            first_line("\n  **Planning** the fix\nmore"),
            ("Planning the fix".to_string(), true)
        );
        assert_eq!(first_line("All of it."), ("All of it.".to_string(), false));
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

    fn store_of(items: &[ItemData]) -> Store {
        let mut store = Store::default();
        for item in items {
            store.add(item.clone(), 0.0);
        }
        store
    }

    fn label(items: &[ItemData], view: View, worker: Option<&str>, grown: &[u64]) -> String {
        let grown = grown.iter().copied().collect();
        jump_label(&layout(items, view, worker), &store_of(items), 5, &grown)
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
    fn latest_counts_what_is_new_before_it_says_text_grew() {
        let mut items = streaming();
        let grown: HashSet<u64> = [4].into();
        let rows = layout(&items, View::Full, None);
        let store = store_of(&items);
        assert_eq!(jump_label(&rows, &store, 3, &grown), "1 new");
        assert_eq!(jump_label(&rows, &store, 4, &grown), "New text");
        // A worker's text that came since isn't new in the main view.
        items.push(item(6, Kind::Text, "r1", "more"));
        let rows = layout(&items, View::Full, None);
        let store = store_of(&items);
        assert_eq!(jump_label(&rows, &store, 5, &HashSet::new()), "Latest");
    }

    /// A turn's count of calls that's open shows the turn's Quiet rows, each
    /// where it came among what the agent said, with the results Quiet puts
    /// in them and its thinking where it happened. Each turn keeps one count.
    #[test]
    fn a_turns_count_opens_to_its_quiet_rows_in_place() {
        use crate::protocol::{Act, ActKind};
        let read = |id: u64, path: &str, turn: u64| ItemData {
            status: Some(CallStatus::Ok),
            act: Some(Act {
                kind: ActKind::Read,
                target: path.into(),
                ..Act::default()
            }),
            turn: Some(turn),
            ..item(id, Kind::Call, MAIN, path)
        };
        let of_turn = |item: ItemData, turn: u64| ItemData {
            turn: Some(turn),
            ..item
        };
        let mut result = item(4, Kind::Result, MAIN, "fn main() {}");
        result.call = Some(3);
        let items = [
            of_turn(item(1, Kind::User, MAIN, "Fix the parser"), 1),
            read(2, "a.rs", 1),
            read(3, "b.rs", 1),
            of_turn(result, 1),
            of_turn(item(5, Kind::Thinking, MAIN, "There."), 1),
            of_turn(item(6, Kind::Text, MAIN, "Found it."), 1),
            read(7, "c.rs", 1),
            of_turn(item(8, Kind::Reply, MAIN, "Fixed."), 1),
            of_turn(item(9, Kind::User, MAIN, "Thanks"), 2),
            read(10, "d.rs", 2),
        ];
        let laid = |open: &[u64]| {
            let open: HashSet<u64> = open.iter().copied().collect();
            layout_sealed(
                &items,
                View::Messages,
                None,
                &HashSet::new(),
                &HashMap::new(),
                &open,
            )
        };
        let closed = ["i1", "c1:2", "i6", "i8", "i9", "c2:10"];
        assert_eq!(ids(&laid(&[])), closed);
        let open = laid(&[2]);
        assert_eq!(
            ids(&open),
            ["i1", "c1:2", "q2", "i5", "i6", "q7", "i8", "i9", "c2:10"]
        );
        let Row::Quiet { row, .. } = &open[2] else {
            panic!("{open:?}");
        };
        assert_eq!(row.words.text, "Read a.rs and b.rs");
        assert_eq!(
            ids(&row.calls),
            ["i2", "i3", "i4"],
            "its calls and a result"
        );
        assert!(
            matches!(
                open[1],
                Row::Count {
                    count: Count { calls: 3, .. },
                    ..
                }
            ),
            "{open:?}"
        );
        assert_eq!(
            ids(&laid(&[10])),
            ["i1", "c1:2", "i6", "i8", "i9", "c2:10", "q10"]
        );
        // A call that runs shows it in its row, where the count is open.
        let mut running = read(11, "e.rs", 2);
        running.status = Some(CallStatus::Running);
        let mut all = items.to_vec();
        all.push(running);
        let store = store_of(&all);
        let laid = |open: &[u64]| {
            let open: HashSet<u64> = open.iter().copied().collect();
            let (sealed, leads) = (HashSet::new(), HashMap::new());
            layout_sealed(&store.items, View::Messages, None, &sealed, &leads, &open)
        };
        assert!(running_shown(&laid(&[10]), &store, None));
        assert!(!running_shown(&laid(&[]), &store, None));
    }

    #[test]
    fn a_count_row_shows_no_text() {
        let mut call = item(3, Kind::Call, MAIN, "ls");
        call.status = Some(CallStatus::Ok);
        let items = vec![item(1, Kind::User, MAIN, "go"), call];
        let rows = layout(&items, View::Messages, None);
        assert_eq!(ids(&rows), ["i1", "c1:3"]);
        assert_eq!(
            jump_label(&rows, &store_of(&items), 3, &[3].into()),
            "Latest"
        );
    }

    /// The label for `items` in a view, once the item `seen` was seen.
    fn says(items: &[ItemData], view: View, worker: Option<&str>, seen: u64) -> String {
        let rows = layout(items, view, worker);
        jump_label(&rows, &store_of(items), seen, &HashSet::new())
    }

    /// A call with a result of two rows, which a command's output may follow.
    fn call_with_results(id: u64, agent: &str) -> Vec<ItemData> {
        let mut call = item(id, Kind::Call, agent, "shell");
        call.status = Some(CallStatus::Ok);
        let mut results = vec![
            item(id + 1, Kind::Result, agent, "out"),
            item(id + 2, Kind::Result, agent, "more"),
        ];
        for result in &mut results {
            result.call = Some(id);
        }
        let mut items = vec![call];
        items.append(&mut results);
        items
    }

    #[test]
    fn a_calls_result_rows_are_part_of_it() {
        let mut items = vec![item(1, Kind::User, MAIN, "go")];
        items.extend(call_with_results(2, MAIN));
        assert_eq!(says(&items, View::Full, None, 1), "1 new");
        // A result that comes after its call was seen isn't new.
        assert_eq!(says(&items, View::Full, None, 2), "Latest");
    }

    #[test]
    fn a_reply_and_ten_calls_with_results_are_eleven_new() {
        let mut items = vec![item(1, Kind::User, MAIN, "go")];
        for n in 0..10 {
            items.extend(call_with_results(2 + n * 3, MAIN));
        }
        items.push(item(32, Kind::Reply, MAIN, "done"));
        assert_eq!(says(&items, View::Full, None, 1), "11 new");
        assert_eq!(says(&items, View::Quiet, None, 1), "11 new");
    }

    #[test]
    fn a_user_message_and_a_reply_are_two_new() {
        let items = vec![
            item(1, Kind::Reply, MAIN, "earlier"),
            item(2, Kind::User, MAIN, "and now?"),
            item(3, Kind::Reply, MAIN, "this"),
        ];
        for view in View::ALL {
            assert_eq!(says(&items, view, None, 1), "2 new", "{view:?}");
        }
    }

    /// A notice that opens a turn Arko started.
    fn opening(id: u64, text: &str) -> ItemData {
        ItemData {
            opens_turn: true,
            level: Some(Level::Info),
            ..item(id, Kind::Notice, MAIN, text)
        }
    }

    /// A turn Arko started for news that ended without a reply. Full shows
    /// its opening and its work, then its line. Messages leaves all of it
    /// out, so it leaves out the line too, which would end a turn that shows
    /// nothing there. A turn whose calls Messages counts keeps its line.
    #[test]
    fn a_turn_that_shows_nothing_has_no_line() {
        let mut call = item(11, Kind::Call, MAIN, "cat report.md");
        call.status = Some(CallStatus::Ok);
        let items = vec![
            item(1, Kind::User, MAIN, "go"),
            item(2, Kind::Reply, MAIN, "Started r1."),
            item(3, Kind::Turn, MAIN, "3 s"),
            opening(4, "r1 reported"),
            item(5, Kind::Thinking, MAIN, "Nothing for the user."),
            item(6, Kind::Turn, MAIN, "2 s"),
            item(7, Kind::User, MAIN, "and now?"),
            item(8, Kind::Reply, MAIN, "Done."),
            item(9, Kind::Turn, MAIN, "1 s"),
            opening(10, "r1 reported"),
            call,
            item(12, Kind::Turn, MAIN, "2 s"),
        ];
        assert_eq!(
            ids(&layout(&items, View::Full, None)),
            [
                "i1", "i2", "i3", "i4", "i5", "i6", "i7", "i8", "i9", "i10", "i11", "i12"
            ]
        );
        assert_eq!(
            ids(&layout(&items, View::Messages, None)),
            ["i1", "i2", "i3", "i7", "i8", "i9", "c1:11", "i12"]
        );
    }

    /// A news turn after a worker's report keeps its line in Messages, since
    /// the report's row shows there.
    #[test]
    fn a_turn_after_a_workers_report_keeps_its_line() {
        let items = vec![
            item(1, Kind::User, MAIN, "go"),
            item(2, Kind::Reply, MAIN, "Started r1."),
            item(3, Kind::Turn, MAIN, "3 s"),
            item(4, Kind::Reply, "r1", "Counted."),
            opening(5, "r1 reported"),
            item(6, Kind::Thinking, MAIN, "Nothing for the user."),
            item(7, Kind::Turn, MAIN, "2 s"),
        ];
        assert_eq!(
            ids(&layout(&items, View::Messages, None)),
            ["i1", "i2", "i3", "s4", "i7"]
        );
    }

    #[test]
    fn marks_count_for_nothing_and_a_notice_that_opens_a_turn_counts() {
        let mut items = vec![
            item(1, Kind::User, MAIN, "go"),
            item(2, Kind::Turn, MAIN, "3 s"),
            item(3, Kind::Conversation, MAIN, "New conversation"),
            item(4, Kind::Output, MAIN, "output"),
            item(5, Kind::Notice, MAIN, "rate limited, retrying"),
            item(6, Kind::WorkerLine, MAIN, "r1"),
            // The mark counts, not the words.
            ItemData {
                level: Some(Level::Info),
                ..item(7, Kind::Notice, MAIN, "r1 reported")
            },
        ];
        assert_eq!(says(&items, View::Full, None, 1), "Latest");
        items.push(opening(8, "r1 reported"));
        assert_eq!(says(&items, View::Full, None, 1), "1 new");
    }

    /// A worker's report is a row of every main view, and counts there.
    /// Its text, its calls and the messages it got count in its own view.
    #[test]
    fn a_workers_report_counts_in_the_main_views() {
        let items = vec![
            item(1, Kind::User, MAIN, "go"),
            ItemData {
                from: Some(MAIN.into()),
                ..item(2, Kind::Message, "r1", "Also the docs.")
            },
            item(3, Kind::Text, "r1", "On it."),
            item(4, Kind::Reply, "r1", "Fixed."),
        ];
        assert_eq!(says(&items, View::Full, Some("r1"), 0), "3 new");
        for view in View::ALL {
            assert_eq!(says(&items, view, None, 1), "1 new", "{view:?}");
        }
        let mut call = item(5, Kind::Call, "r1", "cat");
        call.status = Some(CallStatus::Ok);
        let mut items = items;
        items.push(call);
        assert_eq!(says(&items, View::Full, None, 4), "Latest");
    }

    /// A worker's message to the user is a row of its own view only. The
    /// main views show the worker as its reports and its messages to its
    /// lead, each a row that opens to its words, and leave out a worker's
    /// worker's, which go to its lead's view.
    #[test]
    fn a_worker_shows_in_the_main_views_as_its_reports_and_messages() {
        use crate::protocol::{Act, ActKind};
        let mut to_lead = item(5, Kind::Call, "r1", "message main “Halfway.”");
        to_lead.status = Some(CallStatus::Ok);
        to_lead.act = Some(Act {
            kind: ActKind::Message,
            target: MAIN.into(),
            ..Act::default()
        });
        let mut said = item(6, Kind::Result, "r1", "Halfway.");
        said.call = Some(5);
        let mut read = item(7, Kind::Call, "r1", "read a.rs");
        read.status = Some(CallStatus::Ok);
        read.act = Some(Act {
            kind: ActKind::Read,
            target: "a.rs".into(),
            ..Act::default()
        });
        let items = vec![
            item(1, Kind::User, MAIN, "go"),
            ItemData {
                from: Some(MAIN.into()),
                ..item(2, Kind::Message, "r1", "Also the docs.")
            },
            item(3, Kind::Text, "r1", "On it."),
            item(4, Kind::Message, "r1", "Found it."),
            to_lead,
            said,
            read,
            ItemData {
                level: Some(Level::Error),
                ..item(8, Kind::Notice, "r1", "failed (boom)")
            },
            item(9, Kind::Reply, "r1", "Fixed."),
            item(10, Kind::Reply, "r2", "Counted."),
        ];
        let leads: HashMap<String, String> = [("r2".to_string(), "r1".to_string())].into();
        let laid = |view, worker| {
            layout_sealed(
                &items,
                view,
                worker,
                &HashSet::new(),
                &leads,
                &HashSet::new(),
            )
        };
        for view in View::ALL {
            assert_eq!(
                laid(view, None),
                [Row::Item(1, Kind::User), Row::Status(5), Row::Status(9)],
                "{view:?}"
            );
        }
        // r2's report goes to r1, its lead, in r1's view.
        assert_eq!(
            ids(&laid(View::Full, Some("r1"))),
            ["i2", "i3", "i4", "i5", "i6", "i7", "i8", "i9", "s10"]
        );
    }

    #[test]
    fn messages_count_what_it_shows_and_a_turns_calls_as_one() {
        let mut items = vec![item(1, Kind::User, MAIN, "go")];
        items.push(item(2, Kind::Thinking, MAIN, "hmm"));
        items.extend(call_with_results(3, MAIN));
        items.extend(call_with_results(6, MAIN));
        items.push(opening(9, "Monitor alerted · cargo test"));
        // Thinking, a result and an info notice are out of Messages.
        assert_eq!(says(&items, View::Messages, None, 2), "1 new");
        assert_eq!(says(&items, View::Full, None, 2), "3 new");
        items.push(item(10, Kind::Reply, MAIN, "done"));
        assert_eq!(says(&items, View::Messages, None, 2), "2 new");
        assert_eq!(says(&items, View::Full, None, 2), "4 new");
        // What Messages hides alone says nothing is new.
        let hidden = vec![
            item(1, Kind::User, MAIN, "go"),
            item(2, Kind::Thinking, MAIN, "hmm"),
            opening(3, "Monitor alerted · cargo test"),
        ];
        assert_eq!(says(&hidden, View::Messages, None, 1), "Latest");
        assert_eq!(says(&hidden, View::Full, None, 1), "2 new");
    }

    /// A long call joins the row above as it comes and shows there while
    /// it runs, so no row comes or goes when it starts or ends.
    #[test]
    fn a_long_call_leaves_the_rows_as_they_are() {
        use crate::protocol::{Act, ActKind};
        let call = |id: u64, target: &str, status: CallStatus| ItemData {
            status: Some(status),
            act: Some(Act {
                kind: ActKind::Command,
                target: target.into(),
                ..Act::default()
            }),
            ..item(id, Kind::Call, MAIN, target)
        };
        let mut items = vec![
            item(1, Kind::User, MAIN, "go"),
            call(2, "rg x", CallStatus::Ok),
            call(3, "cargo build", CallStatus::Running),
        ];
        let said = |rows: &[Row]| match &rows[1] {
            Row::Quiet { row, .. } => (row.words.said(), row.words.running),
            other => panic!("{other:?}"),
        };
        let running = layout(&items, View::Quiet, None);
        assert_eq!(ids(&running), ["i1", "q2"]);
        assert_eq!(
            said(&running),
            ("Ran rg x · running cargo build".into(), true)
        );
        items[2].status = Some(CallStatus::Ok);
        let settled = layout(&items, View::Quiet, None);
        assert_eq!(ids(&settled), ids(&running));
        assert_eq!(said(&settled), ("Ran 2 commands (rg, cargo)".into(), false));
    }

    /// Each of the shared fixtures, alone in its Quiet row, says and opens
    /// to what the terminal's row does, from the same projection, its
    /// content inside it, and a row with nothing to show has no fold.
    #[test]
    fn quiet_rows_open_to_the_fixtures_contents() {
        for fixture in quiet::fixtures() {
            let name = fixture.name;
            let call = ItemData {
                status: Some(fixture.status),
                outcome: Some(fixture.outcome.to_string()).filter(|outcome| !outcome.is_empty()),
                act: Some(fixture.act.clone()),
                detail: fixture.full_detail.map(str::to_string),
                job: fixture.job.then(|| "4d2c1a9b".to_string()),
                ended: fixture.ended,
                ..item(2, Kind::Call, MAIN, fixture.full)
            };
            let mut items = vec![item(1, Kind::User, MAIN, "go"), call];
            if let Some((kind, preview)) = fixture.content {
                let mut result = item(3, Kind::Result, MAIN, preview);
                result.content = Some(kind);
                result.more = fixture.more.map(str::to_string);
                result.call = Some(2);
                items.push(result);
            }
            let rows = layout(&items, View::Quiet, None);
            let Some(Row::Quiet { row, .. }) = rows.get(1) else {
                panic!("{name}: {rows:?}");
            };
            assert_eq!(row.words.said(), fixture.words, "{name}");
            assert_eq!(row.words.failed, fixture.failed, "{name}");
            assert_eq!(row.opens, fixture.opens, "{name}");
            assert!(!row.merged, "{name}");
            let content = row
                .calls
                .iter()
                .any(|row| matches!(row, Row::Item(3, Kind::Result)));
            assert_eq!(content, fixture.content.is_some(), "{name}");
        }
    }

    /// A failure ends its Quiet row, as the terminal's: the command after
    /// it starts a row of its own, and one called with it stays in its row.
    #[test]
    fn a_failure_ends_its_quiet_row() {
        use crate::protocol::{Act, ActKind};
        let call = |id: u64, target: &str, status: CallStatus, ended: Option<u64>| ItemData {
            status: Some(status),
            ended,
            act: Some(Act {
                kind: ActKind::Command,
                target: target.into(),
                ..Act::default()
            }),
            ..item(id, Kind::Call, MAIN, target)
        };
        let words = |items: &[ItemData]| -> Vec<(String, bool)> {
            layout(items, View::Quiet, None)
                .into_iter()
                .filter_map(|row| match row {
                    Row::Quiet { row, .. } => Some((row.words.text, row.words.failed)),
                    _ => None,
                })
                .collect()
        };
        let after = [
            call(1, "rg x", CallStatus::Ok, None),
            call(2, "cargo clippy", CallStatus::Failed, Some(3)),
            call(3, "sed -i x", CallStatus::Ok, None),
        ];
        assert_eq!(
            words(&after),
            [
                ("Ran 2 commands (rg, cargo)".to_string(), true),
                ("Ran sed -i x".to_string(), false)
            ]
        );
        let together = [
            call(1, "cargo clippy", CallStatus::Failed, Some(3)),
            call(2, "sed -i x", CallStatus::Ok, None),
        ];
        assert_eq!(
            words(&together),
            [("Ran 2 commands (cargo, sed)".to_string(), true)]
        );
    }

    /// A Quiet row of one call that the reader opened keeps its form, as
    /// the terminal's: the next read starts a row of its own.
    #[test]
    fn an_opened_quiet_row_of_one_call_keeps_its_form() {
        use crate::protocol::{Act, ActKind};
        let read = |id: u64, path: &str| ItemData {
            status: Some(CallStatus::Ok),
            act: Some(Act {
                kind: ActKind::Read,
                target: path.into(),
                ..Act::default()
            }),
            ..item(id, Kind::Call, MAIN, path)
        };
        let items = [read(1, "a.rs"), read(2, "b.rs")];
        let words = |sealed: &[u64]| -> Vec<String> {
            let sealed: HashSet<u64> = sealed.iter().copied().collect();
            layout_sealed(
                &items,
                View::Quiet,
                None,
                &sealed,
                &HashMap::new(),
                &HashSet::new(),
            )
            .into_iter()
            .filter_map(|row| match row {
                Row::Quiet { row, .. } => Some(row.words.text),
                _ => None,
            })
            .collect()
        };
        assert_eq!(words(&[]), ["Read a.rs and b.rs"]);
        assert_eq!(words(&[1]), ["Read a.rs", "Read b.rs"]);
    }

    /// A thinking after a call stands on a line of its own at the end,
    /// where it happened, and goes in the call's row, between its calls,
    /// when the next call joins it. One the reader opened stays where it is.
    #[test]
    fn a_thinking_goes_in_a_row_between_its_calls() {
        use crate::protocol::{Act, ActKind};
        let read = |id: u64, path: &str| ItemData {
            status: Some(CallStatus::Ok),
            act: Some(Act {
                kind: ActKind::Read,
                target: path.into(),
                ..Act::default()
            }),
            ..item(id, Kind::Call, MAIN, path)
        };
        let mut items = vec![
            item(1, Kind::User, MAIN, "go"),
            read(2, "a.rs"),
            item(3, Kind::Thinking, MAIN, "Which next?"),
        ];
        let thinking = layout(&items, View::Quiet, None);
        assert_eq!(ids(&thinking), ["i1", "q2", "i3"]);
        assert!(!in_quiet_row(&thinking, 3));
        items.push(read(4, "b.rs"));
        let joined = layout(&items, View::Quiet, None);
        assert_eq!(ids(&joined), ["i1", "q2"]);
        assert!(in_quiet_row(&joined, 3));
        let opened = layout_sealed(
            &items,
            View::Quiet,
            None,
            &[3].into(),
            &HashMap::new(),
            &HashSet::new(),
        );
        assert_eq!(ids(&opened), ["i1", "q2", "i3", "q4"]);
    }

    /// An open Quiet row shows each of its calls with the output under it,
    /// and the thinking between them by itself.
    #[test]
    fn an_open_quiet_row_keeps_each_calls_output_with_it() {
        let calls = [
            Row::Item(2, Kind::Call),
            Row::Item(3, Kind::Result),
            Row::Item(4, Kind::Thinking),
            Row::Item(5, Kind::Call),
            Row::Item(6, Kind::Call),
            Row::Item(7, Kind::Result),
            Row::Item(8, Kind::Result),
        ];
        let keys = |rows: &[Row]| rows.iter().map(Row::key).collect::<Vec<_>>();
        let members: Vec<(String, Vec<String>)> = quiet_members(&calls)
            .iter()
            .map(|(row, results)| (row.key(), keys(results)))
            .collect();
        let member = |key: &str, results: &[&str]| {
            (
                key.to_string(),
                results.iter().map(|key| key.to_string()).collect(),
            )
        };
        assert_eq!(
            members,
            [
                member("i2", &["i3"]),
                member("i4", &[]),
                member("i5", &[]),
                member("i6", &["i7", "i8"]),
            ]
        );
    }

    /// Quiet says each call in a few words, as the terminal does. Reads of
    /// a file being edited join its edits, a test run that failed stands
    /// alone with one ✗ and its output inside it, and the agent's text ends
    /// a run.
    #[test]
    fn quiet_merges_calls_into_rows_of_a_few_words() {
        use crate::protocol::{Act, ActKind};
        let call = |id: u64, kind: ActKind, target: &str, status: CallStatus| ItemData {
            status: Some(status),
            act: Some(Act {
                kind,
                target: target.into(),
                ..Act::default()
            }),
            ..item(id, Kind::Call, MAIN, "call")
        };
        let mut test = call(5, ActKind::Command, "cargo test", CallStatus::Failed);
        if let Some(act) = test.act.as_mut() {
            act.ran = true;
            act.news = "1 failed, 3 passed".into();
        }
        let mut result = item(6, Kind::Result, MAIN, "test a ... FAILED");
        result.call = Some(5);
        let mut edit_result = item(4, Kind::Result, MAIN, "1 | a");
        edit_result.call = Some(3);
        let items = vec![
            item(1, Kind::User, MAIN, "go"),
            call(2, ActKind::Read, "src/app.rs", CallStatus::Ok),
            call(3, ActKind::Edit, "src/app.rs", CallStatus::Ok),
            edit_result,
            test,
            result,
            item(7, Kind::Text, MAIN, "Fixing the test."),
            call(8, ActKind::Edit, "src/app.rs", CallStatus::Ok),
            // A call without an act shows as Full does.
            ItemData {
                status: Some(CallStatus::Ok),
                ..item(9, Kind::Call, MAIN, "ls")
            },
        ];
        let rows = layout(&items, View::Quiet, None);
        assert_eq!(ids(&rows), ["i1", "q2", "q5", "i7", "q8", "i9"]);
        let said = |row: &Row| match row {
            Row::Quiet { row, .. } => (row.words.clone(), ids(&row.calls)),
            other => panic!("{other:?}"),
        };
        let (words, calls) = said(&rows[1]);
        assert_eq!(words.text, "Read app.rs and made 1 edit");
        assert_eq!(calls, ["i2", "i3", "i4"]);
        let (words, calls) = said(&rows[2]);
        assert_eq!(words.said(), "Ran cargo test (1 failed, 3 passed)");
        assert!(words.failed);
        assert_eq!(calls, ["i5", "i6"]);
        // Images that a call's result brings later leave the rows as they
        // were, since they show under the row's line whatever row it is.
        let mut with_images = items.clone();
        with_images[2].images.push(crate::protocol::ImageData {
            id: 1,
            mime: "image/png".into(),
            label: "shot.png".into(),
            bytes: 10,
            url: "/i/1".into(),
        });
        assert_eq!(layout(&with_images, View::Quiet, None), rows);
        assert!(!moves_rows(&items[2], &with_images[2], View::Quiet));
        // Full keeps every call's row.
        assert_eq!(
            ids(&layout(&items, View::Full, None)),
            ["i1", "i2", "i3", "i4", "i5", "i6", "i7", "i8", "i9"]
        );
    }

    /// A change lays the rows out again only when it changes what they're
    /// laid out by, as a text that becomes the reply does in every view.
    #[test]
    fn a_change_moves_the_rows_only_by_what_they_read() {
        let text = item(4, Kind::Text, MAIN, "Done.");
        let moves = |change: &dyn Fn(&mut ItemData), view| {
            let mut new = text.clone();
            change(&mut new);
            moves_rows(&text, &new, view)
        };
        for view in View::ALL {
            assert!(moves(&|item| item.kind = Kind::Reply, view), "{view:?}");
            assert!(moves(&|item| item.pending = true, view), "{view:?}");
            assert!(moves(&|item| item.started = Some("r1".into()), view));
            assert!(moves(&|item| item.level = Some(Level::Warn), view));
            assert!(moves(&|item| item.from = Some(MAIN.into()), view));
            assert!(!moves(&|item| item.text.push_str(" Mostly."), view));
            assert!(!moves(&|item| item.level = Some(Level::Info), view));
            // How a call went words its Quiet row, which a count that's
            // open shows in Messages.
            assert_eq!(
                moves(&|item| item.outcome = Some("ok".into()), view),
                view != View::Full
            );
        }
        // A call's status counts in Messages and words a Quiet row.
        let status = |item: &mut ItemData| item.status = Some(CallStatus::Failed);
        assert!(moves(&status, View::Messages));
        assert!(moves(&status, View::Quiet));
        assert!(!moves(&status, View::Full));
    }
}

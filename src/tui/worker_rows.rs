//! A worker's row in the terminal's transcript. The call that set a worker
//! going (a start, or a message that continued it) is the worker's one row
//! in every view, which says how the worker stands and opens to what the
//! worker was asked. The feed's placeholder for the worker shows only where
//! no such call stands for it.

use std::collections::{HashMap, HashSet};

use crate::crew::{MESSAGE_AGENT, START_AGENT};
use crate::remote::{CallStatus, Item, Kind, WorkerState};

/// The word a worker's row says while the call that starts it runs, before
/// the worker has an ID.
pub(super) const STARTING: &str = "starting";

/// How a part of a worker's row looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Tone {
    Muted,
    Running,
    Failed,
    Stopped,
}

/// A call that is a worker's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Going<'a> {
    /// A start, with its worker's ID once its result names it.
    Start(Option<&'a str>),
    /// A message that set a worker that ended going again.
    Again(&'a str),
}

/// Whether `item` is a worker's row ([`Going`]), a call that starts a
/// worker or one that set a worker going again.
pub(super) fn going(item: &Item) -> Option<Going<'_>> {
    if item.kind != Kind::Call {
        return None;
    }
    match item.tool.as_deref() {
        Some(START_AGENT) => Some(Going::Start(item.sets.as_deref())),
        Some(MESSAGE_AGENT) => item.sets.as_deref().map(Going::Again),
        _ => None,
    }
}

impl<'a> Going<'a> {
    /// The worker whose row it is, once known.
    pub(super) fn worker(self) -> Option<&'a str> {
        match self {
            Going::Start(worker) => worker,
            Going::Again(worker) => Some(worker),
        }
    }

    /// What the row says before the worker's facts, with the ID that leads
    /// it. A start says its worker's ID and title, or the title its task
    /// gave (`task`) until the worker has them. A message says it messaged
    /// the worker.
    pub(super) fn words(self, task: &str, title: Option<&str>) -> (Option<&'a str>, String) {
        match self {
            Going::Start(worker) => {
                let title = title.filter(|title| !title.is_empty()).unwrap_or(task);
                (worker, title.to_string())
            }
            Going::Again(worker) => (None, format!("Messaged {worker}")),
        }
    }

    /// What the row says in place of its words where they don't fit whole.
    /// A message's row keeps its worker's ID, which a cut would lose, and a
    /// start's ID leads its row anyway.
    pub(super) fn short(self) -> Option<&'a str> {
        match self {
            Going::Start(_) => None,
            Going::Again(worker) => Some(worker),
        }
    }
}

/// What a worker's row says after its ID and title, in order. That is its
/// state unless it finished (success needs no word, and its report has a
/// row of its own), how many calls it made once it made one, and how long
/// it has run (`time`), each part with how it looks.
pub(super) fn worker_facts(
    state: WorkerState,
    calls: u32,
    time: Option<String>,
) -> Vec<(String, Tone)> {
    let mut facts = Vec::new();
    let word = match state {
        WorkerState::Queued => Some(("queued", Tone::Muted)),
        WorkerState::Running => Some(("running", Tone::Running)),
        WorkerState::Failed => Some(("failed", Tone::Failed)),
        WorkerState::Stopped => Some(("stopped", Tone::Stopped)),
        _ => None,
    };
    if let Some((word, tone)) = word {
        facts.push((word.to_string(), tone));
    }
    if calls > 0 {
        // (In the words of the agent list and the Messages view's count.)
        let calls = if calls == 1 {
            "1 tool call".to_string()
        } else {
            format!("{calls} tool calls")
        };
        facts.push((calls, Tone::Muted));
    }
    if let Some(time) = time {
        facts.push((time, Tone::Muted));
    }
    facts
}

/// The workers' rows among a transcript's items.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Rows {
    /// The placeholders that a call of the worker's lead stands for, by
    /// their IDs, which don't show. Such a call came before the placeholder
    /// in its turn and set the worker going, or is a start or a message of
    /// that turn that still runs, which may be the one.
    pub(super) covered: HashSet<u64>,
    /// The latest row of each worker, the calls that set it going and its
    /// placeholders that show, by the worker's ID. That row alone says how
    /// the worker stands. A worker set going again in a later turn has a row
    /// there, and its earlier rows keep only its ID and title.
    pub(super) latest: HashMap<String, u64>,
}

/// The workers' rows among `items`, in order ([`Rows`]).
///
/// A call stands only for a placeholder that came after it, since a worker
/// can't exist before the call that set it going. A start that still runs
/// can only be starting a new worker, so it stands only for a worker with
/// no earlier placeholder and no earlier call that set it going. A line
/// that shows keeps showing whatever its turn calls later.
pub(super) fn rows<'a>(items: impl IntoIterator<Item = &'a Item> + Clone) -> Rows {
    // What the calls of each lead's turn set going, or may while they run,
    // each with the first such call's ID.
    #[derive(Default)]
    struct Turn<'a> {
        set: HashMap<&'a str, u64>,
        starting: Option<u64>,
        messaging: HashMap<&'a str, u64>,
    }
    fn first<'a>(firsts: &mut HashMap<&'a str, u64>, worker: &'a str, id: u64) {
        let at = firsts.entry(worker).or_insert(id);
        *at = (*at).min(id);
    }
    let mut turns: HashMap<(&str, Option<u64>), Turn> = HashMap::new();
    // The first call that set each worker going and its first placeholder,
    // of any lead and turn.
    let mut set_first: HashMap<&str, u64> = HashMap::new();
    let mut line_first: HashMap<&str, u64> = HashMap::new();
    for item in items.clone() {
        if item.kind == Kind::WorkerLine {
            if let Some(worker) = item.worker.as_deref() {
                first(&mut line_first, worker, item.id);
            }
            continue;
        }
        if item.kind != Kind::Call {
            continue;
        }
        let running = item.status == Some(CallStatus::Running);
        let key = (item.agent.as_str(), item.turn);
        match going(item) {
            Some(Going::Start(Some(set)) | Going::Again(set)) => {
                first(&mut turns.entry(key).or_default().set, set, item.id);
                first(&mut set_first, set, item.id);
            }
            Some(Going::Start(None)) if running => {
                let starting = &mut turns.entry(key).or_default().starting;
                *starting = Some(starting.map_or(item.id, |at| at.min(item.id)));
            }
            None if running && item.tool.as_deref() == Some(MESSAGE_AGENT) => {
                if let Some(act) = &item.act {
                    let target = act.target.as_str();
                    first(
                        &mut turns.entry(key).or_default().messaging,
                        target,
                        item.id,
                    );
                }
            }
            _ => {}
        }
    }
    let mut rows = Rows::default();
    for item in items {
        let worker = match item.kind {
            Kind::WorkerLine => {
                let Some(worker) = item.worker.as_deref() else {
                    continue;
                };
                let before = |at: Option<&u64>| at.is_some_and(|&at| at < item.id);
                let new = !before(set_first.get(worker)) && !before(line_first.get(worker));
                let covered = turns
                    .get(&(item.agent.as_str(), item.turn))
                    .is_some_and(|turn| {
                        before(turn.set.get(worker))
                            || before(turn.messaging.get(worker))
                            || (new && before(turn.starting.as_ref()))
                    });
                if covered {
                    rows.covered.insert(item.id);
                    continue;
                }
                Some(worker)
            }
            _ => going(item).and_then(Going::worker),
        };
        if let Some(worker) = worker {
            rows.latest.insert(worker.to_string(), item.id);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Act;

    fn call(id: u64, tool: &str, sets: Option<&str>, status: CallStatus) -> Item {
        let mut call = Item::new(Kind::Call, "a call");
        call.id = id;
        call.turn = Some(1);
        call.tool = Some(tool.to_string());
        call.sets = sets.map(str::to_string);
        call.status = Some(status);
        call
    }

    fn line(id: u64, worker: &str) -> Item {
        let mut line = Item::new(Kind::WorkerLine, worker);
        line.id = id;
        line.turn = Some(1);
        line.worker = Some(worker.to_string());
        line
    }

    /// A worker's row says its state only when it is unusual, its calls
    /// from the first, and its time.
    #[test]
    fn a_workers_row_says_how_it_stands() {
        let said = |state, calls, time: Option<&str>| -> Vec<String> {
            worker_facts(state, calls, time.map(str::to_string))
                .into_iter()
                .map(|(word, _)| word)
                .collect()
        };
        assert_eq!(said(WorkerState::Queued, 0, None), ["queued"]);
        assert_eq!(said(WorkerState::Running, 0, Some("3s")), ["running", "3s"]);
        assert_eq!(
            said(WorkerState::Running, 1, Some("8s")),
            ["running", "1 tool call", "8s"]
        );
        assert_eq!(
            said(WorkerState::Finished, 2, Some("20s")),
            ["2 tool calls", "20s"]
        );
        assert_eq!(
            said(WorkerState::Failed, 1, Some("6s")),
            ["failed", "1 tool call", "6s"]
        );
        assert_eq!(
            said(WorkerState::Stopped, 3, Some("1m")),
            ["stopped", "3 tool calls", "1m"]
        );
        let tones: Vec<Tone> = worker_facts(WorkerState::Failed, 1, None)
            .into_iter()
            .map(|(_, tone)| tone)
            .collect();
        assert_eq!(tones, [Tone::Failed, Tone::Muted]);
    }

    /// A start is a worker's row from the moment it is called, and a
    /// message is one only once its result says it set the worker going.
    /// A start says its task's title until its worker has one.
    #[test]
    fn a_call_that_set_a_worker_going_is_its_row() {
        let starting = call(1, START_AGENT, None, CallStatus::Running);
        assert_eq!(going(&starting), Some(Going::Start(None)));
        assert_eq!(
            Going::Start(None).words("Fix the test", None),
            (None, "Fix the test".to_string())
        );
        assert_eq!(
            Going::Start(Some("r1")).words("Fix the test", Some("Fix the tests")),
            (Some("r1"), "Fix the tests".to_string())
        );
        let again = call(2, MESSAGE_AGENT, Some("r2"), CallStatus::Ok);
        assert_eq!(going(&again), Some(Going::Again("r2")));
        assert_eq!(
            Going::Again("r2").words("", None),
            (None, "Messaged r2".to_string())
        );
        assert_eq!(
            (Going::Again("r2").short(), Going::Start(Some("r1")).short()),
            (Some("r2"), None)
        );
        assert_eq!(going(&call(3, MESSAGE_AGENT, None, CallStatus::Ok)), None);
        assert_eq!(going(&call(4, "shell", Some("r1"), CallStatus::Ok)), None);
    }

    /// A placeholder is covered by a call of its turn that set its worker
    /// going, or by a start or a message to it that still runs, and not by
    /// one of another turn or another lead, or one that set another worker
    /// going. The latest row of a worker is its last call or placeholder
    /// that shows.
    #[test]
    fn a_call_of_its_turn_stands_for_a_workers_placeholder() {
        let r1 = line(9, "r1");
        let covers = |call: Item| rows([&call, &r1]).covered.contains(&9);
        assert!(covers(call(1, START_AGENT, Some("r1"), CallStatus::Ok)));
        assert!(covers(call(1, START_AGENT, None, CallStatus::Running)));
        assert!(!covers(call(1, START_AGENT, None, CallStatus::Failed)));
        assert!(!covers(call(1, START_AGENT, Some("r2"), CallStatus::Ok)));
        let mut message = call(1, MESSAGE_AGENT, None, CallStatus::Running);
        message.act = Some(Act {
            target: "r1".into(),
            ..Act::default()
        });
        assert!(covers(message.clone()));
        message.status = Some(CallStatus::Ok);
        assert!(!covers(message.clone()));
        message.sets = Some("r1".into());
        assert!(covers(message));
        let mut earlier = call(1, START_AGENT, Some("r1"), CallStatus::Ok);
        earlier.turn = None;
        assert!(!covers(earlier));
        assert!(!covers(
            call(1, START_AGENT, Some("r1"), CallStatus::Ok).agent("r5")
        ));
        let mut later = line(4, "r2");
        later.turn = Some(2);
        let latest = rows([
            &call(1, START_AGENT, Some("r1"), CallStatus::Ok),
            &call(2, START_AGENT, Some("r2"), CallStatus::Ok),
            &line(3, "r1"),
            &later,
        ])
        .latest;
        assert_eq!((latest["r1"], latest["r2"]), (1, 4));
    }

    /// A worker's line that shows keeps showing when a later start of its
    /// turn runs, which can't be starting a worker that already has a line
    /// or a call that set it going. A start that runs still stands for a
    /// new worker's line that comes after it.
    #[test]
    fn a_later_start_leaves_a_line_that_shows() {
        // r3 left the queue in this turn, and another start runs.
        let mut queued = call(1, START_AGENT, Some("r3"), CallStatus::Ok);
        queued.turn = Some(0);
        let mut first = line(2, "r3");
        first.turn = Some(0);
        let shown = rows([
            &queued,
            &first,
            &line(5, "r3"),
            &call(6, START_AGENT, None, CallStatus::Running),
        ]);
        assert!(!shown.covered.contains(&5));
        assert_eq!(shown.latest["r3"], 5);
        // Nor does a start stand for a line that came before it.
        let before = rows([
            &line(5, "r4"),
            &call(6, START_AGENT, None, CallStatus::Running),
        ]);
        assert!(!before.covered.contains(&5));
        // A new worker's line after it is the start's to stand for.
        let after = rows([
            &call(6, START_AGENT, None, CallStatus::Running),
            &line(7, "r4"),
        ]);
        assert!(after.covered.contains(&7));
    }

    /// A worker's line that shows stays when a later message of its turn
    /// continues its worker, as an earlier row with its ID and title, and
    /// the message is the worker's latest row.
    #[test]
    fn a_later_message_leaves_a_line_that_shows() {
        let mut message = call(6, MESSAGE_AGENT, None, CallStatus::Running);
        message.act = Some(Act {
            target: "r1".into(),
            ..Act::default()
        });
        let running = rows([&line(5, "r1"), &message]);
        assert!(!running.covered.contains(&5));
        message.status = Some(CallStatus::Ok);
        message.sets = Some("r1".into());
        let settled = rows([&line(5, "r1"), &message]);
        assert!(!settled.covered.contains(&5));
        assert_eq!(settled.latest["r1"], 6);
    }
}

//! The session's items in order, with the workers' and the background jobs'
//! summaries, as the stream gives them. Plain data, so the views' logic
//! and its tests need no page.

use std::collections::HashMap;

use crate::protocol::{ItemData, JobSummary, Kind, WorkerData};

/// The main agent's `agent`.
pub const MAIN: &str = "main";

/// How many items the page keeps, as Arko does, the main transcript's and
/// the workers' each on its own.
pub const BUDGET: usize = 3000;

#[derive(Default)]
pub struct Store {
    /// The feed the items are of, as the stream's start names it. Item IDs,
    /// turns and workers' names start over in another one (Arko started
    /// again), so all the page keeps of them goes with it.
    pub feed: Option<u64>,
    pub items: Vec<ItemData>,
    index: HashMap<u64, usize>,
    pub workers: HashMap<String, WorkerData>,
    /// The workers in the order they first came.
    pub worker_order: Vec<String>,
    pub jobs: HashMap<String, JobSummary>,
    /// When each item came or last changed, in this device's time (ms).
    pub grown: HashMap<u64, f64>,
    counts: [usize; 2],
}

impl Store {
    /// Starts over (a `reset`) with the items of `feed`. Returns whether
    /// the items it had were of another feed.
    pub fn reset(
        &mut self,
        feed: Option<u64>,
        items: Vec<ItemData>,
        workers: Vec<WorkerData>,
        jobs: Vec<JobSummary>,
        now: f64,
    ) -> bool {
        let switched = self.feed.is_some_and(|had| feed != Some(had));
        *self = Store {
            feed,
            ..Store::default()
        };
        for item in items {
            self.add(item, now);
        }
        for worker in workers {
            self.worker(worker);
        }
        for job in jobs {
            self.jobs.insert(job.id.clone(), job);
        }
        switched
    }

    fn reindex(&mut self) {
        self.index = self
            .items
            .iter()
            .enumerate()
            .map(|(at, item)| (item.id, at))
            .collect();
    }

    pub fn get(&self, id: u64) -> Option<&ItemData> {
        self.index.get(&id).map(|&at| &self.items[at])
    }

    /// A new item, come at `now`. Returns the IDs trimmed to keep the
    /// budget, or false where it changed an item it had.
    pub fn add(&mut self, item: ItemData, now: f64) -> Vec<u64> {
        if self.index.contains_key(&item.id) {
            self.change(item, now);
            return Vec::new();
        }
        let lane = usize::from(item.agent != MAIN);
        self.grown.insert(item.id, now);
        self.index.insert(item.id, self.items.len());
        self.items.push(item);
        self.counts[lane] += 1;
        let mut trimmed = Vec::new();
        while self.counts[lane] > BUDGET {
            let Some(at) = self
                .items
                .iter()
                .position(|old| usize::from(old.agent != MAIN) == lane)
            else {
                break;
            };
            let old = self.items.remove(at);
            self.grown.remove(&old.id);
            self.counts[lane] -= 1;
            trimmed.push(old.id);
        }
        if !trimmed.is_empty() {
            self.reindex();
        }
        trimmed
    }

    /// More text for the item `id`, come at `now`. False if it isn't kept.
    pub fn append(&mut self, id: u64, text: &str, now: f64) -> bool {
        let Some(&at) = self.index.get(&id) else {
            return false;
        };
        self.items[at].text.push_str(text);
        self.grown.insert(id, now);
        true
    }

    /// Takes the item `id` out. False if it isn't kept.
    pub fn remove(&mut self, id: u64) -> bool {
        let Some(at) = self.index.remove(&id) else {
            return false;
        };
        let item = self.items.remove(at);
        self.grown.remove(&id);
        self.counts[usize::from(item.agent != MAIN)] -= 1;
        self.reindex();
        true
    }

    /// The item with this one's ID is now this one, since `now`.
    pub fn change(&mut self, item: ItemData, now: f64) -> bool {
        let Some(&at) = self.index.get(&item.id) else {
            return false;
        };
        self.grown.insert(item.id, now);
        self.items[at] = item;
        true
    }

    pub fn worker(&mut self, summary: WorkerData) {
        if !self.workers.contains_key(&summary.id) {
            self.worker_order.push(summary.id.clone());
        }
        self.workers.insert(summary.id.clone(), summary);
    }

    pub fn job(&mut self, summary: JobSummary) {
        self.jobs.insert(summary.id.clone(), summary);
    }

    /// The IDs of the background jobs that run, in order.
    pub fn running_jobs(&self) -> Vec<String> {
        let mut running: Vec<String> = self
            .jobs
            .values()
            .filter(|job| job.state.is_running())
            .map(|job| job.id.clone())
            .collect();
        running.sort();
        running
    }

    /// When the latest turn began (the time of its first item).
    pub fn turn_since(&self) -> Option<i64> {
        let mut turn = None;
        let mut since = None;
        for item in self.items.iter().rev() {
            let Some(this) = item.turn else { break };
            if turn.is_some_and(|turn| turn != this) {
                break;
            }
            turn = Some(this);
            since = Some(item.at);
        }
        since
    }

    /// The newest item's ID.
    pub fn newest(&self) -> u64 {
        self.items.last().map_or(0, |item| item.id)
    }

    /// The workers' summaries in the order they came.
    pub fn worker_list(&self) -> Vec<&WorkerData> {
        self.worker_order
            .iter()
            .filter_map(|id| self.workers.get(id))
            .collect()
    }

    /// The view of the history entry `entry` (a worker's, none for the main
    /// transcript) in the session `session`. Until the store holds the start
    /// of the stream that's open (`current`), it's the worker the entry
    /// names, to check once it does. Then a worker of another session, or
    /// one this session doesn't have, gives the main transcript.
    pub fn entry_view(&self, entry: &Entry, session: &str, current: bool) -> Option<String> {
        let worker = entry.worker.clone()?;
        if !current {
            return Some(worker);
        }
        (entry.session.as_deref() == Some(session) && self.workers.contains_key(&worker))
            .then_some(worker)
    }
}

/// The feed a stream's start names, in hex.
pub fn parse_feed(hex: &str) -> Option<u64> {
    u64::from_str_radix(hex, 16).ok()
}

/// What an entry of the browser's history keeps of the view it shows: the
/// worker (none for the main transcript), and the session it was in, which
/// Arko names in the status (not the key).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entry {
    pub worker: Option<String>,
    pub session: Option<String>,
}

/// What a call's line offers: Move to background while it runs and the
/// status lists it among the calls that can move (`movable`), and Stop
/// (the job's ID) while the background job it made runs (`runs`). Each is
/// asked only where it could apply, so a line that reads them reactively
/// follows the status only while its call runs, and the jobs only while
/// it has one.
pub fn call_actions(
    item: &ItemData,
    movable: impl FnOnce(u64) -> bool,
    runs: impl FnOnce(&str) -> bool,
) -> (bool, Option<String>) {
    if item.kind != Kind::Call {
        return (false, None);
    }
    let moves = item.status == Some(crate::protocol::CallStatus::Running) && movable(item.id);
    let stop = item.job.clone().filter(|job| runs(job));
    (moves, stop)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::protocol::{CallStatus, JobKind, JobState};

    /// An item for tests.
    pub fn item(id: u64, kind: Kind, agent: &str, text: &str) -> ItemData {
        ItemData {
            id,
            at: id as i64 * 1000,
            kind,
            agent: agent.into(),
            turn: Some(1),
            text: text.into(),
            detail: None,
            level: None,
            source: None,
            delivery: None,
            status: None,
            outcome: None,
            started: None,
            worker: None,
            from: None,
            call: None,
            job: None,
            pending: false,
            opens_turn: false,
            code: Vec::new(),
            questions: None,
            images: Vec::new(),
        }
    }

    pub fn worker(id: &str) -> WorkerData {
        WorkerData {
            id: id.into(),
            title: format!("Task of {id}"),
            state: crate::protocol::WorkerState::Running,
            tool_calls: 0,
            started_at: None,
            ended_at: None,
            last: None,
            report: None,
            ran_ms: 0,
            running_since: None,
            model: None,
            effort: None,
            profile: None,
            skills: None,
            mcp: None,
            parent: None,
            depth: 1,
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

    #[test]
    fn the_store_keeps_each_transcript_to_its_budget_and_changes_items_in_place() {
        let mut store = Store::default();
        for id in 1..=BUDGET as u64 + 2 {
            store.add(item(id, Kind::Text, MAIN, "x"), 0.0);
        }
        store.add(item(10_000, Kind::Text, "r1", "w"), 0.0);
        assert_eq!(store.items.len(), BUDGET + 1);
        assert_eq!(store.items[0].id, 3);
        assert!(store.append(10, "y", 5.0));
        assert_eq!(store.get(10).unwrap().text, "xy");
        assert_eq!(store.grown[&10], 5.0);
        let mut changed = item(10, Kind::Call, MAIN, "ls");
        changed.outcome = Some("ok".into());
        assert!(store.change(changed.clone(), 6.0));
        assert_eq!(store.get(10), Some(&changed));
        // A change without the outcome takes it away.
        changed.outcome = None;
        store.change(changed, 7.0);
        assert_eq!(store.get(10).unwrap().outcome, None);
        assert!(store.remove(10));
        assert!(store.get(10).is_none() && store.get(11).is_some());
        assert!(!store.append(10, "z", 0.0));
    }

    #[test]
    fn a_reset_of_another_feed_says_so() {
        let mut store = Store::default();
        let reset = |store: &mut Store, feed| {
            store.reset(
                feed,
                vec![item(1, Kind::Text, MAIN, "x")],
                Vec::new(),
                Vec::new(),
                0.0,
            )
        };
        // The first feed, and the same one again.
        assert!(!reset(&mut store, Some(1)));
        assert!(!reset(&mut store, Some(1)));
        assert_eq!(store.feed, Some(1));
        assert!(reset(&mut store, Some(2)));
        assert_eq!((store.feed, store.items.len()), (Some(2), 1));
        assert_eq!(parse_feed("00000000000000ff"), Some(255));
        assert_eq!(parse_feed(""), None);
    }

    #[test]
    fn a_history_entry_shows_its_worker_only_in_its_own_session() {
        let entry = |worker: Option<&str>, session: Option<&str>| Entry {
            worker: worker.map(Into::into),
            session: session.map(Into::into),
        };
        let mut store = Store::default();
        store.worker(worker("r1"));
        let r1 = entry(Some("r1"), Some("s1"));
        assert_eq!(store.entry_view(&r1, "s1", true).as_deref(), Some("r1"));
        // Another session's r1, a worker gone, and an entry that names no
        // session show the main transcript.
        assert_eq!(store.entry_view(&r1, "s2", true), None);
        assert_eq!(
            store.entry_view(&entry(Some("r2"), Some("s1")), "s1", true),
            None
        );
        assert_eq!(store.entry_view(&entry(Some("r1"), None), "s1", true), None);
        assert_eq!(store.entry_view(&Entry::default(), "s1", true), None);
        // Before the stream's start (after a reload, offline, or while a new
        // link's key is traded), the entry's worker shows, whatever the store
        // holds, to be checked when the start comes.
        let empty = Store::default();
        assert_eq!(empty.entry_view(&r1, "", false).as_deref(), Some("r1"));
        assert_eq!(store.entry_view(&r1, "s2", false).as_deref(), Some("r1"));
        assert_eq!(empty.entry_view(&Entry::default(), "", false), None);
        // A new session (a reset) has other workers.
        store.reset(None, Vec::new(), vec![worker("r2")], Vec::new(), 0.0);
        assert_eq!(store.entry_view(&r1, "s1", true), None);
    }

    #[test]
    fn the_current_turn_began_with_its_first_item() {
        let mut store = Store::default();
        let mut one = item(1, Kind::User, MAIN, "a");
        one.turn = Some(1);
        let mut two = item(2, Kind::User, MAIN, "b");
        two.turn = Some(2);
        let mut three = item(3, Kind::Text, MAIN, "c");
        three.turn = Some(2);
        for item in [one, two, three] {
            store.add(item, 0.0);
        }
        assert_eq!(store.turn_since(), Some(2000));
    }

    /// `call_actions` with the calls that can move and the store's jobs.
    fn actions(call: &ItemData, store: &Store, movable: &[u64]) -> (bool, Option<String>) {
        let running = store.running_jobs();
        call_actions(
            call,
            |id| movable.contains(&id),
            |job| running.iter().any(|running| running == job),
        )
    }

    #[test]
    fn a_running_call_that_can_move_offers_it_and_a_running_job_offers_stop() {
        let mut store = Store::default();
        let mut call = item(5, Kind::Call, MAIN, "sleep 100");
        call.status = Some(CallStatus::Running);
        assert_eq!(actions(&call, &store, &[5]), (true, None));
        assert_eq!(actions(&call, &store, &[]), (false, None));
        call.status = Some(CallStatus::Ok);
        call.job = Some("j1".into());
        let job = JobSummary {
            id: "j1".into(),
            agent: MAIN.into(),
            call: Some(5),
            kind: JobKind::Command,
            label: "sleep 100".into(),
            state: JobState::Running,
            started_at: 0,
            ended_at: None,
            outcome: None,
            watching: None,
            alerts: None,
            expires_at: None,
        };
        store.job(job.clone());
        assert_eq!(actions(&call, &store, &[5]), (false, Some("j1".into())));
        store.job(JobSummary {
            state: JobState::Finished,
            ..job
        });
        assert_eq!(actions(&call, &store, &[]), (false, None));
    }

    /// A settled call without a job asks neither what can move nor which
    /// jobs run, so its line follows neither.
    #[test]
    fn a_settled_call_asks_nothing() {
        let mut call = item(5, Kind::Call, MAIN, "ls");
        call.status = Some(CallStatus::Ok);
        let asked = |_| -> bool { panic!("asked") };
        assert_eq!(
            call_actions(&call, asked, |_| panic!("asked")),
            (false, None)
        );
        // One that made a job asks only about its job.
        call.job = Some("j1".into());
        assert_eq!(
            call_actions(&call, asked, |job| job == "j1"),
            (false, Some("j1".into()))
        );
    }

    #[test]
    fn running_jobs_are_listed_in_order() {
        let mut store = Store::default();
        for (id, state) in [
            ("j2", JobState::Running),
            ("j1", JobState::Running),
            ("j3", JobState::Finished),
        ] {
            store.job(JobSummary {
                id: id.into(),
                agent: MAIN.into(),
                call: None,
                kind: JobKind::Command,
                label: "sleep 100".into(),
                state,
                started_at: 0,
                ended_at: None,
                outcome: None,
                watching: None,
                alerts: None,
                expires_at: None,
            });
        }
        assert_eq!(store.running_jobs(), ["j1", "j2"]);
    }
}

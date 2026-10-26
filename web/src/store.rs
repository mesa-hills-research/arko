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
    /// Starts over (a `reset`).
    pub fn reset(
        &mut self,
        items: Vec<ItemData>,
        workers: Vec<WorkerData>,
        jobs: Vec<JobSummary>,
        now: f64,
    ) {
        *self = Store::default();
        for item in items {
            self.add(item, now);
        }
        for worker in workers {
            self.worker(worker);
        }
        for job in jobs {
            self.jobs.insert(job.id.clone(), job);
        }
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
}

/// What a call's line offers: Move to background while it runs and the
/// status lists it among the calls that can move (`movable`), and Stop
/// (the job's ID) while the background job it made runs.
pub fn call_actions(item: &ItemData, store: &Store, movable: &[u64]) -> (bool, Option<String>) {
    if item.kind != Kind::Call {
        return (false, None);
    }
    let moves =
        item.status == Some(crate::protocol::CallStatus::Running) && movable.contains(&item.id);
    let stop = item
        .job
        .as_ref()
        .and_then(|job| store.jobs.get(job))
        .filter(|job| job.state.is_running())
        .map(|job| job.id.clone());
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
            worker: None,
            from: None,
            call: None,
            job: None,
            pending: false,
            code: Vec::new(),
            questions: None,
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
            parent: None,
            depth: 1,
            cost_billed: 0.0,
            cost_list: 0.0,
            family_cost_billed: 0.0,
            family_cost_list: 0.0,
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

    #[test]
    fn a_running_call_that_can_move_offers_it_and_a_running_job_offers_stop() {
        let mut store = Store::default();
        let mut call = item(5, Kind::Call, MAIN, "sleep 100");
        call.status = Some(CallStatus::Running);
        assert_eq!(call_actions(&call, &store, &[5]), (true, None));
        assert_eq!(call_actions(&call, &store, &[]), (false, None));
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
        assert_eq!(
            call_actions(&call, &store, &[5]),
            (false, Some("j1".into()))
        );
        store.job(JobSummary {
            state: JobState::Finished,
            ..job
        });
        assert_eq!(call_actions(&call, &store, &[]), (false, None));
    }
}

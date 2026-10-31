//! A session's feed at random, for the tests of the feed and of its clients
//! (the `testing` feature). [`RandomSession`] drives a [`Feed`] through the
//! calls a session makes, from a seed, so a failing run can be made again.
//! [`FeedMirror`] is a client of the feed reduced to what the feed promises
//! it, to compare a client with.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use arko_agents::AgentEvent;
use arko_workflows::WorkflowEvent;
use uuid::Uuid;

use super::feed::{
    Activity, Delivery, ENDED_JOBS, Feed, Item, JobKind, JobStart, JobState, JobSummary, Kind,
    Level, MAIN_AGENT, ManualClock, Snapshot, Source, Update, WORKER_EVERY, WorkerChange,
    WorkerState, WorkerSummary,
};
use crate::approvals::{Answer, Approval, ApprovalAnswers, ApprovalChoice};
use crate::questions::{AskedQuestion, QuestionState};

/// The agent name the main agent's events carry.
const MAIN: &str = "main";

/// Words its text is made of.
const WORDS: [&str; 16] = [
    "the", "test", "passes", "now", "and", "the", "build", "is", "green", "so", "it", "ships",
    "fix", "that", "file", "next",
];

/// A session's calls on a feed, made at random from a seed: its agents'
/// streamed text and thinking, calls and results, failed responses, the
/// user's messages and notes, questions, approvals, workers, background
/// jobs, cuts, conversations and a resumed history. Some text is long, so
/// the feed's budgets drop old items and old whole outputs. Its feed's time
/// moves only as the session's calls let it pass, so the summaries the feed
/// holds back go out at the session's moments.
pub struct RandomSession {
    feed: Feed,
    clock: Arc<ManualClock>,
    rng: Rng,
    /// The main agent's conversation.
    run: Uuid,
    /// The calls waiting for their results, and the worker each is of
    /// (none for the main agent's).
    calls: Vec<(Uuid, Option<String>)>,
    /// The latest calls, for their images.
    made: Vec<Uuid>,
    /// The workers, and whether each runs.
    workers: Vec<(String, bool)>,
    /// The workers' ends, in order.
    ended: Vec<(String, WorkerState)>,
    /// The background jobs running, and whether each is a monitor.
    jobs: Vec<(Uuid, bool)>,
    /// The notes the agent hasn't read, by their numbers.
    notes: Vec<u64>,
    next_note: u64,
    /// The questions open.
    questions: Vec<String>,
    next_question: u32,
    /// The approval requests waiting, with where their answers go.
    approvals: Vec<(String, tokio::sync::oneshot::Receiver<Answer>)>,
    /// The user's messages that started turns, by item.
    messages: Vec<u64>,
    working: bool,
}

impl RandomSession {
    /// A session from `seed`, on a feed of its own.
    pub fn new(seed: u64) -> Self {
        let clock = ManualClock::new();
        Self {
            feed: Feed::with_clock(MAIN, clock.clone()),
            clock,
            rng: Rng(seed),
            run: Uuid::new_v4(),
            calls: Vec::new(),
            made: Vec::new(),
            workers: Vec::new(),
            ended: Vec::new(),
            jobs: Vec::new(),
            notes: Vec::new(),
            next_note: 1,
            questions: Vec::new(),
            next_question: 1,
            approvals: Vec::new(),
            messages: Vec::new(),
            working: false,
        }
    }

    /// Its feed.
    pub fn feed(&self) -> &Feed {
        &self.feed
    }

    /// A number from the session's sequence below `below`, for a test that
    /// chooses its own moments by the same seed.
    pub fn pick(&mut self, below: u64) -> u64 {
        self.rng.below(below)
    }

    /// Lets the time pass for which the feed holds back the workers'
    /// summaries, so none waits to go out.
    pub fn settle(&mut self) {
        self.clock.advance(WORKER_EVERY);
    }

    /// The workers' ends the session made (finished, failed or stopped),
    /// in order.
    pub fn ended(&self) -> &[(String, WorkerState)] {
        &self.ended
    }

    /// Starts a background job that runs on, then starts and ends more
    /// than the feed keeps once ended, so it drops the oldest of them.
    pub fn jobs_past_the_budget(&mut self) -> &'static str {
        let running = Uuid::new_v4();
        let start = JobStart::new(running, MAIN_AGENT, JobKind::Command, "cargo watch");
        self.feed.job_started(start);
        self.jobs.push((running, false));
        for _ in 0..ENDED_JOBS + 20 {
            let id = Uuid::new_v4();
            let start = JobStart::new(id, MAIN_AGENT, JobKind::Command, "cargo test");
            self.feed.job_started(start);
            self.feed
                .job_ended(id, JobState::Finished, Some("exit 0".into()));
        }
        "jobs past the budget"
    }

    /// Makes one more of the session's calls on the feed, or lets some
    /// time pass, and says which.
    pub fn step(&mut self) -> &'static str {
        match self.rng.below(108) {
            0..=17 => self.text(Kind::Text),
            18..=24 => self.text(Kind::Thinking),
            25..=34 => self.call(),
            35..=43 => self.result(),
            44..=45 => self.reject(),
            46..=48 => self.discard(),
            49..=50 => self.error(),
            51..=54 => self.worker(),
            55..=56 => self.worker_info(),
            57..=61 => self.message(),
            62..=64 => self.turn_end(),
            65..=68 => self.note(),
            69..=71 => self.read_note(),
            72..=76 => self.job(),
            77..=79 => self.question(),
            80..=81 => self.answer_question(),
            82..=84 => self.approval(),
            85..=86 => self.answer_approval(),
            87 => self.truncate(),
            88 => self.conversation(),
            89..=90 => self.notice(),
            91 => self.stop(),
            92..=93 => self.images(),
            94 => self.mark_message(),
            95 => self.restore(),
            96..=99 => self.long_text(),
            _ => self.time(),
        }
    }

    /// Up to 0.4 seconds pass.
    fn time(&mut self) -> &'static str {
        let by = Duration::from_millis(self.rng.below(400));
        self.clock.advance(by);
        "time passed"
    }

    fn words(&mut self, count: u64) -> String {
        let count = 1 + self.rng.below(count);
        let words: Vec<&str> = (0..count)
            .map(|_| WORDS[self.rng.below(WORDS.len() as u64) as usize])
            .collect();
        words.join(" ")
    }

    /// An agent at random, the main agent or a running worker, as its
    /// events name it: the agent and the worker's label.
    fn agent(&mut self) -> Option<String> {
        let running: Vec<String> = self
            .workers
            .iter()
            .filter(|(_, running)| *running)
            .map(|(id, _)| id.clone())
            .collect();
        if running.is_empty() || self.rng.below(3) == 0 {
            return None;
        }
        Some(running[self.rng.below(running.len() as u64) as usize].clone())
    }

    fn event(&self, worker: Option<&str>, event: AgentEvent) {
        let event = WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: if worker.is_some() { "worker" } else { MAIN }.into(),
            agent_run_id: if worker.is_some() {
                Uuid::nil()
            } else {
                self.run
            },
            event,
        };
        self.feed.event(&event, worker);
    }

    fn text(&mut self, kind: Kind) -> &'static str {
        let agent = self.agent();
        let text = format!("{} ", self.words(8));
        let event = match kind {
            Kind::Thinking => AgentEvent::ReasoningChunk(text),
            _ => AgentEvent::AssistantResponseChunk(text),
        };
        self.event(agent.as_deref(), event);
        "text"
    }

    /// Text long enough that the feed drops its oldest items for it.
    fn long_text(&mut self) -> &'static str {
        let agent = self.agent();
        let text = "x".repeat(64 * 1024 + self.rng.below(64 * 1024) as usize);
        self.event(agent.as_deref(), AgentEvent::AssistantResponseChunk(text));
        "long text"
    }

    fn call(&mut self) -> &'static str {
        let agent = self.agent();
        let call = Uuid::new_v4();
        let command = format!("cargo {}", self.words(3));
        let event = AgentEvent::ToolCallFormatted {
            tool_call_id: call,
            tool_name: crate::shell::SHELL_TOOL_NAME.into(),
            parameters: [("command".to_string(), command.into())].into(),
        };
        self.event(agent.as_deref(), event);
        self.calls.push((call, agent));
        self.made.push(call);
        if self.made.len() > 64 {
            self.made.remove(0);
        }
        "call"
    }

    /// The result of a call waiting for it, now and then with an output
    /// long enough that the feed drops older whole outputs for it.
    fn result(&mut self) -> &'static str {
        if self.calls.is_empty() {
            return self.call();
        }
        let (call, agent) = self
            .calls
            .remove(self.rng.below(self.calls.len() as u64) as usize);
        let output = match self.rng.below(10) {
            0 => "line\n".repeat(120_000 + self.rng.below(60_000) as usize),
            1..=3 => format!("Exit code: 1\n{}", self.words(30)),
            _ => format!("Exit code: 0\n{}", self.words(30)),
        };
        let event = AgentEvent::ToolResult {
            tool_call_id: call,
            tool_name: crate::shell::SHELL_TOOL_NAME.into(),
            output,
        };
        self.event(agent.as_deref(), event);
        "result"
    }

    fn reject(&mut self) -> &'static str {
        if self.calls.is_empty() {
            return self.call();
        }
        let (call, agent) = self
            .calls
            .remove(self.rng.below(self.calls.len() as u64) as usize);
        let event = AgentEvent::ToolRejected {
            tool_call_id: call,
            tool_name: crate::shell::SHELL_TOOL_NAME.into(),
            reason: Some("The user said no.".into()),
        };
        self.event(agent.as_deref(), event);
        "rejected call"
    }

    /// A model request whose response streams a little and fails, so the
    /// feed takes back what it streamed.
    fn discard(&mut self) -> &'static str {
        let agent = self.agent();
        self.event(
            agent.as_deref(),
            AgentEvent::ModelRequestStarted {
                model: "model".into(),
                subagent: None,
                sizes: None,
                bytes_per_token: 4.0,
                sent_at: None,
            },
        );
        let text = self.words(6);
        self.event(agent.as_deref(), AgentEvent::AssistantResponseChunk(text));
        self.event(
            agent.as_deref(),
            AgentEvent::StreamedOutputDiscarded {
                text_bytes: 0,
                reasoning_bytes: 0,
            },
        );
        "discarded response"
    }

    fn error(&mut self) -> &'static str {
        let agent = self.agent();
        let error_message = format!("the request failed ({})", self.rng.below(1000));
        self.event(
            agent.as_deref(),
            AgentEvent::ErrorEncountered {
                error_message,
                requires_approval_to_retry: false,
                tool_call_id: None,
                error_kind: None,
            },
        );
        "error"
    }

    /// A worker starts, or one ends (finished, failed or stopped), or one
    /// that ended goes on again.
    fn worker(&mut self) -> &'static str {
        let ended: Vec<usize> = (0..self.workers.len())
            .filter(|at| !self.workers[*at].1)
            .collect();
        let running: Vec<usize> = (0..self.workers.len())
            .filter(|at| self.workers[*at].1)
            .collect();
        match self.rng.below(4) {
            0 | 1 if self.workers.len() < 6 => {
                let id = format!("r{}", self.workers.len() + 1);
                let task = format!("Make {}", self.words(4));
                self.feed.worker(&id, &task, WorkerChange::Started);
                self.workers.push((id, true));
                "worker started"
            }
            2 if !ended.is_empty() => {
                let at = ended[self.rng.below(ended.len() as u64) as usize];
                let id = self.workers[at].0.clone();
                self.feed.worker(&id, "Go on.", WorkerChange::Started);
                self.workers[at].1 = true;
                "worker went on"
            }
            _ if !running.is_empty() => {
                let at = running[self.rng.below(running.len() as u64) as usize];
                let id = self.workers[at].0.clone();
                let (change, ended) = match self.rng.below(3) {
                    0 => (
                        WorkerChange::Finished { report: "Done." },
                        WorkerState::Finished,
                    ),
                    1 => (
                        WorkerChange::Failed {
                            error: "It failed.",
                        },
                        WorkerState::Failed,
                    ),
                    _ => (
                        WorkerChange::Stopped {
                            reason: "The user stopped it.",
                        },
                        WorkerState::Stopped,
                    ),
                };
                self.feed.worker(&id, "", change);
                self.ended.push((id.clone(), ended));
                self.workers[at].1 = false;
                self.calls
                    .retain(|(_, agent)| agent.as_deref() != Some(&id));
                "worker ended"
            }
            _ => self.text(Kind::Text),
        }
    }

    /// What a worker started with comes, also for a worker the feed hasn't
    /// heard of yet, as the session's start hook can tell it first.
    fn worker_info(&mut self) -> &'static str {
        let known = !self.workers.is_empty() && self.rng.below(2) == 0;
        let id = match known {
            true => self.workers[self.rng.below(self.workers.len() as u64) as usize]
                .0
                .clone(),
            false if self.workers.len() < 6 => {
                let id = format!("r{}", self.workers.len() + 1);
                self.workers.push((id.clone(), true));
                id
            }
            false => return self.worker(),
        };
        let mut info = super::feed::WorkerInfo::new();
        if self.rng.below(2) == 0 {
            info.model = Some("model".into());
            info.costs = Some((0.01 * self.rng.below(100) as f64, 0.02));
        }
        self.feed.worker_info(&id, info);
        "worker info"
    }

    /// The user's message, which starts a turn.
    fn message(&mut self) -> &'static str {
        if self.working {
            return self.note();
        }
        let text = self.words(10);
        let id = self.feed.user(&text, Source::Terminal, Delivery::Turn);
        self.messages.push(id);
        self.feed.turn_started();
        self.working = true;
        "message"
    }

    fn turn_end(&mut self) -> &'static str {
        if !self.working {
            return self.message();
        }
        if self.rng.below(2) == 0 {
            let text = self.words(12);
            self.feed.reply(&text);
        }
        self.feed.turn_ended("3 s · $0.01", None);
        self.calls.retain(|(_, agent)| agent.is_some());
        self.working = false;
        "turn ended"
    }

    fn note(&mut self) -> &'static str {
        let seq = self.next_note;
        self.next_note += 1;
        let text = self.words(6);
        self.feed.note(&text, Source::Web, seq, Vec::new());
        self.notes.push(seq);
        "note"
    }

    /// The agent reads a note, or one it reads before its item comes.
    fn read_note(&mut self) -> &'static str {
        let seq = match self.notes.is_empty() || self.rng.below(5) == 0 {
            true => self.next_note + self.rng.below(3),
            false => self
                .notes
                .remove(self.rng.below(self.notes.len() as u64) as usize),
        };
        self.event(
            None,
            AgentEvent::ContextInjected {
                ids: vec![format!("note:{seq}")],
                bytes: 10,
            },
        );
        "note read"
    }

    /// A background job starts (a command, or a monitor, some made by a
    /// call), changes, or ends.
    fn job(&mut self) -> &'static str {
        if self.jobs.is_empty() || self.rng.below(2) == 0 {
            let id = Uuid::new_v4();
            let agent = self.agent().unwrap_or_else(|| MAIN_AGENT.to_string());
            let monitor = self.rng.below(3) == 0;
            let kind = if monitor {
                JobKind::Monitor
            } else {
                JobKind::Command
            };
            let mut start = JobStart::new(id, agent, kind, format!("cargo {}", self.words(2)));
            if let Some((call, _)) = self.calls.last() {
                start = start.made_by(*call);
            }
            if monitor {
                start = start.monitoring("cargo test", chrono::Utc::now().timestamp_millis());
            }
            self.feed.job_started(start);
            self.jobs.push((id, monitor));
            return "job started";
        }
        let at = self.rng.below(self.jobs.len() as u64) as usize;
        let (id, monitor) = self.jobs[at];
        if monitor && self.rng.below(2) == 0 {
            let alerts = self.rng.below(5) as u32;
            self.feed
                .job_changed(id, alerts, chrono::Utc::now().timestamp_millis() + 1000);
            return "monitor changed";
        }
        self.jobs.remove(at);
        let ended = match self.rng.below(3) {
            0 => JobState::Failed,
            1 => JobState::Stopped,
            _ => JobState::Finished,
        };
        self.feed.job_ended(id, ended, Some("exit 1".into()));
        "job ended"
    }

    fn question(&mut self) -> &'static str {
        let count = 1 + self.rng.below(2);
        let asked: Vec<AskedQuestion> = (0..count)
            .map(|_| {
                let id = format!("q{}", self.next_question);
                self.next_question += 1;
                AskedQuestion {
                    id,
                    question: arko_agents::questions::Question {
                        question: format!("{}?", self.words(5)),
                        ..Default::default()
                    },
                }
            })
            .collect();
        let waits = self.rng.below(2) == 0;
        self.feed.ask_questions(&asked, waits);
        self.questions
            .extend(asked.into_iter().map(|question| question.id));
        "question"
    }

    fn answer_question(&mut self) -> &'static str {
        if self.questions.is_empty() {
            return self.question();
        }
        let id = self
            .questions
            .remove(self.rng.below(self.questions.len() as u64) as usize);
        let state = match self.rng.below(3) {
            0 => QuestionState::Dismissed,
            1 => QuestionState::Closed { why: None },
            _ => QuestionState::Answered {
                answer: self.words(3),
            },
        };
        self.feed.set_question_state(&id, state);
        "question answered"
    }

    fn approval(&mut self) -> &'static str {
        let name = self.words(1);
        let approval = Approval::Profile(crate::profiles::approval(
            &crate::profiles::Profile::new(&name, "Reviews"),
            crate::profiles::Place::User,
        ));
        let (id, answer) = self.feed.request_approval(self.run, None, approval);
        self.approvals.push((id, answer));
        "approval requested"
    }

    /// The user answers an approval request, or its tool stops waiting.
    fn answer_approval(&mut self) -> &'static str {
        if self.approvals.is_empty() {
            return self.approval();
        }
        let (id, _answer) = self
            .approvals
            .remove(self.rng.below(self.approvals.len() as u64) as usize);
        if self.rng.below(3) == 0 {
            self.feed.withdraw_approval(&id);
            return "approval withdrawn";
        }
        self.feed.answer_approvals(&ApprovalAnswers {
            choices: vec![ApprovalChoice {
                id,
                approve: self.rng.below(2) == 0,
                option: false,
            }],
            words: None,
        });
        "approval answered"
    }

    /// The conversation is cut back to one of the user's messages, as an
    /// edit does.
    fn truncate(&mut self) -> &'static str {
        if self.messages.is_empty() || self.working {
            return self.message();
        }
        let at = self.rng.below(self.messages.len() as u64) as usize;
        let from = self.messages[at];
        self.messages.truncate(at);
        self.feed.truncate(from);
        let text = self.words(6);
        let id = self.feed.user(&text, Source::App, Delivery::Turn);
        self.messages.push(id);
        "truncated"
    }

    fn conversation(&mut self) -> &'static str {
        self.feed.conversation("New conversation");
        self.messages.clear();
        "new conversation"
    }

    fn notice(&mut self) -> &'static str {
        let text = self.words(8);
        match self.rng.below(3) {
            0 => self.feed.notice(Level::Warn, &text),
            1 => self.feed.output(&text),
            _ => self.feed.turn_opening(&text),
        }
        "notice"
    }

    fn stop(&mut self) -> &'static str {
        if !self.working {
            return self.message();
        }
        self.feed.turn_stopping("Stopping the turn.");
        if self.rng.below(2) == 0 {
            self.feed.turn_stopped("Stopped the turn.");
        }
        "stop"
    }

    fn images(&mut self) -> &'static str {
        let call = match self.made.is_empty() || self.rng.below(4) == 0 {
            true => Uuid::new_v4(),
            false => self.made[self.rng.below(self.made.len() as u64) as usize],
        };
        let id = self.rng.below(1000);
        let image = crate::images::ImageRef {
            id,
            mime: "image/png".into(),
            label: format!("{id}.png"),
            bytes: 4,
        };
        self.feed.call_images(call, vec![image]);
        "images"
    }

    fn mark_message(&mut self) -> &'static str {
        let Some(&id) = self.messages.last() else {
            return self.message();
        };
        let index = self.rng.below(20) as usize;
        self.feed.mark_message(id, index);
        "message marked"
    }

    /// A few items from before the feed, as a resumed session's history.
    fn restore(&mut self) -> &'static str {
        let items: Vec<Item> = (1..=3)
            .map(|id| {
                let mut item = Item::new(Kind::Text, self.words(5)).agent(MAIN_AGENT);
                item.id = id;
                item.turn = Some(1);
                item
            })
            .collect();
        self.feed.restore(items);
        "history restored"
    }
}

/// A client of a feed reduced to what the feed promises it. It starts from
/// a snapshot and applies the updates that follow, as a client does. The
/// latest-value kinds keep what came last, and [`Update::Activity`] takes
/// the activity the client reads as the update comes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeedMirror {
    /// The items, by ID.
    pub items: BTreeMap<u64, Item>,
    /// The workers, in the order they came.
    pub workers: Vec<WorkerSummary>,
    /// The background jobs, in the order they came.
    pub jobs: Vec<JobSummary>,
    pub activity: Activity,
}

impl FeedMirror {
    /// A client as it starts from `snapshot`.
    pub fn new(snapshot: &Snapshot) -> Self {
        Self {
            items: snapshot
                .items
                .iter()
                .map(|item| (item.id, item.clone()))
                .collect(),
            workers: snapshot.workers.clone(),
            jobs: snapshot.jobs.clone(),
            activity: snapshot.activity.clone(),
        }
    }

    /// Applies `update`. `activity` reads the feed's activity now.
    pub fn apply(&mut self, update: &Update, activity: impl FnOnce() -> Activity) {
        match update {
            Update::Item(item) => {
                self.items.insert(item.id, item.clone());
            }
            // Only an item it has, as a client replaces it, so a change of
            // one it never got shows.
            Update::Change(item) => {
                if let Some(known) = self.items.get_mut(&item.id) {
                    *known = item.clone();
                }
            }
            Update::Append { id, text } => {
                if let Some(item) = self.items.get_mut(id) {
                    item.text.push_str(text);
                }
            }
            Update::Remove { id } => {
                self.items.remove(id);
            }
            Update::Truncate { from } => {
                self.items.retain(|id, item| *id < *from || !item.is_main())
            }
            Update::Worker(summary) => {
                match self.workers.iter_mut().find(|known| known.id == summary.id) {
                    Some(known) => *known = summary.clone(),
                    None => self.workers.push(summary.clone()),
                }
            }
            Update::Job(summary) => match self.jobs.iter_mut().find(|known| known.id == summary.id)
            {
                Some(known) => *known = summary.clone(),
                None => self.jobs.push(summary.clone()),
            },
            Update::Activity => self.activity = activity(),
            // An edge: what it tells of is in the worker's summary.
            Update::WorkerEnded { .. } => {}
        }
    }

    /// What of it `snapshot` keeps, to compare with a client made from it.
    /// The feed drops its oldest items, the oldest whole outputs and the
    /// ended jobs that started first without an update, so this drops them
    /// too. They are the items of each transcript (the main agent's, and
    /// the workers') older than the snapshot's oldest, and the whole
    /// outputs, parameters and results the snapshot no longer has. Of the
    /// jobs that ended, the [`ENDED_JOBS`] that started last stay, whatever
    /// the snapshot holds, as the feed keeps them, and every job that runs
    /// stays.
    pub fn within(&self, snapshot: &Snapshot) -> Self {
        let oldest = |main: bool| {
            snapshot
                .items
                .iter()
                .filter(|item| (item.agent == MAIN_AGENT) == main)
                .map(|item| item.id)
                .min()
        };
        let (main, workers) = (oldest(true), oldest(false));
        let shown: BTreeMap<u64, &Item> =
            snapshot.items.iter().map(|item| (item.id, item)).collect();
        let items = self
            .items
            .iter()
            .filter(|(id, item)| {
                match if item.agent == MAIN_AGENT {
                    main
                } else {
                    workers
                } {
                    Some(oldest) => **id >= oldest,
                    None => false,
                }
            })
            .map(|(id, item)| {
                let mut item = item.clone();
                if let Some(kept) = shown.get(id) {
                    if kept.whole.is_none() {
                        item.whole = None;
                    }
                    if kept.parameters.is_none() {
                        item.parameters = None;
                    }
                    if kept.output.is_none() {
                        item.output = None;
                    }
                }
                (*id, item)
            })
            .collect();
        let ended = self
            .jobs
            .iter()
            .filter(|job| !job.state.is_running())
            .count();
        let mut dropped = ended.saturating_sub(ENDED_JOBS);
        Self {
            items,
            workers: self.workers.clone(),
            jobs: self
                .jobs
                .iter()
                .filter(|job| {
                    let drop = dropped > 0 && !job.state.is_running();
                    dropped -= usize::from(drop);
                    !drop
                })
                .cloned()
                .collect(),
            activity: self.activity.clone(),
        }
    }

    /// Where it first differs from `other`, in words, or `None` when they
    /// are the same.
    pub fn difference(&self, other: &FeedMirror) -> Option<String> {
        let ids = |mirror: &FeedMirror| mirror.items.keys().copied().collect::<Vec<_>>();
        if ids(self) != ids(other) {
            let only = |a: &FeedMirror, b: &FeedMirror| {
                a.items
                    .keys()
                    .filter(|id| !b.items.contains_key(id))
                    .copied()
                    .collect::<Vec<_>>()
            };
            return Some(format!(
                "items only here {:?}, only there {:?}",
                only(self, other),
                only(other, self)
            ));
        }
        if let Some((id, item)) = self
            .items
            .iter()
            .find(|(id, item)| other.items.get(id) != Some(item))
        {
            return Some(format!(
                "item {id} differs:\n here  {item:?}\n there {:?}",
                other.items[id]
            ));
        }
        if self.workers != other.workers {
            return Some(format!(
                "workers differ:\n here  {:?}\n there {:?}",
                self.workers, other.workers
            ));
        }
        if self.jobs != other.jobs {
            return Some(format!(
                "jobs differ:\n here  {:?}\n there {:?}",
                self.jobs, other.jobs
            ));
        }
        if self.activity != other.activity {
            return Some(format!(
                "activity differs:\n here  {:?}\n there {:?}",
                self.activity, other.activity
            ));
        }
        None
    }
}

/// SplitMix64, enough for choosing a session's next call.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `below` (which isn't 0).
    fn below(&mut self, below: u64) -> u64 {
        self.next() % below
    }
}

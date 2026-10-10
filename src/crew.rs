//! Workers: subagents that carry out tasks the main agent (the lead) hands
//! them while it goes on with its own work.
//!
//! Each worker runs in a workflow of its own, rooted in the working
//! directory or in a git worktree made for it, so its commands, its file
//! tools and the command guard all work there. `start_worker` returns at
//! once; the worker's report, when it finishes, and the messages it sends
//! (`message_lead`) reach the lead in the first way that applies:
//! - a `wait_for_workers` call waiting for that worker returns them;
//! - while the lead's turn runs, as a note at its next step;
//! - else they wait in the lead's mailbox, and fin starts a turn with them
//!   (or adds them to the user's next message).
//!
//! The lead's messages (`message_worker`) reach a running worker as a note,
//! or a `wait_for_lead` call; a worker that has finished, or was stopped,
//! continues its task with the message. A note that arrives as its reader
//! finishes is not lost: the run continues, and reads it.
//!
//! The workers are kept in the session (`workers/`), each with its
//! workflow's checkpoints, so a resumed session can continue them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use arko_agents::{
    AgentConfig, Task, TaskContext, Tool, ToolDefinition, ToolError, ToolParameterDefinition,
    ToolParameterType, ToolRecovery, ToolTypeDetails,
};
use arko_ai::LLMClient;
use arko_workflows::notes::MAX_NOTE_BYTES;
use arko_workflows::prelude::*;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use crate::agents::{MAIN, WORKER};
use crate::ui::{self, Control, Output, Stats};

pub const START_WORKER: &str = "start_worker";
pub const MESSAGE_WORKER: &str = "message_worker";
pub const WAIT_FOR_WORKERS: &str = "wait_for_workers";
pub const LIST_WORKERS: &str = "list_workers";
pub const STOP_WORKER: &str = "stop_worker";
pub const MESSAGE_LEAD: &str = "message_lead";
pub const WAIT_FOR_LEAD: &str = "wait_for_lead";

/// The lead's (the main agent's) worker tools.
pub const LEAD_TOOL_NAMES: &[&str] = &[
    START_WORKER,
    MESSAGE_WORKER,
    WAIT_FOR_WORKERS,
    LIST_WORKERS,
    STOP_WORKER,
];
/// A worker's tools for talking with the lead.
pub const WORKER_TOOL_NAMES: &[&str] = &[MESSAGE_LEAD, WAIT_FOR_LEAD];

/// The workers' workflows' name.
const WORKFLOW_NAME: &str = "fin-worker";
/// Who the lead's notes are from, as a worker reads them.
const LEAD: &str = "lead";
/// The workers' records, in the session's `workers` folder.
const REGISTRY_FILE: &str = "workers.json";
/// A note's text stays under this; a longer report is cut, and saved whole
/// in a file.
const NOTE_BUDGET: usize = MAX_NOTE_BYTES - 1024;
/// How long the wait tools wait by default, and at most, in seconds.
const DEFAULT_WAIT: u64 = 600;
const MAX_WAIT: u64 = 3600;
/// How long a stopped worker gets to save its state.
const GRACE: Duration = Duration::from_secs(10);
/// `start_worker`'s choice of prompt cache.
pub const CACHE_PARAM: &str = "cache";

/// How long a worker on a Claude model keeps its prompt cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

impl CacheTtl {
    pub fn from_1h(one_hour: bool) -> Self {
        if one_hour {
            Self::OneHour
        } else {
            Self::FiveMinutes
        }
    }

    fn parse(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "5m" | "5min" | "5 minutes" => Ok(Self::FiveMinutes),
            "1h" | "60m" | "1 hour" => Ok(Self::OneHour),
            other => Err(format!("cache is `5m` or `1h`, not `{other}`")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }

    fn lasts(self) -> chrono::Duration {
        match self {
            Self::FiveMinutes => chrono::Duration::minutes(5),
            Self::OneHour => chrono::Duration::hours(1),
        }
    }

    /// Sets `config`'s Anthropic cache lifetime to this one.
    fn apply(self, config: &mut AgentConfig) {
        config.llm_config.generation_config.anthropic_cache_ttl = match self {
            Self::FiveMinutes => None,
            Self::OneHour => Some(arko_ai::AnthropicCacheTtl::OneHour),
        };
    }
}

/// What the crew needs from the session.
pub struct Setup {
    pub session_id: String,
    pub session_dir: PathBuf,
    /// The lead's working directory.
    pub workspace: PathBuf,
    pub client: Arc<LLMClient>,
    /// A new worker's configuration (a resumed one keeps its own).
    pub worker_config: AgentConfig,
    /// The tools the lead and its workers share: the shell, the terminal,
    /// the search indexes and the MCP servers' tools.
    pub tools: Vec<Arc<dyn Tool>>,
    pub output: Arc<Output>,
    pub stats: Arc<Mutex<Stats>>,
    pub show_thinking: bool,
    /// How many workers may run at once (`NO_LIMIT`: any number).
    pub max_running: u32,
    /// A new worker's prompt cache when the lead doesn't choose, for
    /// workers on a Claude model; `None` on other models, which have no
    /// choice.
    pub cache: Option<CacheTtl>,
}

/// A worker as the session records it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    number: u32,
    /// The task as the lead gave it.
    task: String,
    /// Where it works: the working directory, or its place in the worktree.
    dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worktree: Option<Worktree>,
    created: DateTime<Utc>,
    /// Its worktree was removed: it can't continue.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    removed: bool,
    /// Its prompt cache (Claude models).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache: Option<CacheTtl>,
}

/// A git worktree made for a worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Worktree {
    /// The repository's top folder (the lead's checkout).
    pub repo: PathBuf,
    /// The worktree's folder.
    pub path: PathBuf,
    pub branch: String,
    /// The commit it was made from.
    pub base: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Running,
    Finished,
    Stopped,
    Failed(String),
    /// fin ended while it ran.
    Interrupted,
    /// Its worktree was removed.
    Closed,
    /// Its workflow couldn't be opened.
    Unavailable(String),
}

/// What continues a worker that isn't running.
enum Last {
    Nothing,
    Answered(Box<AgentResult>),
    Stopped(Box<StoppedRun>),
}

/// How a worker's run starts.
enum Launch {
    New(String),
    Continue(Box<AgentResult>, String),
    Restart(Box<StoppedRun>, String),
}

/// A note posted to a run, which it may not have read yet.
struct Posted {
    seq: u64,
    at: DateTime<Utc>,
    /// Its source, as the note names it.
    source: String,
    /// The run was already continued once to read it.
    redelivered: bool,
    /// For the lead: the event the note carries.
    event: Option<Event>,
}

/// A workflow's event printer: its control channel, and its task.
type Printer = (mpsc::UnboundedSender<Control>, JoinHandle<()>);

struct Worker {
    record: Record,
    workflow: Option<Arc<Workflow>>,
    printer: Option<Printer>,
    run: Option<JoinHandle<()>>,
    status: Status,
    last: Last,
    /// When the status last changed.
    since: DateTime<Utc>,
    /// The latest report.
    report: Option<String>,
    /// A `wait_for_lead` call is waiting.
    waiting: bool,
    /// The lead's messages for that call.
    inbox: Vec<String>,
    /// The lead's notes to the run going on.
    posted: Vec<Posted>,
    /// The lead stopped it (no event is sent for the stop).
    stop_requested: bool,
}

/// Something a worker sends the lead.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub worker: String,
    pub kind: EventKind,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// It finished; the body is its report.
    Report,
    /// It sent a message and goes on.
    Message,
    /// It stopped with an error, the body.
    Failed,
}

impl Event {
    fn heading(&self) -> String {
        match self.kind {
            EventKind::Report => format!("{} finished. Its report:", self.worker),
            EventKind::Message => {
                format!("{} sent you a message (it goes on working):", self.worker)
            }
            EventKind::Failed => format!("{} stopped with an error:", self.worker),
        }
    }

    /// As one text: in a note, or a tool's result.
    pub fn text(&self) -> String {
        let mut text = format!("{}\n\n{}", self.heading(), self.body.trim());
        if self.kind == EventKind::Failed {
            text.push_str("\n\n`message_worker` continues it from where it was, if it can.");
        }
        text
    }
}

/// The lead's side.
#[derive(Default)]
struct Lead {
    /// Its turn is running.
    running: bool,
    notes: Option<NotesHandle>,
    /// The `wait_for_workers` calls waiting, each with the workers it waits
    /// for (`None`: any).
    waiters: Vec<(u64, Option<Vec<String>>)>,
    next_waiter: u64,
    /// Events it hasn't been given.
    pending: Vec<Event>,
    /// Events given as notes to its run, which it may not have read.
    posted: Vec<Posted>,
}

#[derive(Default)]
struct State {
    workers: Vec<Worker>,
    next: u32,
    lead: Lead,
    /// Bumped when the user types during the lead's turn: its waits return.
    user_notes: u64,
    /// fin is closing: stopped workers aren't reopened.
    closing: bool,
}

impl State {
    fn worker(&mut self, id: &str) -> Result<&mut Worker, String> {
        let known = self
            .workers
            .iter()
            .map(|worker| worker.record.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        self.workers
            .iter_mut()
            .find(|worker| worker.record.id == id)
            .ok_or_else(|| {
                if known.is_empty() {
                    format!("there is no worker `{id}`: none has been started")
                } else {
                    format!("there is no worker `{id}`; the workers are {known}")
                }
            })
    }

    fn running(&self) -> usize {
        self.workers
            .iter()
            .filter(|worker| worker.status == Status::Running)
            .count()
    }
}

pub struct Crew {
    setup: Setup,
    me: Weak<Crew>,
    state: Mutex<State>,
    /// Wakes the waits when something changes.
    changed: Notify,
    /// Tells fin's loop that the lead has mail, or a worker stopped
    /// running.
    wake: mpsc::UnboundedSender<()>,
    /// Worktrees are made one at a time (git locks the repository's files).
    worktrees: tokio::sync::Mutex<()>,
}

impl Crew {
    pub fn new(setup: Setup) -> (Arc<Self>, mpsc::UnboundedReceiver<()>) {
        let (wake, woken) = mpsc::unbounded_channel();
        let crew = Arc::new_cyclic(|me| Self {
            setup,
            me: me.clone(),
            state: Mutex::new(State::default()),
            changed: Notify::new(),
            wake,
            worktrees: tokio::sync::Mutex::new(()),
        });
        (crew, woken)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn me(&self) -> Arc<Self> {
        self.me.upgrade().expect("the crew is alive")
    }

    fn out(&self) -> &Output {
        &self.setup.output
    }

    fn folder(&self) -> PathBuf {
        self.setup.session_dir.join("workers")
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.folder().join(id).join("run")
    }

    /// The lead's tools.
    pub fn lead_tools(&self) -> Vec<Arc<dyn Tool>> {
        LEAD_TOOL_NAMES
            .iter()
            .map(|name| {
                Arc::new(CrewTool {
                    crew: self.me.clone(),
                    name,
                    worker: None,
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    fn worker_tools(&self, id: &str) -> Vec<Arc<dyn Tool>> {
        WORKER_TOOL_NAMES
            .iter()
            .map(|name| {
                Arc::new(CrewTool {
                    crew: self.me.clone(),
                    name,
                    worker: Some(id.to_string()),
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    // ----- the session's workers -----

    /// Opens the workers of a resumed session, as their checkpoints left
    /// them. Those that ran when fin ended are marked interrupted: a
    /// message continues them.
    pub fn load(&self) -> Vec<String> {
        let path = self.folder().join(REGISTRY_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let records: Vec<Record> = match serde_json::from_str(&text) {
            Ok(records) => records,
            Err(error) => {
                return vec![format!(
                    "the session's workers couldn't be read from {}: {error}",
                    path.display()
                )];
            }
        };
        let mut problems = Vec::new();
        let mut workers = Vec::new();
        for record in records {
            let mut worker = Worker {
                workflow: None,
                printer: None,
                run: None,
                status: Status::Closed,
                last: Last::Nothing,
                since: Utc::now(),
                report: None,
                waiting: false,
                inbox: Vec::new(),
                posted: Vec::new(),
                stop_requested: false,
                record,
            };
            if !worker.record.removed {
                match self.reopen(&worker.record) {
                    Ok((workflow, last, printer)) => {
                        worker.status = match &last {
                            Last::Stopped(run) if run.restart == Restart::Recover => {
                                Status::Interrupted
                            }
                            Last::Stopped(_) => Status::Stopped,
                            Last::Answered(_) => Status::Finished,
                            Last::Nothing => {
                                Status::Failed("it has no run to continue".to_string())
                            }
                        };
                        if let Last::Answered(result) = &last {
                            worker.report = Some(result.summary.clone());
                        }
                        worker.workflow = Some(workflow);
                        worker.printer = Some(printer);
                        worker.last = last;
                    }
                    Err(error) => {
                        problems.push(format!("worker {}: {error}", worker.record.id));
                        worker.status = Status::Unavailable(error);
                    }
                }
            }
            workers.push(worker);
        }
        let mut state = self.lock();
        state.next = workers
            .iter()
            .map(|worker| worker.record.number)
            .max()
            .unwrap_or(0);
        state.workers = workers;
        problems
    }

    fn save(&self, state: &State) {
        let records = state
            .workers
            .iter()
            .map(|worker| worker.record.clone())
            .collect::<Vec<_>>();
        let folder = self.folder();
        let result = std::fs::create_dir_all(&folder).and_then(|()| {
            let text = serde_json::to_string_pretty(&records)?;
            let temporary = folder.join(format!("{REGISTRY_FILE}.tmp"));
            std::fs::write(&temporary, text)?;
            std::fs::rename(temporary, folder.join(REGISTRY_FILE))
        });
        if let Err(error) = result {
            log::warn!("could not save the workers' records: {error}");
        }
    }

    /// A worker's workflow, new or from its latest checkpoint, with a
    /// printer for its events.
    fn build(
        &self,
        record: &Record,
        checkpoint: Option<&Path>,
    ) -> Result<(Arc<Workflow>, Printer), String> {
        let run_dir = self.run_dir(&record.id);
        // A resume needs the configuration the checkpoint was saved with.
        let config = match checkpoint {
            Some(checkpoint) => crate::saved_setup(checkpoint, &run_dir)?
                .configs
                .remove(WORKER)
                .unwrap_or_else(|| self.setup.worker_config.clone()),
            None => {
                let mut config = self.setup.worker_config.clone();
                if let Some(cache) = record.cache {
                    cache.apply(&mut config);
                }
                config
            }
        };
        let mut builder = Workflow::builder()
            .name(WORKFLOW_NAME)
            .workspace(&record.dir)
            .llm_client(self.setup.client.clone())
            .agent(WORKER, config)
            .persistence(
                PersistenceConfig::new(run_dir)
                    .event_log(EventLog::LifecycleOnly)
                    .keep_last_checkpoints(3),
            )
            .disable_messaging()
            .cancel_grace_period(GRACE);
        for tool in self.setup.tools.iter().cloned() {
            builder = builder.agent_tool(WORKER, tool);
        }
        for tool in self.worker_tools(&record.id) {
            builder = builder.agent_tool(WORKER, tool);
        }
        if let Some(checkpoint) = checkpoint {
            builder = builder.resume_from(checkpoint);
        }
        let workflow = Arc::new(builder.build().map_err(|error| error.to_string())?);
        let (control, controls) = mpsc::unbounded_channel();
        let printer = tokio::spawn(ui::print_events(
            workflow.subscribe_events(),
            controls,
            self.setup.output.clone(),
            self.setup.stats.clone(),
            self.setup.show_thinking,
            Some(record.id.clone()),
        ));
        Ok((workflow, (control, printer)))
    }

    /// A worker's workflow from its latest checkpoint, and what continues
    /// it.
    fn reopen(&self, record: &Record) -> Result<(Arc<Workflow>, Last, Printer), String> {
        if !record.dir.is_dir() {
            return Err(format!("its folder {} is gone", record.dir.display()));
        }
        let checkpoint = crate::session::latest_checkpoint_in(&self.run_dir(&record.id))
            .ok_or_else(|| "it saved no checkpoint".to_string())?;
        let (workflow, printer) = self.build(record, Some(&checkpoint))?;
        let last = last_of(&workflow);
        Ok((workflow, last, printer))
    }

    // ----- the lead's tools -----

    async fn start(
        &self,
        task: String,
        worktree: bool,
        cache: Option<CacheTtl>,
    ) -> Result<String, String> {
        let task = task.trim().to_string();
        if task.is_empty() {
            return Err("give the worker a task".into());
        }
        // Only Claude models have a choice.
        let cache = self.setup.cache.map(|default| cache.unwrap_or(default));
        let number = {
            let mut state = self.lock();
            if state.closing {
                return Err("fin is closing".into());
            }
            self.check_limit(&state)?;
            state.next += 1;
            state.next
        };
        let id = format!("w{number}");
        let (dir, worktree) = if worktree {
            let path = self.setup.session_dir.join("worktrees").join(&id);
            let branch = format!("fin/{}/{id}", self.setup.session_id);
            let _one_at_a_time = self.worktrees.lock().await;
            let (worktree, dir) = add_worktree(&self.setup.workspace, &path, &branch).await?;
            (dir, Some(worktree))
        } else {
            (self.setup.workspace.clone(), None)
        };
        let record = Record {
            id: id.clone(),
            number,
            task: task.clone(),
            dir: dir.clone(),
            worktree: worktree.clone(),
            created: Utc::now(),
            removed: false,
            cache,
        };
        let (workflow, printer) = self.build(&record, None)?;
        let prompt = worker_prompt(&id, &task, &dir, worktree.as_ref());
        {
            let mut state = self.lock();
            state.workers.push(Worker {
                record,
                workflow: Some(workflow.clone()),
                printer: Some(printer),
                run: None,
                status: Status::Running,
                last: Last::Nothing,
                since: Utc::now(),
                report: None,
                waiting: false,
                inbox: Vec::new(),
                posted: Vec::new(),
                stop_requested: false,
            });
            state.workers.sort_by_key(|worker| worker.record.number);
            self.save(&state);
            let run = tokio::spawn(self.me().drive(id.clone(), workflow, Launch::New(prompt)));
            if let Ok(worker) = state.worker(&id) {
                worker.run = Some(run);
            }
        }
        let place = match &worktree {
            Some(worktree) => format!(
                "in a worktree on branch `{}` ({})",
                worktree.branch,
                dir.display()
            ),
            None => "in the working directory".to_string(),
        };
        let cached = cache.map_or(String::new(), |cache| {
            format!(", with a {} prompt cache", cache.name())
        });
        self.out().line(&self.out().dim(&format!(
            "  ⇢ {id} started {place}{cached}: {}",
            ui::cut(&task, 120)
        )));
        Ok(format!(
            "Started worker {id}, {place}{}{cached}. It works while you go on; its report reaches you when it finishes (wait_for_workers waits for it).",
            match &worktree {
                Some(worktree) => format!(", made from commit {}", short(&worktree.base)),
                None => String::new(),
            }
        ))
    }

    fn check_limit(&self, state: &State) -> Result<(), String> {
        let max = self.setup.max_running;
        if max != crate::agents::NO_LIMIT && state.running() as u32 >= max {
            return Err(format!(
                "{max} worker{} running, the most this session allows (--workers); wait for one to finish (wait_for_workers) or stop one",
                if max == 1 { " is" } else { "s are" }
            ));
        }
        Ok(())
    }

    fn message(&self, id: &str, text: String) -> Result<String, String> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err("the message is empty".into());
        }
        let mut state = self.lock();
        let running = state.running();
        let limit = self.check_limit(&state);
        let worker = state.worker(id)?;
        let workflow = worker.workflow.clone();
        let launch = match (&worker.status, &worker.last) {
            (Status::Running, _) => {
                if worker.waiting {
                    worker.inbox.push(text);
                    drop(state);
                    self.changed.notify_waiters();
                    return Ok(format!("{id} was waiting for you and has your message."));
                }
                let workflow = workflow.ok_or("the worker has no workflow")?;
                let posted = post(&workflow, WORKER, LEAD, &text)?;
                worker.posted.push(Posted {
                    seq: posted.seq,
                    at: posted.posted_at,
                    source: LEAD.into(),
                    redelivered: false,
                    event: None,
                });
                return Ok(format!(
                    "Sent: {id} reads it at its next step ({running} worker{} running).",
                    if running == 1 { "" } else { "s" }
                ));
            }
            (Status::Closed, _) => {
                return Err(format!(
                    "{id}'s worktree was removed, so it can't continue; start a new worker"
                ));
            }
            (Status::Unavailable(why), _) => {
                return Err(format!("{id} can't continue: {why}; start a new worker"));
            }
            (_, Last::Answered(result)) => Launch::Continue(
                result.clone(),
                format!(
                    "# A message from the lead\n\nYou reported on your task; the lead sends you this. Carry it out, then call `complete_task` again with your report on it.\n\n{text}"
                ),
            ),
            (_, Last::Stopped(run)) => {
                if run.restart == Restart::Recover {
                    // A recovered run gets no message: the note reaches it.
                    limit.clone()?;
                    let workflow = workflow.clone().ok_or("the worker has no workflow")?;
                    post(&workflow, WORKER, LEAD, &text)?;
                    Launch::Restart(run.clone(), String::new())
                } else {
                    Launch::Restart(run.clone(), format!("A message from the lead:\n\n{text}"))
                }
            }
            (status, Last::Nothing) => {
                return Err(format!(
                    "{id} can't continue ({}); start a new worker",
                    describe_status(status)
                ));
            }
        };
        limit?;
        let workflow = workflow.ok_or("the worker has no workflow")?;
        let resumed = matches!(launch, Launch::Restart(..));
        let expired = match worker.record.cache {
            Some(cache) if Utc::now() - worker.since > cache.lasts() => format!(
                " Its {} prompt cache had expired (it stopped {} ago), so it rereads its conversation at full price.",
                cache.name(),
                ago(worker.since, Utc::now()).trim_start_matches("for ")
            ),
            _ => String::new(),
        };
        worker.status = Status::Running;
        worker.since = Utc::now();
        worker.stop_requested = false;
        worker.last = Last::Nothing;
        let run = tokio::spawn(self.me().drive(id.to_string(), workflow, launch));
        worker.run = Some(run);
        drop(state);
        self.out().line(&self.out().dim(&format!(
            "  ⇢ {id} {} with the lead's message",
            if resumed { "resumes" } else { "continues" }
        )));
        Ok(format!(
            "{id} {} its task with your message; its report reaches you when it finishes.{expired}",
            if resumed { "resumes" } else { "continues" }
        ))
    }

    async fn wait(&self, ids: Option<Vec<String>>, timeout: Duration) -> Result<String, String> {
        let (waiter, user_notes) = {
            let mut state = self.lock();
            if let Some(ids) = &ids {
                for id in ids {
                    state.worker(id)?;
                }
            }
            let waiter = state.lead.next_waiter;
            state.lead.next_waiter += 1;
            state.lead.waiters.push((waiter, ids.clone()));
            (waiter, state.user_notes)
        };
        let _guard = Unregister { crew: self, waiter };
        let wanted = |worker: &str| {
            ids.as_ref()
                .is_none_or(|ids| ids.iter().any(|id| id == worker))
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.lock();
                let (events, rest): (Vec<Event>, Vec<Event>) =
                    std::mem::take(&mut state.lead.pending)
                        .into_iter()
                        .partition(|event| wanted(&event.worker));
                state.lead.pending = rest;
                if !events.is_empty() {
                    let still = running_list(&state);
                    return Ok(format!(
                        "{}\n\n{}",
                        events
                            .iter()
                            .map(Event::text)
                            .collect::<Vec<_>>()
                            .join("\n\n---\n\n"),
                        still
                    ));
                }
                if state.user_notes != user_notes {
                    return Ok("The user typed something while you waited: it follows as a note. Wait again if you still need to.".into());
                }
                let running = state
                    .workers
                    .iter()
                    .filter(|worker| worker.status == Status::Running && wanted(&worker.record.id))
                    .count();
                if running == 0 {
                    let which = match &ids {
                        Some(ids) => ids
                            .iter()
                            .filter_map(|id| {
                                let worker = state.workers.iter().find(|w| &w.record.id == id)?;
                                Some(format!("{id} is {}", describe_status(&worker.status)))
                            })
                            .collect::<Vec<_>>()
                            .join("; "),
                        None => "no worker is running".to_string(),
                    };
                    return Ok(format!(
                        "Nothing to wait for: {which}. Reports that came in earlier reached you as notes or messages."
                    ));
                }
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => {
                    let state = self.lock();
                    return Ok(format!(
                        "No news in {} s. {}",
                        timeout.as_secs(),
                        running_list(&state)
                    ));
                }
            }
        }
    }

    fn list(&self) -> String {
        let state = self.lock();
        if state.workers.is_empty() {
            return "No workers yet.".into();
        }
        let now = Utc::now();
        let mut lines = Vec::new();
        for worker in &state.workers {
            let record = &worker.record;
            let place = match &record.worktree {
                Some(worktree) => format!(
                    "worktree {} (branch {}, from {})",
                    record.dir.display(),
                    worktree.branch,
                    short(&worktree.base)
                ),
                None => "in the working directory".into(),
            };
            let place = match record.cache {
                Some(cache) => format!("{place} · {} cache", cache.name()),
                None => place,
            };
            let mut line = format!(
                "{}: {} {} · {place}\n  task: {}",
                record.id,
                describe_status(&worker.status),
                ago(worker.since, now),
                ui::cut(&record.task, 200)
            );
            if worker.status != Status::Running
                && let Some(report) = &worker.report
            {
                line.push_str(&format!("\n  report: {}", ui::cut(report, 200)));
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    async fn stop(&self, id: &str, remove_worktree: bool) -> Result<String, String> {
        let (stopping, run) = {
            let mut state = self.lock();
            let worker = state.worker(id)?;
            if remove_worktree && worker.record.worktree.is_none() {
                return Err(format!(
                    "{id} works in the working directory: it has no worktree"
                ));
            }
            if worker.status == Status::Running {
                worker.stop_requested = true;
                if let Some(workflow) = &worker.workflow {
                    workflow.stop(StopRequest::operator("the lead stopped this worker"));
                }
                (
                    true,
                    if remove_worktree {
                        worker.run.take()
                    } else {
                        None
                    },
                )
            } else {
                (false, None)
            }
        };
        if !remove_worktree {
            return Ok(if stopping {
                format!("{id} is stopping. message_worker resumes it later, where it stopped.")
            } else {
                format!("{id} isn't running.")
            });
        }
        if let Some(run) = run {
            let _ = tokio::time::timeout(GRACE + Duration::from_secs(5), run).await;
        }
        let worktree = {
            let mut state = self.lock();
            let worker = state.worker(id)?;
            if worker.status == Status::Running {
                return Err(format!("{id} is still running; try again in a moment"));
            }
            worker.record.worktree.clone()
        };
        let Some(worktree) = worktree else {
            return Err(format!("{id} has no worktree"));
        };
        remove_worktree_at(&worktree).await?;
        let mut state = self.lock();
        let worker = state.worker(id)?;
        worker.record.removed = true;
        worker.status = Status::Closed;
        worker.since = Utc::now();
        worker.last = Last::Nothing;
        worker.workflow = None;
        if let Some((_, printer)) = worker.printer.take() {
            printer.abort();
        }
        self.save(&state);
        Ok(format!(
            "{}Removed {id}'s worktree {}; its branch `{}` stays.",
            if stopping {
                format!("Stopped {id}. ")
            } else {
                String::new()
            },
            worktree.path.display(),
            worktree.branch
        ))
    }

    // ----- a worker's tools -----

    fn message_lead(&self, id: &str, text: String) -> Result<String, String> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err("the message is empty".into());
        }
        self.out().line(
            &self
                .out()
                .dim(&format!("  ✉ {id} → lead: {}", ui::cut(&text, 160))),
        );
        self.deliver(Event {
            worker: id.to_string(),
            kind: EventKind::Message,
            body: text,
        });
        Ok(
            "Sent. The lead's answer, if any, reaches you as a note (or through wait_for_lead)."
                .into(),
        )
    }

    async fn wait_for_lead(&self, id: &str, timeout: Duration) -> Result<String, String> {
        {
            let mut state = self.lock();
            state.worker(id)?.waiting = true;
        }
        struct Done<'a>(&'a Crew, &'a str);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                if let Ok(worker) = self.0.lock().worker(self.1) {
                    worker.waiting = false;
                }
            }
        }
        let _done = Done(self, id);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.lock();
                let worker = state.worker(id)?;
                if !worker.inbox.is_empty() {
                    let messages = std::mem::take(&mut worker.inbox);
                    return Ok(format!(
                        "The lead says:\n\n{}",
                        messages.join("\n\n---\n\n")
                    ));
                }
                if state.closing {
                    return Ok("fin is closing; finish up and report.".into());
                }
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => {
                    return Ok(format!(
                        "No message from the lead in {} s. Go on with your best judgement, and say what you decided in your report.",
                        timeout.as_secs()
                    ));
                }
            }
        }
    }

    // ----- runs -----

    /// Runs a worker until it reports, fails or is stopped.
    async fn drive(self: Arc<Self>, id: String, workflow: Arc<Workflow>, mut launch: Launch) {
        loop {
            let outcome = Box::pin(async {
                match launch {
                    Launch::New(prompt) => workflow.run_agent(WORKER, &prompt).await,
                    Launch::Continue(previous, message) => {
                        workflow.continue_agent(&previous, &message).await
                    }
                    Launch::Restart(run, message) => workflow.restart(&run, &message).await,
                }
            })
            .await;
            match outcome {
                Ok(result) if result.status == AgentRunStatus::Completed => {
                    // A message that came as it finished: it goes on.
                    if self.unread_by_worker(&id, &result.task) {
                        launch = Launch::Continue(
                            Box::new(result),
                            "The lead sent you a message as you finished; it follows. Take it into account, then call `complete_task` again with your updated report.".into(),
                        );
                        continue;
                    }
                    self.finished(&id, result);
                }
                Ok(result) => {
                    let why = result
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("its run ended {:?}", result.status));
                    self.failed(&id, why, &workflow);
                }
                Err(WorkflowError::Cancelled) => self.stopped(&id),
                Err(error) => self.failed(&id, error.to_string(), &workflow),
            }
            break;
        }
    }

    /// Whether the run whose task is `task` left a note of the lead unread
    /// (the first time only).
    fn unread_by_worker(&self, id: &str, task: &Task) -> bool {
        let mut state = self.lock();
        let Ok(worker) = state.worker(id) else {
            return false;
        };
        let mut unread = false;
        worker.posted.retain_mut(|posted| {
            if posted.redelivered || was_read(task, posted) {
                return false;
            }
            posted.redelivered = true;
            unread = true;
            true
        });
        unread
    }

    fn finished(&self, id: &str, result: AgentResult) {
        let report = result.summary.trim().to_string();
        let file = self.folder().join(id).join("report.md");
        if let Err(error) = std::fs::create_dir_all(self.folder().join(id))
            .and_then(|()| std::fs::write(&file, &report))
        {
            log::warn!("could not save {id}'s report: {error}");
        }
        let lines = report.lines().count();
        self.out().line(&self.out().dim(&format!(
            "  ✓ {id} finished ({lines} line{} of report)",
            if lines == 1 { "" } else { "s" }
        )));
        let event = Event {
            worker: id.to_string(),
            kind: EventKind::Report,
            body: fit_note(&report, &file),
        };
        // The worker stops running and its report is the lead's at once
        // (under one lock): no one sees it stopped without its report.
        let mut state = self.lock();
        if let Ok(worker) = state.worker(id) {
            worker.status = Status::Finished;
            worker.since = Utc::now();
            worker.report = Some(report);
            worker.last = Last::Answered(Box::new(result));
            worker.posted.clear();
        }
        self.deliver_in(state, event);
    }

    fn failed(&self, id: &str, error: String, workflow: &Workflow) {
        let last = last_of(workflow);
        self.out().line(
            &self
                .out()
                .error(&format!("  ✗ {id} failed: {}", ui::cut(&error, 300))),
        );
        let mut state = self.lock();
        if let Ok(worker) = state.worker(id) {
            worker.status = Status::Failed(ui::cut(&error, 300));
            worker.since = Utc::now();
            worker.last = last;
            worker.posted.clear();
        }
        self.deliver_in(
            state,
            Event {
                worker: id.to_string(),
                kind: EventKind::Failed,
                body: error,
            },
        );
    }

    /// After a stop: the workflow is built again from its checkpoint, so a
    /// message can resume the worker. The lead hears nothing of a stop:
    /// it made it, or the user did.
    fn stopped(&self, id: &str) {
        let (record, closing, by_lead) = {
            let mut state = self.lock();
            let closing = state.closing;
            let Ok(worker) = state.worker(id) else {
                return;
            };
            (worker.record.clone(), closing, worker.stop_requested)
        };
        let reopened = if closing {
            None
        } else {
            Some(self.reopen(&record))
        };
        {
            let mut state = self.lock();
            if let Ok(worker) = state.worker(id) {
                worker.since = Utc::now();
                worker.posted.clear();
                match reopened {
                    None => worker.status = Status::Stopped,
                    Some(Ok((workflow, last, printer))) => {
                        worker.status = Status::Stopped;
                        worker.workflow = Some(workflow);
                        worker.last = last;
                        if let Some((_, old)) = worker.printer.replace(printer) {
                            old.abort();
                        }
                    }
                    Some(Err(error)) => {
                        worker.status = Status::Unavailable(format!(
                            "it was stopped, and couldn't be reopened: {error}"
                        ));
                    }
                }
            }
        }
        if !closing {
            self.out().line(&self.out().dim(&format!(
                "  ■ {id} stopped{}",
                if by_lead { " (by the lead)" } else { "" }
            )));
        }
        self.changed.notify_waiters();
        let _ = self.wake.send(());
    }

    /// Gives the lead `event`: to a wait that wants it, as a note while its
    /// turn runs, else to its mailbox.
    fn deliver(&self, event: Event) {
        self.deliver_in(self.lock(), event);
    }

    /// [`deliver`](Self::deliver), with the state locked; then the waits
    /// and fin's loop are woken.
    fn deliver_in(&self, mut state: MutexGuard<'_, State>, event: Event) {
        self.post_or_keep(&mut state, event);
        drop(state);
        self.changed.notify_waiters();
        let _ = self.wake.send(());
    }

    fn post_or_keep(&self, state: &mut State, event: Event) {
        let lead = &mut state.lead;
        let waited = lead
            .waiters
            .iter()
            .any(|(_, ids)| ids.as_ref().is_none_or(|ids| ids.contains(&event.worker)));
        if !waited
            && lead.running
            && let Some(notes) = &lead.notes
        {
            match notes.post(
                Note::new(event.text())
                    .to([MAIN])
                    .once()
                    .source(NoteSource::Agent(event.worker.clone())),
            ) {
                Ok(posted) => {
                    lead.posted.push(Posted {
                        seq: posted.seq,
                        at: posted.posted_at,
                        source: event.worker.clone(),
                        redelivered: false,
                        event: Some(event),
                    });
                    return;
                }
                Err(error) => log::warn!("could not pass {}'s news on: {error}", event.worker),
            }
        }
        lead.pending.push(event);
    }

    // ----- the lead's turns -----

    /// The lead's turn starts: news reaches it as notes through `notes`.
    pub fn lead_started(&self, notes: NotesHandle) {
        let mut state = self.lock();
        state.lead.running = true;
        state.lead.notes = Some(notes);
    }

    /// The lead's turn ended, with `result` when it replied. Returns how
    /// many notes it didn't get to read: they reach it when its
    /// conversation continues (the first time; after that they count as
    /// read).
    pub fn lead_ended(&self, result: Option<&AgentResult>) -> usize {
        let mut state = self.lock();
        state.lead.running = false;
        let Some(result) = result else {
            return state.lead.posted.len();
        };
        let mut unread = 0;
        state.lead.posted.retain_mut(|posted| {
            if posted.redelivered || was_read(&result.task, posted) {
                return false;
            }
            posted.redelivered = true;
            unread += 1;
            true
        });
        unread
    }

    /// The lead's conversation starts again (`/new`, or after a failure):
    /// the news it was given as notes and may not have read go to its
    /// mailbox (those `previous` shows it read stay read).
    pub fn lead_restarted(&self, previous: Option<&AgentResult>) {
        let mut state = self.lock();
        let posted = std::mem::take(&mut state.lead.posted);
        let unread = posted
            .into_iter()
            .filter(|posted| previous.is_none_or(|result| !was_read(&result.task, posted)))
            .filter_map(|posted| posted.event);
        let mut pending: Vec<Event> = unread.collect();
        pending.append(&mut state.lead.pending);
        state.lead.pending = pending;
    }

    /// The news the lead hasn't been given.
    pub fn take_mail(&self) -> Vec<Event> {
        std::mem::take(&mut self.lock().lead.pending)
    }

    /// News taken with [`take_mail`](Self::take_mail) that didn't reach
    /// the lead (its turn failed) goes back, first.
    pub fn put_back(&self, mut events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        let mut state = self.lock();
        events.append(&mut state.lead.pending);
        state.lead.pending = events;
    }

    pub fn has_mail(&self) -> bool {
        !self.lock().lead.pending.is_empty()
    }

    /// The user typed during the lead's turn: its waits return so it reads
    /// what they typed.
    pub fn user_typed(&self) {
        self.lock().user_notes += 1;
        self.changed.notify_waiters();
    }

    /// The workers running.
    pub fn running(&self) -> Vec<String> {
        self.lock()
            .workers
            .iter()
            .filter(|worker| worker.status == Status::Running)
            .map(|worker| worker.record.id.clone())
            .collect()
    }

    /// What the workers' models have cost, in dollars, with their input and
    /// output tokens.
    pub fn spent(&self) -> (f64, usize, usize) {
        let workflows = self
            .lock()
            .workers
            .iter()
            .filter_map(|worker| worker.workflow.clone())
            .collect::<Vec<_>>();
        let mut total = (0.0, 0, 0);
        for workflow in workflows {
            total.0 += workflow.context().budget.total_spent();
            for run in workflow.result().agent_results {
                total.1 += run.total_input_tokens();
                total.2 += run.total_output_tokens();
            }
        }
        total
    }

    /// `/workers`: each worker, a line or two.
    pub fn describe(&self) -> String {
        self.list()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().workers.is_empty()
    }

    /// Stops the workers running; returns how many.
    pub fn stop_all(&self, reason: &str) -> usize {
        let state = self.lock();
        let mut count = 0;
        for worker in &state.workers {
            if worker.status == Status::Running
                && let Some(workflow) = &worker.workflow
            {
                workflow.stop(StopRequest::operator(reason));
                count += 1;
            }
        }
        count
    }

    /// fin is closing: the workers running are stopped (a resumed session
    /// can continue them), and their printers end.
    pub async fn shutdown(&self) {
        let runs = {
            let mut state = self.lock();
            state.closing = true;
            let mut runs = Vec::new();
            for worker in &mut state.workers {
                if worker.status == Status::Running
                    && let Some(workflow) = &worker.workflow
                {
                    workflow.stop(StopRequest::operator("fin is closing"));
                }
                runs.extend(worker.run.take());
            }
            runs
        };
        self.changed.notify_waiters();
        for run in runs {
            let _ = tokio::time::timeout(GRACE + Duration::from_secs(5), run).await;
        }
        let printers = {
            let mut state = self.lock();
            state
                .workers
                .iter_mut()
                .filter_map(|worker| worker.printer.take())
                .collect::<Vec<_>>()
        };
        for (control, printer) in printers {
            let (done, flushed) = tokio::sync::oneshot::channel();
            if control.send(Control::Flush(done)).is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(1), flushed).await;
            }
            printer.abort();
        }
    }
}

/// Removes a `wait_for_workers` call's registration when it ends.
struct Unregister<'a> {
    crew: &'a Crew,
    waiter: u64,
}

impl Drop for Unregister<'_> {
    fn drop(&mut self) {
        self.crew
            .lock()
            .lead
            .waiters
            .retain(|(waiter, _)| *waiter != self.waiter);
    }
}

/// Posts `text` to `agent` as a note from `source`.
fn post(workflow: &Workflow, agent: &str, source: &str, text: &str) -> Result<PostedNote, String> {
    workflow
        .post_note(
            Note::new(text)
                .to([agent])
                .once()
                .source(NoteSource::Agent(source.to_string())),
        )
        .map_err(|error| format!("the message couldn't be passed on: {error}"))
}

/// Whether the run whose task is `task` has read the note `posted`: the
/// note is in its conversation, or was posted before its latest handover
/// (it was read before, or came after it, and the report covers it).
fn was_read(task: &Task, posted: &Posted) -> bool {
    if task
        .handovers
        .last()
        .is_some_and(|handover| handover.at >= posted.at)
    {
        return true;
    }
    let marker = format!("Note {} from agent `{}`", posted.seq, posted.source);
    task.conversation.messages.iter().any(|message| {
        message
            .content
            .iter()
            .filter_map(arko_ai::Content::as_text)
            .any(|text| text.contains(&marker))
    })
}

/// Where a workflow built from a checkpoint leaves its worker: stopped (or
/// interrupted), answered, or neither.
fn last_of(workflow: &Workflow) -> Last {
    if let Some(run) = workflow
        .stopped_runs()
        .into_iter()
        .rfind(|run| run.result.agent_name == WORKER)
    {
        return Last::Stopped(Box::new(run));
    }
    match workflow
        .result()
        .agent_results
        .into_iter()
        .rfind(|result| result.agent_name == WORKER)
    {
        Some(result) if result.status == AgentRunStatus::Completed => {
            Last::Answered(Box::new(result))
        }
        _ => Last::Nothing,
    }
}

/// The report as a note can hold it: whole, or cut, with the file it is
/// saved in.
fn fit_note(report: &str, file: &Path) -> String {
    if report.len() <= NOTE_BUDGET {
        return report.to_string();
    }
    let mut end = NOTE_BUDGET - 300;
    while !report.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[... the report is cut here; read all of it with read_file at {} ...]",
        &report[..end],
        file.display()
    )
}

fn describe_status(status: &Status) -> String {
    match status {
        Status::Running => "running".into(),
        Status::Finished => "finished".into(),
        Status::Stopped => "stopped".into(),
        Status::Failed(error) => format!("failed ({error})"),
        Status::Interrupted => "interrupted (fin ended while it ran)".into(),
        Status::Closed => "closed (its worktree was removed)".into(),
        Status::Unavailable(why) => format!("unavailable ({why})"),
    }
}

/// The workers still running, as a sentence.
fn running_list(state: &State) -> String {
    let running = state
        .workers
        .iter()
        .filter(|worker| worker.status == Status::Running)
        .map(|worker| worker.record.id.as_str())
        .collect::<Vec<_>>();
    match running.as_slice() {
        [] => "No worker is running now.".into(),
        [one] => format!("Still running: {one}."),
        many => format!("Still running: {}.", many.join(", ")),
    }
}

/// `for 3m`, `since 2h 5m ago`: how long a status has lasted.
fn ago(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - since).num_seconds().max(0);
    let span = if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    };
    format!("for {span}")
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(10)]
}

/// A worker's first message: its task, and where it works.
fn worker_prompt(id: &str, task: &str, dir: &Path, worktree: Option<&Worktree>) -> String {
    let place = match worktree {
        Some(worktree) => format!(
            "You work in `{}`: a git worktree of the lead's repository, on the branch `{}`, made from commit {}. The lead and the other workers work in other checkouts, so your changes reach them only when the lead merges your branch. Commit your changes on this branch before you report (don't switch branches, rebase or push), and name your commits in your report. Changes the lead hadn't committed aren't in this checkout.",
            dir.display(),
            worktree.branch,
            short(&worktree.base)
        ),
        None => format!(
            "You work in `{}`, the lead's working directory. The lead and other workers may be working there too: change only the files your task is about, and don't commit unless your task says to.",
            dir.display()
        ),
    };
    format!("{task}\n\n# Where you work\nYou are worker {id}. {place}")
}

// ----- git worktrees -----

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .map_err(|error| format!("could not run git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if error.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            error
        })
    }
}

/// Makes a worktree of the repository `workspace` is in, at `path`, on a
/// new branch `branch` from the current commit. Returns it, and the folder
/// in it that corresponds to `workspace`.
pub async fn add_worktree(
    workspace: &Path,
    path: &Path,
    branch: &str,
) -> Result<(Worktree, PathBuf), String> {
    let repo = git(workspace, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|_| {
            format!(
                "a worktree needs a git repository, and {} isn't in one; start the worker without a worktree",
                workspace.display()
            )
        })?;
    let repo = PathBuf::from(repo);
    let base = git(workspace, &["rev-parse", "--verify", "HEAD"])
        .await
        .map_err(|_| "the repository has no commit yet to make a worktree from".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let target = path.to_string_lossy().to_string();
    git(&repo, &["worktree", "add", "-b", branch, &target, &base])
        .await
        .map_err(|error| format!("could not make the worktree: {error}"))?;
    // The same place in the worktree as the working directory in the
    // repository.
    let relative = std::fs::canonicalize(workspace)
        .ok()
        .zip(std::fs::canonicalize(&repo).ok())
        .and_then(|(workspace, repo)| workspace.strip_prefix(&repo).ok().map(Path::to_path_buf))
        .unwrap_or_default();
    let dir = path.join(relative);
    let dir = if dir.is_dir() {
        dir
    } else {
        path.to_path_buf()
    };
    Ok((
        Worktree {
            repo,
            path: path.to_path_buf(),
            branch: branch.to_string(),
            base,
        },
        dir,
    ))
}

/// Removes a worktree (not its branch); git refuses while it has changes
/// that aren't committed.
pub async fn remove_worktree_at(worktree: &Worktree) -> Result<(), String> {
    let target = worktree.path.to_string_lossy().to_string();
    git(&worktree.repo, &["worktree", "remove", &target])
        .await
        .map(|_| ())
        .map_err(|error| {
            format!(
                "the worktree wasn't removed: {error}. Commit or discard its changes first (in {})",
                worktree.path.display()
            )
        })
}

// ----- the tools -----

/// One of the crew's tools, for the lead or for worker `worker`.
#[derive(Debug)]
struct CrewTool {
    crew: Weak<Crew>,
    name: &'static str,
    worker: Option<String>,
}

fn param(
    name: &str,
    description: &str,
    param_type: ToolParameterType,
    required: bool,
) -> ToolParameterDefinition {
    ToolParameterDefinition {
        name: name.into(),
        description: description.into(),
        param_type,
        required,
        is_path: false,
        flag: None,
        default_value: None,
    }
}

fn text_param(params: &HashMap<String, Value>, name: &str) -> Option<String> {
    match params.get(name)? {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn required(params: &HashMap<String, Value>, name: &str) -> Result<String, ToolError> {
    text_param(params, name).ok_or_else(|| ToolError::MissingParameter {
        param_name: name.to_string(),
    })
}

fn bool_param(params: &HashMap<String, Value>, name: &str) -> Result<bool, ToolError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(false),
        Some(Value::String(text)) if text.trim().eq_ignore_ascii_case("true") => Ok(true),
        Some(Value::String(text)) if text.trim().eq_ignore_ascii_case("false") => Ok(false),
        Some(other) => Err(ToolError::ParameterValidation {
            param_name: name.to_string(),
            message: format!("`{name}` is true or false, not {other}"),
        }),
    }
}

fn seconds_param(params: &HashMap<String, Value>, name: &str) -> Result<Duration, ToolError> {
    let seconds = match params.get(name) {
        None | Some(Value::Null) => DEFAULT_WAIT,
        Some(Value::Number(number)) => number.as_f64().map_or(DEFAULT_WAIT, |n| n.max(1.0) as u64),
        Some(Value::String(text)) if text.trim().is_empty() => DEFAULT_WAIT,
        Some(Value::String(text)) => text
            .trim()
            .parse::<f64>()
            .map_err(|_| ToolError::ParameterValidation {
                param_name: name.to_string(),
                message: format!("`{name}` is a number of seconds, not {text:?}"),
            })?
            .max(1.0) as u64,
        Some(other) => {
            return Err(ToolError::ParameterValidation {
                param_name: name.to_string(),
                message: format!("`{name}` is a number of seconds, not {other}"),
            });
        }
    };
    Ok(Duration::from_secs(seconds.min(MAX_WAIT)))
}

/// Worker IDs, from a list or a text with commas or spaces.
fn ids_param(params: &HashMap<String, Value>, name: &str) -> Option<Vec<String>> {
    let ids: Vec<String> = match params.get(name)? {
        Value::String(text) => text
            .split([',', ' '])
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(String::from)
            .collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str())
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect(),
        _ => return None,
    };
    (!ids.is_empty()).then_some(ids)
}

const WAIT_PARAM: &str = "timeout_seconds";

#[async_trait]
impl Tool for CrewTool {
    fn definition(&self) -> ToolDefinition {
        use ToolParameterType::{Bool, String as Text};
        let cache = self.crew.upgrade().and_then(|crew| crew.setup.cache);
        let (description, mut parameters, parallel_safe) = match self.name {
            START_WORKER => (
                "Starts a worker on a task and returns its ID (w1, w2, ...) at once: it works while you go on. Its report reaches you when it finishes. Start several in one response to run them in parallel.",
                vec![
                    param(
                        "task",
                        "The whole task: the goal, the context and constraints it needs (it doesn't see this conversation), what done looks like, and what to report.",
                        Text,
                        true,
                    ),
                    param(
                        "worktree",
                        "true: the worker works in a git worktree of its own (a checkout of the current commit, on a new branch) and commits its changes there. Use it for workers that change files. Default false: it works in the working directory.",
                        Bool,
                        false,
                    ),
                ],
                true,
            ),
            MESSAGE_WORKER => (
                "Sends a worker a message. A running worker reads it at its next step; one that has finished or was stopped continues its task with it, with its context.",
                vec![
                    param("worker", "The worker's ID, such as w1.", Text, true),
                    param("message", "The message.", Text, true),
                ],
                true,
            ),
            WAIT_FOR_WORKERS => (
                "Waits until a worker finishes or sends you a message, or the user types something, and returns what arrived (a report or message that came in earlier comes back at once). Use it when you can't go on without the workers' results.",
                vec![
                    param(
                        "workers",
                        "The IDs to wait for, separated by commas (default: any worker).",
                        Text,
                        false,
                    ),
                    param(
                        WAIT_PARAM,
                        "How long to wait at most, in seconds (default 600, at most 3600).",
                        Text,
                        false,
                    ),
                ],
                false,
            ),
            LIST_WORKERS => (
                "Lists the workers: what each is doing or reported, how long it has, and where it works.",
                vec![],
                true,
            ),
            STOP_WORKER => (
                "Stops a running worker (message_worker resumes it later). With remove_worktree, also removes its worktree once it is stopped (git refuses while it has uncommitted changes); its branch stays.",
                vec![
                    param("worker", "The worker's ID, such as w1.", Text, true),
                    param(
                        "remove_worktree",
                        "true: remove its worktree too, as when you have merged its branch. Default false.",
                        Bool,
                        false,
                    ),
                ],
                true,
            ),
            MESSAGE_LEAD => (
                "Sends the lead a message without ending your task, such as a finding it needs now or a question only it can answer. Your report at the end goes to it anyway.",
                vec![param("message", "The message.", Text, true)],
                true,
            ),
            _ => (
                "Waits for a message from the lead and returns it; a message that came while you didn't wait reached you as a note.",
                vec![param(
                    WAIT_PARAM,
                    "How long to wait at most, in seconds (default 600, at most 3600).",
                    Text,
                    false,
                )],
                false,
            ),
        };
        if let (START_WORKER, Some(default)) = (self.name, cache) {
            parameters.push(param(
                CACHE_PARAM,
                &format!(
                    "How long the worker's prompt cache lasts after its last step: `1h` when you expect to message it or give it follow-up work after it reports (continuing it then reuses the cache), `5m` for a quick one-off such as research (cheaper to write). Default `{}`.",
                    default.name()
                ),
                ToolParameterType::String,
                false,
            ));
        }
        ToolDefinition {
            name: self.name.to_string(),
            description: description.to_string(),
            output_based_example: None,
            parameters,
            parallel_safe,
            tool_type_details: ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: None,
        }
    }

    /// The waits have their own time limit.
    fn uses_host_timeout(&self) -> bool {
        !matches!(self.name, WAIT_FOR_WORKERS | WAIT_FOR_LEAD)
    }

    fn recovery_mode(&self) -> ToolRecovery {
        match self.name {
            WAIT_FOR_WORKERS | LIST_WORKERS | WAIT_FOR_LEAD => ToolRecovery::Rerun,
            _ => ToolRecovery::InDoubt,
        }
    }

    async fn execute(
        &self,
        _context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let crew = self
            .crew
            .upgrade()
            .ok_or_else(|| ToolError::ExecutionFailed("fin is closing".into()))?;
        let me = self.worker.as_deref().unwrap_or_default();
        let result = match self.name {
            START_WORKER => {
                let cache = match params.get(CACHE_PARAM).and_then(Value::as_str) {
                    Some(text) if !text.trim().is_empty() => {
                        Some(CacheTtl::parse(text).map_err(|message| {
                            ToolError::ParameterValidation {
                                param_name: CACHE_PARAM.to_string(),
                                message,
                            }
                        })?)
                    }
                    _ => None,
                };
                crew.start(
                    required(&params, "task")?,
                    bool_param(&params, "worktree")?,
                    cache,
                )
                .await
            }
            MESSAGE_WORKER => crew.message(
                required(&params, "worker")?.trim(),
                required(&params, "message")?,
            ),
            WAIT_FOR_WORKERS => {
                crew.wait(
                    ids_param(&params, "workers"),
                    seconds_param(&params, WAIT_PARAM)?,
                )
                .await
            }
            LIST_WORKERS => Ok(crew.list()),
            STOP_WORKER => {
                crew.stop(
                    required(&params, "worker")?.trim(),
                    bool_param(&params, "remove_worktree")?,
                )
                .await
            }
            MESSAGE_LEAD => crew.message_lead(me, required(&params, "message")?),
            _ => {
                crew.wait_for_lead(me, seconds_param(&params, WAIT_PARAM)?)
                    .await
            }
        };
        result.map_err(ToolError::ExecutionFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentSettings, Effort, ModelSettings, NO_LIMIT, resolve_model};
    use crate::mock_model::{ModelServer, complete, tool_call};

    fn settings_model(name: &str) -> ModelSettings {
        ModelSettings {
            model: resolve_model(name).unwrap().id,
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        }
    }

    fn config(main: bool) -> AgentConfig {
        config_on("opus", main)
    }

    fn config_on(model: &str, main: bool) -> AgentConfig {
        let model = settings_model(model);
        let settings = AgentSettings {
            main: &model,
            worker: &model,
            max_workers: NO_LIMIT,
            handover_at: 200_000,
            show_thinking: false,
            shell_name: "bash",
            allow_outside_workspace: false,
            mcp_tools: &[],
            mcp_servers: &[],
            skill_folders: &[],
            index_tools: &[],
        };
        if main {
            crate::agents::main_config(&settings).unwrap()
        } else {
            crate::agents::worker_config(&settings).unwrap()
        }
    }

    /// A crew in `dir`, without a model to call.
    fn crew(dir: &Path, max_running: u32) -> (Arc<Crew>, mpsc::UnboundedReceiver<()>) {
        crew_with(dir, max_running, LLMClient::new(), config(false))
    }

    fn crew_with(
        dir: &Path,
        max_running: u32,
        client: LLMClient,
        worker_config: AgentConfig,
    ) -> (Arc<Crew>, mpsc::UnboundedReceiver<()>) {
        Crew::new(Setup {
            client: Arc::new(client),
            worker_config,
            max_running,
            ..setup(dir)
        })
    }

    fn setup(dir: &Path) -> Setup {
        Setup {
            session_id: "s1".into(),
            session_dir: dir.join("session"),
            workspace: dir.to_path_buf(),
            client: Arc::new(LLMClient::new()),
            worker_config: config(false),
            tools: Vec::new(),
            output: Arc::new(Output::new(None, true)),
            stats: Arc::default(),
            show_thinking: false,
            max_running: NO_LIMIT,
            cache: None,
        }
    }

    /// A worker in the crew's records that runs no workflow.
    fn fake(crew: &Crew, id: &str, status: Status) {
        let mut state = crew.lock();
        state.next += 1;
        let number = state.next;
        state.workers.push(Worker {
            record: Record {
                id: id.into(),
                number,
                task: format!("the task of {id}"),
                dir: crew.setup.workspace.clone(),
                worktree: None,
                created: Utc::now(),
                removed: false,
                cache: None,
            },
            workflow: None,
            printer: None,
            run: None,
            status,
            last: Last::Nothing,
            since: Utc::now(),
            report: None,
            waiting: false,
            inbox: Vec::new(),
            posted: Vec::new(),
            stop_requested: false,
        });
    }

    fn event(worker: &str, kind: EventKind, body: &str) -> Event {
        Event {
            worker: worker.into(),
            kind,
            body: body.into(),
        }
    }

    fn params(json: Value) -> HashMap<String, Value> {
        serde_json::from_value(json).unwrap()
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn news_waits_in_the_mailbox_while_the_lead_is_idle() {
        let temp = tempfile::tempdir().unwrap();
        let (crew, mut woken) = crew(temp.path(), NO_LIMIT);
        crew.deliver(event("w1", EventKind::Report, "all done"));
        assert!(woken.try_recv().is_ok(), "fin's loop is woken");
        assert!(crew.has_mail());
        let mail = crew.take_mail();
        assert_eq!(mail, vec![event("w1", EventKind::Report, "all done")]);
        assert!(!crew.has_mail());
        // News that didn't reach the lead goes back, before newer news.
        crew.deliver(event("w2", EventKind::Message, "a question"));
        crew.put_back(mail);
        let workers: Vec<String> = crew.take_mail().into_iter().map(|e| e.worker).collect();
        assert_eq!(workers, ["w1", "w2"]);
    }

    #[tokio::test]
    async fn a_wait_returns_news_that_came_earlier_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let (crew, _woken) = crew(temp.path(), NO_LIMIT);
        let nothing = crew.wait(None, Duration::from_secs(5)).await.unwrap();
        assert!(nothing.starts_with("Nothing to wait for"), "{nothing}");
        crew.deliver(event("w1", EventKind::Report, "the report"));
        let text = crew.wait(None, Duration::from_secs(5)).await.unwrap();
        assert!(
            text.contains("w1 finished. Its report:\n\nthe report"),
            "{text}"
        );
        assert!(!crew.has_mail(), "the wait took it");
        assert!(
            crew.wait(Some(vec!["w9".into()]), Duration::from_secs(1))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_wait_returns_a_report_as_it_comes_and_leaves_other_news() {
        let temp = tempfile::tempdir().unwrap();
        let (crew, _woken) = crew(temp.path(), NO_LIMIT);
        fake(&crew, "w1", Status::Running);
        fake(&crew, "w2", Status::Running);
        let waiting = tokio::spawn({
            let crew = crew.clone();
            async move {
                crew.wait(Some(vec!["w2".into()]), Duration::from_secs(10))
                    .await
            }
        });
        settle().await;
        // News from a worker the wait isn't for waits in the mailbox.
        crew.deliver(event("w1", EventKind::Message, "by the way"));
        settle().await;
        assert!(!waiting.is_finished());
        crew.deliver(event("w2", EventKind::Report, "w2's report"));
        let text = waiting.await.unwrap().unwrap();
        assert!(text.contains("w2's report"), "{text}");
        assert!(text.contains("Still running: w1, w2."), "{text}");
        let mail = crew.take_mail();
        assert_eq!(mail, vec![event("w1", EventKind::Message, "by the way")]);
        assert!(crew.lock().lead.waiters.is_empty(), "the wait unregistered");
    }

    #[tokio::test]
    async fn the_user_typing_ends_a_wait() {
        let temp = tempfile::tempdir().unwrap();
        let (crew, _woken) = crew(temp.path(), NO_LIMIT);
        fake(&crew, "w1", Status::Running);
        let waiting = tokio::spawn({
            let crew = crew.clone();
            async move { crew.wait(None, Duration::from_secs(10)).await }
        });
        settle().await;
        crew.user_typed();
        let text = waiting.await.unwrap().unwrap();
        assert!(text.starts_with("The user typed something"), "{text}");
        let timeout = crew.wait(None, Duration::from_millis(100)).await.unwrap();
        assert!(timeout.starts_with("No news in"), "{timeout}");
    }

    #[tokio::test]
    async fn news_reaches_a_running_lead_as_a_note() {
        let temp = tempfile::tempdir().unwrap();
        let (crew, _woken) = crew(temp.path(), NO_LIMIT);
        let workflow = Workflow::builder()
            .name("test")
            .workspace(temp.path())
            .llm_client(Arc::new(LLMClient::new()))
            .agent(MAIN, config(true))
            .disable_messaging()
            .build()
            .unwrap();
        crew.lead_started(workflow.notes_handle());
        crew.deliver(event("w1", EventKind::Report, "the report"));
        assert!(!crew.has_mail(), "it went as a note");
        // The turn ended without reading it: a new conversation gets it.
        assert_eq!(crew.lead_ended(None), 1);
        crew.lead_restarted(None);
        assert_eq!(
            crew.take_mail(),
            vec![event("w1", EventKind::Report, "the report")]
        );
        // With the lead idle, news waits in the mailbox.
        crew.deliver(event("w1", EventKind::Message, "more"));
        assert!(crew.has_mail());
    }

    #[test]
    fn a_note_counts_as_read_once_it_is_in_the_conversation_or_handed_over() {
        let temp = tempfile::tempdir().unwrap();
        let mut task = Task::new(config(true), "task".into(), temp.path().to_path_buf(), None);
        let posted = |seq| Posted {
            seq,
            at: Utc::now(),
            source: "w1".into(),
            redelivered: false,
            event: None,
        };
        assert!(!was_read(&task, &posted(3)));
        task.conversation.messages.push(arko_ai::Message::new_user(
            "Notes:\n- Note 3 from agent `w1`, 2026-10-10 05:30 UTC:\n  w1 finished.",
        ));
        assert!(was_read(&task, &posted(3)));
        assert!(!was_read(&task, &posted(4)));
        let earlier = Posted {
            at: Utc::now() - chrono::Duration::minutes(5),
            ..posted(4)
        };
        task.handovers.push(arko_agents::HandoverRecord {
            at: Utc::now(),
            input_tokens: 0,
            replaced_messages: 0,
            report: String::new(),
        });
        assert!(was_read(&task, &earlier), "posted before the handover");
        assert!(!was_read(&task, &posted(4)), "posted after it");
    }

    #[tokio::test]
    async fn a_waiting_worker_gets_the_leads_message() {
        let temp = tempfile::tempdir().unwrap();
        let (crew, _woken) = crew(temp.path(), NO_LIMIT);
        fake(&crew, "w1", Status::Running);
        let waiting = tokio::spawn({
            let crew = crew.clone();
            async move { crew.wait_for_lead("w1", Duration::from_secs(10)).await }
        });
        settle().await;
        let sent = crew
            .message("w1", "use the second approach".into())
            .unwrap();
        assert!(sent.contains("was waiting"), "{sent}");
        let text = waiting.await.unwrap().unwrap();
        assert_eq!(text, "The lead says:\n\nuse the second approach");
        assert!(!crew.lock().worker("w1").unwrap().waiting);
    }

    #[tokio::test]
    async fn a_worker_message_reaches_the_lead() {
        let temp = tempfile::tempdir().unwrap();
        let (crew, _woken) = crew(temp.path(), NO_LIMIT);
        fake(&crew, "w1", Status::Running);
        crew.message_lead("w1", "found it: src/x.rs".into())
            .unwrap();
        let mail = crew.take_mail();
        assert_eq!(mail.len(), 1);
        assert_eq!(
            mail[0].text(),
            "w1 sent you a message (it goes on working):\n\nfound it: src/x.rs"
        );
        assert!(crew.message_lead("w1", "  ".into()).is_err());
    }

    #[tokio::test]
    async fn messages_say_what_cant_be_done() {
        let temp = tempfile::tempdir().unwrap();
        let (crew, _woken) = crew(temp.path(), 1);
        let none = crew.message("w1", "hi".into()).unwrap_err();
        assert!(none.contains("none has been started"), "{none}");
        fake(&crew, "w1", Status::Finished);
        let unknown = crew.message("w2", "hi".into()).unwrap_err();
        assert!(unknown.contains("the workers are w1"), "{unknown}");
        let stuck = crew.message("w1", "hi".into()).unwrap_err();
        assert!(stuck.contains("can't continue"), "{stuck}");
        assert!(crew.message("w1", " ".into()).is_err());
        let no_worktree = crew.stop("w1", true).await.unwrap_err();
        assert!(no_worktree.contains("no worktree"), "{no_worktree}");
        assert_eq!(crew.stop("w1", false).await.unwrap(), "w1 isn't running.");
        // The limit counts the workers running.
        fake(&crew, "w2", Status::Running);
        let full = crew
            .start("another task".into(), false, None)
            .await
            .unwrap_err();
        assert!(full.starts_with("1 worker is running"), "{full}");
        let list = crew.list();
        assert!(list.contains("w1: finished"), "{list}");
        assert!(list.contains("w2: running"), "{list}");
        assert!(list.contains("task: the task of w2"), "{list}");
        assert_eq!(crew.running(), vec!["w2".to_string()]);
    }

    #[test]
    fn events_read_as_news() {
        let failed = event("w3", EventKind::Failed, "the model refused").text();
        assert!(failed.starts_with("w3 stopped with an error:\n\nthe model refused"));
        assert!(failed.contains("`message_worker` continues it"));
        assert_eq!(
            event("w1", EventKind::Report, " done \n").text(),
            "w1 finished. Its report:\n\ndone"
        );
    }

    #[test]
    fn a_long_report_is_cut_to_fit_a_note() {
        let file = Path::new("/s/workers/w1/report.md");
        assert_eq!(fit_note("short", file), "short");
        let long = "é".repeat(NOTE_BUDGET);
        let note = fit_note(&long, file);
        assert!(note.len() <= NOTE_BUDGET, "{}", note.len());
        assert!(note.ends_with("read all of it with read_file at /s/workers/w1/report.md ...]"));
    }

    #[test]
    fn the_tools_parameters_are_read_leniently() {
        let p = params(serde_json::json!({
            "a": "w1, w2 w3",
            "b": ["w4", " "],
            "c": "",
            "yes": "True",
            "no": false,
            "odd": 3,
            "secs": "30",
            "big": 99999,
            "zero": 0,
            "bad": "soon"
        }));
        assert_eq!(ids_param(&p, "a").unwrap(), ["w1", "w2", "w3"]);
        assert_eq!(ids_param(&p, "b").unwrap(), ["w4"]);
        assert_eq!(ids_param(&p, "c"), None);
        assert_eq!(ids_param(&p, "missing"), None);
        assert!(bool_param(&p, "yes").unwrap());
        assert!(!bool_param(&p, "no").unwrap());
        assert!(!bool_param(&p, "missing").unwrap());
        assert!(bool_param(&p, "odd").is_err());
        assert_eq!(seconds_param(&p, "secs").unwrap(), Duration::from_secs(30));
        assert_eq!(
            seconds_param(&p, "big").unwrap(),
            Duration::from_secs(MAX_WAIT)
        );
        assert_eq!(seconds_param(&p, "zero").unwrap(), Duration::from_secs(1));
        assert_eq!(
            seconds_param(&p, "missing").unwrap(),
            Duration::from_secs(DEFAULT_WAIT)
        );
        assert!(seconds_param(&p, "bad").is_err());
        assert!(required(&p, "c").is_err());
        assert_eq!(required(&p, "secs").unwrap(), "30");
    }

    #[test]
    fn a_worker_is_told_where_it_works() {
        let worktree = Worktree {
            repo: "/repo".into(),
            path: "/s/worktrees/w2".into(),
            branch: "fin/s1/w2".into(),
            base: "0123456789abcdef".into(),
        };
        let prompt = worker_prompt(
            "w2",
            "Fix the bug.",
            Path::new("/s/worktrees/w2/app"),
            Some(&worktree),
        );
        assert!(prompt.starts_with("Fix the bug.\n\n# Where you work\nYou are worker w2."));
        assert!(prompt.contains("on the branch `fin/s1/w2`, made from commit 0123456789."));
        assert!(prompt.contains("Commit your changes on this branch"));
        let shared = worker_prompt("w1", "Look.", Path::new("/repo"), None);
        assert!(shared.contains("`/repo`, the lead's working directory"));
    }

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=fin", "-c", "user.email=fin@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn a_worktree_is_made_at_the_working_directorys_place_and_removed() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let workspace = repo.join("app");
        std::fs::create_dir_all(&workspace).unwrap();
        run_git(&repo, &["init", "-q"]);
        // Without a commit, there is nothing to make a worktree from.
        let place = temp.path().join("session/worktrees/w1");
        let early = add_worktree(&workspace, &place, "fin/s1/w1")
            .await
            .unwrap_err();
        assert!(early.contains("no commit"), "{early}");
        std::fs::write(workspace.join("main.rs"), "fn main() {}\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-q", "-m", "first"]);
        let head = run_git(&repo, &["rev-parse", "HEAD"]);

        let (worktree, dir) = add_worktree(&workspace, &place, "fin/s1/w1").await.unwrap();
        assert_eq!(dir, place.join("app"));
        assert_eq!(worktree.base, head);
        assert_eq!(worktree.branch, "fin/s1/w1");
        assert_eq!(
            std::fs::canonicalize(&worktree.repo).unwrap(),
            std::fs::canonicalize(&repo).unwrap()
        );
        assert!(dir.join("main.rs").is_file());
        assert_eq!(run_git(&dir, &["branch", "--show-current"]), "fin/s1/w1");

        // git keeps a worktree with changes that aren't committed.
        std::fs::write(dir.join("main.rs"), "changed").unwrap();
        let refused = remove_worktree_at(&worktree).await.unwrap_err();
        assert!(refused.contains("Commit or discard"), "{refused}");
        run_git(&dir, &["commit", "-q", "-am", "work"]);
        remove_worktree_at(&worktree).await.unwrap();
        assert!(!place.exists());
        // Its branch stays, with the work.
        assert_eq!(
            run_git(&repo, &["log", "-1", "--format=%s", "fin/s1/w1"]),
            "work"
        );

        let outside = tempfile::tempdir().unwrap();
        let not_a_repo = add_worktree(outside.path(), &temp.path().join("w2"), "fin/s1/w2")
            .await
            .unwrap_err();
        assert!(
            not_a_repo.contains("needs a git repository"),
            "{not_a_repo}"
        );
    }

    /// A worker runs, reports, and continues its task with the lead's
    /// message; a resumed session finds it finished, with its report.
    #[tokio::test]
    async fn a_worker_reports_and_continues_with_a_message() {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let server = ModelServer::start(Arc::new(move |index, body: String| {
            seen.lock().unwrap().push(body);
            Box::pin(async move {
                match index {
                    0 => complete("a", "There are 3 files."),
                    _ => complete("b", "Plan B: 4 files."),
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (crew, _woken) = crew_with(
            temp.path(),
            NO_LIMIT,
            server.client(),
            config_on("luna", false),
        );
        let started = crew
            .start("Count the files.".into(), false, None)
            .await
            .unwrap();
        assert!(
            started.starts_with("Started worker w1, in the working directory"),
            "{started}"
        );
        let report = crew
            .wait(Some(vec!["w1".into()]), Duration::from_secs(20))
            .await
            .unwrap();
        assert!(
            report.contains("w1 finished. Its report:\n\nThere are 3 files."),
            "{report}"
        );
        assert!(bodies.lock().unwrap()[0].contains("Count the files."));
        let saved = temp.path().join("session/workers/w1/report.md");
        assert_eq!(
            std::fs::read_to_string(saved).unwrap(),
            "There are 3 files."
        );

        let sent = crew.message("w1", "Use plan B.".into()).unwrap();
        assert!(sent.contains("continues its task"), "{sent}");
        let report = crew
            .wait(Some(vec!["w1".into()]), Duration::from_secs(20))
            .await
            .unwrap();
        assert!(report.contains("Plan B: 4 files."), "{report}");
        assert!(bodies.lock().unwrap()[1].contains("Use plan B."));
        assert!(crew.list().contains("w1: finished"));
        let (_, input, output) = crew.spent();
        assert!(input > 0 && output > 0);
        crew.shutdown().await;

        // A resumed session's crew opens it from its checkpoint.
        let (again, _woken) = crew_with(
            temp.path(),
            NO_LIMIT,
            server.client(),
            config_on("luna", false),
        );
        assert_eq!(again.load(), Vec::<String>::new());
        let list = again.list();
        assert!(list.contains("w1: finished"), "{list}");
        assert!(list.contains("report: Plan B: 4 files."), "{list}");
        again.shutdown().await;
    }

    /// A worker's message reaches the lead as it goes on; the lead's
    /// message that arrives as the worker finishes isn't lost: the worker
    /// continues, and reads it.
    #[tokio::test]
    async fn messages_go_both_ways_while_a_worker_runs() {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let release = Arc::new(Notify::new());
        let hold = release.clone();
        let server = ModelServer::start(Arc::new(move |index, body: String| {
            seen.lock().unwrap().push(body);
            let hold = hold.clone();
            Box::pin(async move {
                match index {
                    0 => tool_call(
                        "a",
                        MESSAGE_LEAD,
                        serde_json::json!({"message": "Found the bug in parse.rs."}),
                    ),
                    1 => {
                        // The lead's message comes while this request runs.
                        hold.notified().await;
                        complete("b", "Fixed parse.rs.")
                    }
                    _ => complete(
                        "c",
                        "Fixed parse.rs, and added the test the lead asked for.",
                    ),
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let (crew, _woken) = crew_with(
            temp.path(),
            NO_LIMIT,
            server.client(),
            config_on("luna", false),
        );
        crew.start("Fix the parser.".into(), false, None)
            .await
            .unwrap();
        let message = crew.wait(None, Duration::from_secs(20)).await.unwrap();
        assert!(
            message.starts_with(
                "w1 sent you a message (it goes on working):\n\nFound the bug in parse.rs."
            ),
            "{message}"
        );
        // The worker is in its second request now.
        for _ in 0..200 {
            if bodies.lock().unwrap().len() >= 2 {
                break;
            }
            settle().await;
        }
        let sent = crew.message("w1", "Add a test too.".into()).unwrap();
        assert!(
            sent.starts_with("Sent: w1 reads it at its next step"),
            "{sent}"
        );
        release.notify_one();
        let report = crew.wait(None, Duration::from_secs(20)).await.unwrap();
        assert!(
            report.contains("added the test the lead asked for"),
            "{report}"
        );
        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(
            bodies.len(),
            3,
            "the worker continued once to read the note"
        );
        assert!(bodies[2].contains("from agent `lead`"), "{}", bodies[2]);
        assert!(bodies[2].contains("Add a test too."));
        assert!(!crew.has_mail(), "one report, the last");
        crew.shutdown().await;
    }

    /// The lead stops a worker in the middle of a request; a message
    /// resumes it where it stopped, in a worktree of its own.
    #[tokio::test]
    async fn a_stopped_worker_resumes_with_a_message() {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let server = ModelServer::start(Arc::new(move |index, body: String| {
            seen.lock().unwrap().push(body);
            Box::pin(async move {
                match index {
                    // Never answered: the worker is stopped while it waits.
                    0 => std::future::pending::<String>().await,
                    _ => complete("b", "Resumed and done."),
                }
            })
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-q", "-m", "first"]);
        let (crew, mut woken) =
            crew_with(&repo, NO_LIMIT, server.client(), config_on("luna", false));
        let started = crew.start("Edit a.txt.".into(), true, None).await.unwrap();
        assert!(
            started.contains("in a worktree on branch `fin/s1/w1`"),
            "{started}"
        );
        let place = repo.join("session/worktrees/w1");
        assert!(place.join("a.txt").is_file());
        for _ in 0..200 {
            if !bodies.lock().unwrap().is_empty() {
                break;
            }
            settle().await;
        }
        let stopping = crew.stop("w1", false).await.unwrap();
        assert!(stopping.starts_with("w1 is stopping"), "{stopping}");
        tokio::time::timeout(Duration::from_secs(20), woken.recv())
            .await
            .unwrap();
        let list = crew.list();
        assert!(list.contains("w1: stopped"), "{list}");
        assert!(!crew.has_mail(), "a stop sends the lead nothing");

        let resumed = crew.message("w1", "Go on.".into()).unwrap();
        assert!(resumed.contains("resumes its task"), "{resumed}");
        let report = crew.wait(None, Duration::from_secs(20)).await.unwrap();
        assert!(report.contains("Resumed and done."), "{report}");
        assert!(bodies.lock().unwrap()[1].contains("Go on."));

        let removed = crew.stop("w1", true).await.unwrap();
        assert!(
            removed.contains("its branch `fin/s1/w1` stays"),
            "{removed}"
        );
        assert!(!place.exists());
        let closed = crew.message("w1", "More.".into()).unwrap_err();
        assert!(closed.contains("worktree was removed"), "{closed}");
        crew.shutdown().await;
    }

    #[test]
    fn a_claude_worker_s_cache_is_chosen_at_its_start() {
        assert_eq!(CacheTtl::parse("1h"), Ok(CacheTtl::OneHour));
        assert_eq!(CacheTtl::parse(" 5M "), Ok(CacheTtl::FiveMinutes));
        assert!(CacheTtl::parse("2h").is_err());
        assert_eq!(serde_json::to_string(&CacheTtl::OneHour).unwrap(), "\"1h\"");

        let mut worker = config(false);
        CacheTtl::OneHour.apply(&mut worker);
        assert_eq!(
            worker.llm_config.generation_config.anthropic_cache_ttl,
            Some(arko_ai::AnthropicCacheTtl::OneHour)
        );
        CacheTtl::FiveMinutes.apply(&mut worker);
        assert_eq!(
            worker.llm_config.generation_config.anthropic_cache_ttl,
            None
        );

        // The choice is offered for Claude workers only.
        let temp = tempfile::tempdir().unwrap();
        let has_cache = |crew: &Arc<Crew>| {
            let tool = crew
                .lead_tools()
                .into_iter()
                .find(|tool| tool.definition().name == START_WORKER)
                .unwrap();
            tool.definition()
                .parameters
                .iter()
                .any(|param| param.name == CACHE_PARAM)
        };
        let (crew, _woken) = crew(temp.path(), NO_LIMIT);
        assert!(!has_cache(&crew));
        let (claude, _woken) = Crew::new(Setup {
            cache: Some(CacheTtl::FiveMinutes),
            ..setup(temp.path())
        });
        assert!(has_cache(&claude));

        fake(&claude, "w1", Status::Finished);
        claude.lock().workers[0].record.cache = Some(CacheTtl::OneHour);
        assert!(claude.list().contains("· 1h cache"), "{}", claude.list());
    }
}

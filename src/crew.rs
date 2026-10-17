//! Native subagent configuration and the session's worker status display.
//! arko owns the runs, mail, checkpoints, costs and isolated workspaces.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use arko_agents::{
    AgentConfig, AgentLLMConfig, ClaudeCodeSettings, HandoverConfig, TaskContext, Tool,
    ToolDefinition, ToolError, ToolParameterDefinition, ToolParameterType, ToolTypeDetails,
};
use arko_ai::AnthropicCacheTtl;
use arko_ai::prompt_cache::PromptCacheLifetime;
use arko_workflows::prelude::*;
use arko_workflows::subagents::{
    GitWorktreeProvider, StartRequest, SubagentEventKind, SubagentState, SubagentsConfig,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agents::NO_LIMIT;
use crate::remote;
use crate::ui::{self, Output};

pub use arko_workflows::subagents::{
    LIST_AGENTS_TOOL_NAME as LIST_AGENTS, MESSAGE_AGENT_TOOL_NAME as MESSAGE_AGENT,
    START_AGENT_TOOL_NAME as START_AGENT, STOP_AGENT_TOOL_NAME as STOP_AGENT,
    WAIT_FOR_AGENTS_TOOL_NAME as WAIT_FOR_AGENTS,
};
pub const LEAD_TOOL_NAMES: &[&str] = &[
    START_AGENT,
    MESSAGE_AGENT,
    WAIT_FOR_AGENTS,
    LIST_AGENTS,
    STOP_AGENT,
    "link_agents",
];
pub const WORKER_TOOL_NAMES: &[&str] = &[MESSAGE_AGENT, WAIT_FOR_AGENTS];
pub const CACHE_PARAM: &str = "cache";
pub const LEGACY_TOOL_NAMES: &[&str] = &[
    "start_worker",
    "message_worker",
    "wait_for_workers",
    "list_workers",
    "stop_worker",
    "message_lead",
    "wait_for_lead",
];
pub const LEGACY_NOTICE: &str = "This session uses the old worker tools. Its old workers and worktrees remain archived, but cannot be continued here. The old worker tools are unavailable. Continue the task yourself, or ask the user to start a new Arko session for native workers. /new resets this conversation but keeps its old tools.";

pub struct Setup {
    pub session_id: String,
    pub session_dir: PathBuf,
    pub output: Arc<Output>,
    pub max_running: u32,
    pub cache_1h: bool,
    /// The client's cache lifetime for the worker model and authentication.
    pub cache_lifetime: PromptCacheLifetime,
    pub feed: Option<remote::Feed>,
    /// Why new workers can't start on a model, when they can't: its
    /// provider's key was removed while Arko runs.
    pub refuse: Option<Refuse>,
}

/// See [`Setup::refuse`].
pub type Refuse = Arc<dyn Fn(&arko_ai::Model) -> Option<String> + Send + Sync>;

/// How long each worker of the session ran, by its handle (milliseconds),
/// in its folder.
const RUN_TIMES_FILE: &str = "worker-times.json";

/// How long each worker of the session in `dir` ran, as last kept (none:
/// a session from before they were kept).
fn load_run_times(dir: &std::path::Path) -> BTreeMap<String, u64> {
    std::fs::read_to_string(dir.join(RUN_TIMES_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// A worker, as `/workers` tells it.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerFacts {
    /// Its handle: r1, r2, ...
    pub id: String,
    /// Its run's state: running, finished, failed, stopped, ...
    pub state: String,
    pub task: String,
    /// The model it started on, when new workers start on another.
    pub other_model: Option<String>,
    /// How long its prompt cache lasts, when that is known.
    pub cache_lifetime: Option<Duration>,
    /// The worktree it works in, with its branch.
    pub worktree: Option<(PathBuf, Option<String>)>,
    /// Its report, or why it failed.
    pub report: Option<String>,
}

/// What a worker run starts with that the workers' model decides: the
/// model with its generation settings (the prompt cache's lifetime too),
/// Claude Code's settings, and the handover size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerModel {
    pub llm_config: AgentLLMConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code: Option<ClaudeCodeSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handover: Option<HandoverConfig>,
}

impl WorkerModel {
    /// What `config` (a worker's configuration) starts a run with.
    pub fn of(config: &AgentConfig) -> Self {
        Self {
            llm_config: config.llm_config.clone(),
            claude_code: config.claude_code.clone(),
            handover: config.handover.clone(),
        }
    }

    /// Whether Claude Code compacts the run's conversation at the handover
    /// size, instead of a handover (as `agents::compacts` says of a
    /// configuration).
    pub fn compacts(&self) -> bool {
        self.llm_config.model.provider == arko_ai::Provider::ClaudeCode
            && self
                .handover
                .as_ref()
                .is_some_and(|handover| handover.compact_in_claude_code)
    }

    fn apply(&self, config: &mut AgentConfig) {
        config.llm_config = self.llm_config.clone();
        config.claude_code = self.claude_code.clone();
        config.handover = self.handover.clone();
    }
}

/// The model new workers start on, with their default prompt cache (the
/// 1-hour one or not) and its lifetime, for `/workers`.
#[derive(Debug, Clone)]
pub struct NewWorkers {
    pub model: WorkerModel,
    pub cache_1h: bool,
    pub cache_lifetime: PromptCacheLifetime,
    /// OpenAI models run on the ChatGPT login (whose prompt cache lives
    /// differently).
    pub chatgpt: bool,
}

/// The workers' models: what new workers start on, and what each run
/// started on. A run keeps its own when it resumes (Arko restarted) or is
/// continued (`message_agent`, a follow-up), whatever new workers start
/// on by then.
///
/// arko continues a run from its saved task, but checks it against the
/// configuration the start hook gives the run again (as a workflow is
/// rebuilt from a checkpoint), and a difference stops the session from
/// resuming. So the hook records each run's configuration as the run
/// starts (in the session's folder, before the run is saved), and gives
/// the same again every time after.
struct WorkerModels {
    /// The record, in the session's folder.
    path: PathBuf,
    new: Option<NewWorkers>,
    runs: BTreeMap<Uuid, WorkerModel>,
    /// The crew is attached to its workflow, whose start hook calls are
    /// starts. Before (while a checkpoint is read back), a call for a run
    /// without a record is the check of a run saved before records
    /// existed: it gets the configuration it started with, as then.
    attached: bool,
}

/// The file in the session's folder that holds each worker run's model.
const WORKER_MODELS_FILE: &str = "worker-models.json";

impl WorkerModels {
    fn load(session_dir: &Path) -> Self {
        let path = session_dir.join(WORKER_MODELS_FILE);
        let runs = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            new: None,
            runs,
            attached: false,
        }
    }

    /// Writes the record, before the run it was made for starts.
    fn save(&self) -> Result<(), String> {
        let failed = |error: std::io::Error| {
            format!(
                "Arko could not record the worker's model in {} ({error})",
                self.path.display()
            )
        };
        let text = serde_json::to_string_pretty(&self.runs).map_err(|error| error.to_string())?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, text).map_err(failed)?;
        std::fs::rename(&temporary, &self.path).map_err(failed)
    }
}

/// The prompt cache a worker run asks for in `start_agent`'s `cache`, else
/// `default_1h`, on the models that take one: on `config`.
fn apply_cache(
    request: &StartRequest,
    config: &mut AgentConfig,
    default_1h: bool,
) -> Result<(), String> {
    let chosen = request.parameters.get(CACHE_PARAM).and_then(Value::as_str);
    let one_hour = match chosen {
        Some("1h") => true,
        Some("5m") => false,
        Some(other) => return Err(format!("cache is 5m or 1h, not {other}")),
        None => default_1h,
    };
    if crate::providers::takes_cache_ttl(&config.llm_config.model) {
        config.llm_config.generation_config.anthropic_cache_ttl =
            one_hour.then_some(AnthropicCacheTtl::OneHour);
    }
    Ok(())
}

/// A view of one workflow's subagents. It has no worker runtime or mailbox.
pub struct Crew {
    setup: Setup,
    models: Arc<Mutex<WorkerModels>>,
    /// The project's instructions new workers get in their first message,
    /// if any (see `project`).
    project: Arc<Mutex<Option<String>>>,
    workflow: Mutex<Weak<Workflow>>,
    lead: Mutex<Option<Uuid>>,
    wake: mpsc::UnboundedSender<()>,
    events: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Crew {
    pub fn new(setup: Setup) -> (Arc<Self>, mpsc::UnboundedReceiver<()>) {
        let (wake, woken) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                models: Arc::new(Mutex::new(WorkerModels::load(&setup.session_dir))),
                project: Arc::default(),
                setup,
                workflow: Mutex::new(Weak::new()),
                lead: Mutex::new(None),
                wake,
                events: Mutex::new(None),
            }),
            woken,
        )
    }

    pub fn config(&self) -> SubagentsConfig {
        let default_1h = self.setup.cache_1h;
        let models = self.models.clone();
        let refuse = self.setup.refuse.clone();
        let project = self.project.clone();
        let mut config = SubagentsConfig::default()
            .offer_blocking_tool(false)
            .questions_to_lead(true)
            .workspace_provider(Arc::new(GitWorktreeProvider::new(self.setup.session_dir.join("worktrees"))
                .branch_prefix(format!("arko/{}/", self.setup.session_id))))
            .hook(Arc::new(move |request: &StartRequest, config: &mut AgentConfig| {
                let mut models = models.lock().unwrap();
                // A run seen before: what it started with, always.
                if let Some(started) = models.runs.get(&request.run_id) {
                    started.apply(config);
                    return Ok(());
                }
                if !models.attached {
                    // A run saved before records existed, checked as a
                    // checkpoint is read back.
                    return apply_cache(request, config, default_1h);
                }
                // A start: on the model new workers start on, recorded
                // before the run starts.
                let mut one_hour = default_1h;
                if let Some(new) = &models.new {
                    new.model.apply(config);
                    one_hour = new.cache_1h;
                }
                // Not on a provider whose key was removed.
                if let Some(why) = refuse
                    .as_ref()
                    .and_then(|refuse| refuse(&config.llm_config.model))
                {
                    return Err(why);
                }
                apply_cache(request, config, one_hour)?;
                models.runs.insert(request.run_id, WorkerModel::of(config));
                models.save()
            }))
            // Asked as each worker starts: a run keeps its first message.
            .start_context(Arc::new(move |_: &StartRequest| project.lock().unwrap().clone()))
            .start_parameter(ToolParameterDefinition {
                name: CACHE_PARAM.into(),
                description: "1h or 5m: how long the worker's prompt cache lasts after its last step. Only Claude workers on an API key or OpenRouter take it: ChatGPT keeps its own 30-minute cache, and Claude Code an hour.".into(),
                param_type: ToolParameterType::String, required: false, is_path: false, flag: None, default_value: None,
            });
        config = if self.setup.max_running == NO_LIMIT {
            config.unlimited()
        } else {
            config.max_running(self.setup.max_running as usize)
        };
        // A worker's workspace may be outside the lead's only while outside
        // access is on, which can change while the session runs: the
        // `outside` hook refuses the others.
        config.workspaces_anywhere()
    }

    /// The model new workers start on from now on.
    pub fn set_new_workers(&self, new: NewWorkers) {
        self.models.lock().unwrap().new = Some(new);
    }

    /// The project's instructions new workers get in their first message
    /// from now on, if any.
    pub fn set_project(&self, text: Option<String>) {
        *self.project.lock().unwrap() = text;
    }

    /// The project's instructions new workers get, if any.
    #[cfg(test)]
    pub fn project(&self) -> Option<String> {
        self.project.lock().unwrap().clone()
    }

    /// What new workers start on, if it was set.
    pub fn new_workers(&self) -> Option<NewWorkers> {
        self.models.lock().unwrap().new.clone()
    }

    /// Before a workflow is built from a checkpoint: the start hook's calls
    /// until it is attached are checks of the saved runs.
    pub fn detach(&self) {
        self.models.lock().unwrap().attached = false;
    }

    /// Records the model of each worker run that has no record yet (runs
    /// of Arko before records existed), as it started: so that it keeps it
    /// once new workers start on another. Called as that changes. Returns
    /// how many workers the session has started.
    pub fn keep_started_models(&self) -> Result<usize, String> {
        let Some(workflow) = self.workflow() else {
            return Ok(0);
        };
        let mut models = self.models.lock().unwrap();
        let mut added = false;
        let started = workflow.subagents();
        for run in &started {
            if models.runs.contains_key(&run.run_id) {
                continue;
            }
            let Some(mut config) = workflow.context().agent_configs.get(&run.agent).cloned() else {
                continue;
            };
            let request = StartRequest {
                workflow_run_id: Uuid::nil(),
                run_id: run.run_id,
                handle: run.handle.clone(),
                agent: run.agent.clone(),
                caller_run_id: run.caller_run_id,
                caller_agent: String::new(),
                task: String::new(),
                parameters: run.parameters.clone(),
            };
            if apply_cache(&request, &mut config, self.setup.cache_1h).is_ok() {
                models.runs.insert(run.run_id, WorkerModel::of(&config));
                added = true;
            }
        }
        if added {
            models.save()?;
        }
        Ok(started.len())
    }

    /// The model worker run `run` started on, when Arko recorded it.
    pub fn run_model(&self, run: Uuid) -> Option<WorkerModel> {
        self.models.lock().unwrap().runs.get(&run).cloned()
    }

    /// The models the session's worker runs started on, as Arko recorded
    /// them, each once (a run keeps its own when it is continued or
    /// resumed).
    pub fn run_models(&self) -> Vec<arko_ai::Model> {
        let models = self.models.lock().unwrap();
        let mut found: Vec<arko_ai::Model> = Vec::new();
        for run in models.runs.values() {
            let model = &run.llm_config.model;
            if !found
                .iter()
                .any(|known| known.id == model.id && known.provider == model.provider)
            {
                found.push(model.clone());
            }
        }
        found
    }

    pub fn attach(self: &Arc<Self>, workflow: &Arc<Workflow>, lead: Option<Uuid>) {
        *self.workflow.lock().unwrap() = Arc::downgrade(workflow);
        self.models.lock().unwrap().attached = true;
        // A crash before the fresh lead's first tool can leave its workers
        // owned by the previous lead. Keep that owner until lead_run transfers it.
        *self.lead.lock().unwrap() = workflow
            .subagents()
            .last()
            .map(|run| run.caller_run_id)
            .or(lead);
        if let Some(old) = self.events.lock().unwrap().take() {
            old.abort();
        }
        let mut events = workflow.subscribe_subagents();
        if let Some(feed) = &self.setup.feed {
            // The workers of a resumed session, for the clients' lists, with
            // how long they ran before.
            let ran = load_run_times(&self.setup.session_dir);
            for run in workflow.subagents() {
                let state = match run.state {
                    SubagentState::Queued => remote::WorkerState::Queued,
                    SubagentState::Running => remote::WorkerState::Running,
                    SubagentState::Finished => remote::WorkerState::Finished,
                    SubagentState::Failed => remote::WorkerState::Failed,
                    SubagentState::Stopped | SubagentState::Interrupted => {
                        remote::WorkerState::Stopped
                    }
                    SubagentState::Closed if run.report.is_some() => remote::WorkerState::Finished,
                    SubagentState::Closed => remote::WorkerState::Stopped,
                };
                feed.worker_restored(
                    &run.handle,
                    &run.task,
                    state,
                    run.report.as_deref(),
                    run.since.timestamp_millis(),
                    ran.get(&run.handle).copied().unwrap_or(0),
                );
            }
        }
        if self.has_mail() {
            let _ = self.wake.send(());
        }
        let weak = Arc::downgrade(self);
        *self.events.lock().unwrap() = Some(tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(crew) = weak.upgrade() {
                            let _ = crew.wake.send(());
                        }
                        continue;
                    }
                    Err(_) => break,
                };
                let Some(crew) = weak.upgrade() else { break };
                let text = match event.kind.clone() {
                    SubagentEventKind::Queued => Some(format!("{} queued", event.handle)),
                    SubagentEventKind::Started => Some(format!("{} started", event.handle)),
                    SubagentEventKind::Resumed => Some(format!("{} resumed", event.handle)),
                    SubagentEventKind::Finished { summary } => Some(format!(
                        "{} reported. {}",
                        event.handle,
                        ui::cut(&summary, 180)
                    )),
                    SubagentEventKind::Failed { error } => {
                        Some(format!("{} failed ({error})", event.handle))
                    }
                    SubagentEventKind::Stopped { reason, .. } => {
                        Some(format!("{} stopped ({reason})", event.handle))
                    }
                    SubagentEventKind::WorkspaceRemoved => {
                        Some(format!("{} worktree removed", event.handle))
                    }
                    _ => None,
                };
                if let Some(text) = text {
                    // (The feed has the worker's summary.)
                    crew.setup
                        .output
                        .echo(&crew.setup.output.dim(&format!("  ↳ {text}")));
                }
                if let Some(feed) = &crew.setup.feed {
                    crew.tell_feed(feed, &event.handle, event.run_id, &event.kind);
                }
                let _ = crew.wake.send(());
            }
        }));
    }

    /// What became of a worker, for the feed: its summary and placeholder
    /// follow its runs.
    fn tell_feed(&self, feed: &remote::Feed, handle: &str, run: Uuid, kind: &SubagentEventKind) {
        use remote::WorkerChange;
        let change = match kind {
            SubagentEventKind::Queued => WorkerChange::Queued,
            SubagentEventKind::Started | SubagentEventKind::Resumed => WorkerChange::Started,
            SubagentEventKind::Finished { summary } => WorkerChange::Finished { report: summary },
            SubagentEventKind::Failed { error } => WorkerChange::Failed { error },
            SubagentEventKind::Stopped { reason, .. } => WorkerChange::Stopped { reason },
            SubagentEventKind::WorkspaceRemoved => {
                feed.notice(remote::Level::Info, &format!("{handle} worktree removed"));
                return;
            }
            _ => return,
        };
        let task = self
            .workflow()
            .and_then(|workflow| {
                workflow
                    .subagents()
                    .into_iter()
                    .find(|record| record.run_id == run)
            })
            .map(|record| record.task)
            .unwrap_or_default();
        let ended = matches!(
            change,
            WorkerChange::Finished { .. }
                | WorkerChange::Failed { .. }
                | WorkerChange::Stopped { .. }
        );
        feed.worker(handle, &task, change);
        if ended {
            self.save_run_times();
        }
    }

    /// Keeps how long each worker ran with the session, to go on from when
    /// it opens again.
    fn save_run_times(&self) {
        let Some(feed) = &self.setup.feed else {
            return;
        };
        let times: BTreeMap<String, u64> = feed.worker_run_times().into_iter().collect();
        let written = serde_json::to_vec_pretty(&times)
            .map_err(std::io::Error::other)
            .and_then(|text| {
                crate::session::write_whole(&self.setup.session_dir.join(RUN_TIMES_FILE), &text)
            });
        if let Err(error) = written {
            log::warn!("could not keep how long the workers ran ({error})");
        }
    }

    fn workflow(&self) -> Option<Arc<Workflow>> {
        self.workflow.lock().unwrap().upgrade()
    }

    /// Called before the lead's first tool execution, before it can act on
    /// workers transferred by /new. Continuations keep the same run id.
    pub fn lead_run(&self, run: Uuid) {
        let previous = self.lead.lock().unwrap().replace(run);
        if let Some(previous) = previous.filter(|old| *old != run)
            && let Some(workflow) = self.workflow()
        {
            workflow.transfer_subagents(previous, run);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.workflow()
            .is_none_or(|workflow| workflow.subagents().is_empty())
    }
    pub fn running(&self) -> Vec<String> {
        self.workflow()
            .map(|workflow| {
                workflow
                    .active_subagents()
                    .into_iter()
                    .map(|run| run.handle)
                    .collect()
            })
            .unwrap_or_default()
    }
    /// The user's message to worker `to` (`r1`), which goes as its lead's
    /// message does: a running worker reads it at its next step, and one
    /// that ended goes on with it. The lead gets a copy with its next news.
    /// Returns the worker's ID, or why the message can't go.
    pub fn message(&self, to: &str, text: &str) -> Result<String, String> {
        let workflow = self.workflow().ok_or("there are no workers")?;
        let id = workflow
            .subagents()
            .into_iter()
            .find(|run| run.handle.eq_ignore_ascii_case(to.trim()))
            .map(|run| run.handle)
            .ok_or_else(|| format!("there is no worker {}", to.trim()))?;
        workflow.message_subagent(
            &id,
            text,
            "The user sent you a message directly:",
            &format!("The user sent {id} a message directly (a copy for you):"),
        )?;
        Ok(id)
    }

    pub fn has_mail(&self) -> bool {
        let lead = *self.lead.lock().unwrap();
        lead.zip(self.workflow()).is_some_and(|(lead, workflow)| {
            workflow
                .pending_news(lead)
                .iter()
                .any(|item| item.kind.wakes())
        })
    }
    pub fn describe(&self) -> String {
        self.facts()
            .iter()
            .map(|worker| {
                let mut line = format!(
                    "{}  {}  {}",
                    worker.id,
                    worker.state,
                    ui::cut(&worker.task, 100)
                );
                if let Some(model) = &worker.other_model {
                    line.push_str(&format!("\n  runs {model}, the model it started on"));
                }
                if let Some(lifetime) = worker.cache_lifetime {
                    line.push_str(&format!(
                        "\n  prompt cache of {} minutes",
                        lifetime.as_secs() / 60
                    ));
                }
                if let Some((path, branch)) = &worker.worktree {
                    line.push_str(&format!(
                        "\n  {} ({})",
                        path.display(),
                        branch.as_deref().unwrap_or("worktree")
                    ));
                }
                if let Some(report) = &worker.report {
                    line.push_str(&format!("\n  {}", ui::cut(report, 240)));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The session's workers, as [`describe`](Self::describe) tells them.
    pub fn facts(&self) -> Vec<WorkerFacts> {
        let Some(workflow) = self.workflow() else {
            return Vec::new();
        };
        let new = self.new_workers();
        workflow
            .subagents()
            .into_iter()
            .map(|run| {
                // A run on another model than new workers' says it.
                let started = self.run_model(run.run_id);
                let other_model = match (&started, &new) {
                    (Some(started), Some(new))
                        if started.llm_config.model.id != new.model.llm_config.model.id =>
                    {
                        Some(crate::providers::model_name(&started.llm_config.model))
                    }
                    _ => None,
                };
                let mut lifetime = match (&started, &new) {
                    (Some(started), Some(new)) => arko_ai::prompt_cache::prompt_cache_lifetime(
                        &started.llm_config.model,
                        &started.llm_config.generation_config,
                        new.chatgpt,
                    ),
                    (None, Some(new)) => new.cache_lifetime,
                    _ => self.setup.cache_lifetime,
                };
                if workflow
                    .context()
                    .agent_configs
                    .get(&run.agent)
                    .is_some_and(|config| {
                        crate::providers::takes_cache_ttl(&config.llm_config.model)
                    })
                {
                    lifetime = match run.parameters.get(CACHE_PARAM).and_then(Value::as_str) {
                        Some("1h") => PromptCacheLifetime::Known(Duration::from_secs(3600)),
                        Some("5m") => PromptCacheLifetime::Known(Duration::from_secs(300)),
                        _ => lifetime,
                    };
                }
                WorkerFacts {
                    state: format!("{:?}", run.state).to_lowercase(),
                    task: run.task.clone(),
                    other_model,
                    cache_lifetime: match lifetime {
                        PromptCacheLifetime::Known(lifetime) => Some(lifetime),
                        _ => None,
                    },
                    worktree: run
                        .isolated
                        .as_ref()
                        .map(|worktree| (worktree.path.clone(), worktree.branch.clone())),
                    report: run.report.clone().or_else(|| run.error.clone()),
                    id: run.handle,
                }
            })
            .collect()
    }

    /// Stops the worker `id` (r1, r2, ...), when it is running, else says
    /// why not.
    pub fn stop(&self, id: &str, reason: &str) -> Result<(), String> {
        let workflow = self
            .workflow()
            .ok_or_else(|| "No worker is running.".to_string())?;
        if let Some(run) = workflow
            .active_subagents()
            .iter()
            .find(|run| run.handle == id)
            && workflow.stop_subagent(run.run_id, StopRequest::operator(reason))
        {
            return Ok(());
        }
        Err(if workflow.subagents().iter().any(|run| run.handle == id) {
            format!("{id} isn't running.")
        } else {
            format!("There is no worker {id}. /workers lists them.")
        })
    }

    pub fn stop_all(&self, reason: &str) -> usize {
        self.workflow().map_or(0, |workflow| {
            workflow
                .active_subagents()
                .iter()
                .filter(|run| workflow.stop_subagent(run.run_id, StopRequest::operator(reason)))
                .count()
        })
    }
    pub async fn shutdown(&self) {
        if let Some(workflow) = self.workflow() {
            // Stopping the workflow marks its unfinished subagents for resume.
            workflow.stop(StopRequest::operator("Arko closed"));
            let _ = tokio::time::timeout(Duration::from_secs(12), async {
                while !workflow.active_subagents().is_empty() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            // How long they ran, the runs just stopped too.
            self.save_run_times();
            // Mail publication schedules its own checkpoint. Await one here so
            // exiting immediately after a report preserves that report on resume.
            if let Err(error) = workflow.checkpoint().await {
                let text = format!("Could not save the final worker state ({error})");
                self.setup.output.echo(&self.setup.output.warn(&text));
                if let Some(feed) = &self.setup.feed {
                    feed.notice(remote::Level::Warn, &text);
                }
            }
        }
        if let Some(events) = self.events.lock().unwrap().take() {
            events.abort();
        }
    }
}

impl Drop for Crew {
    fn drop(&mut self) {
        if let Some(events) = self.events.get_mut().unwrap().take() {
            events.abort();
        }
    }
}

/// Saved legacy conversations still offer these names. Successful tool
/// results explain the migration instead of triggering error recovery.
#[derive(Debug)]
pub struct LegacyTool(pub &'static str);
#[async_trait]
impl Tool for LegacyTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.0.into(),
            description: LEGACY_NOTICE.into(),
            output_based_example: None,
            parameters: vec![],
            parallel_safe: true,
            tool_type_details: ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: Some(
                serde_json::json!({"type":"object", "properties":{}, "additionalProperties":true}),
            ),
        }
    }
    async fn execute(
        &self,
        _: &TaskContext<'_>,
        _: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        Ok(LEGACY_NOTICE.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::MAIN;
    use arko_agents::AgentLLMConfig;

    #[test]
    fn native_worker_options_preserve_cache_choices_and_queue_limits() {
        let temp = tempfile::tempdir().unwrap();
        let (crew, _) = Crew::new(Setup {
            session_id: "example".into(),
            session_dir: temp.path().into(),
            output: Arc::new(Output::new(None, true)),
            max_running: 2,
            cache_1h: true,
            cache_lifetime: PromptCacheLifetime::Known(Duration::from_secs(3600)),
            feed: None,
            refuse: None,
        });
        let native = crew.config();
        assert_eq!(native.max_running.unwrap().get(), 2);
        assert!(native.questions_to_lead);
        assert!(!native.offer_blocking_tool);
        let hook = native.hook.unwrap();
        let mut config = AgentConfig::builder(
            "worker".into(),
            "worker".into(),
            AgentLLMConfig {
                model: crate::agents::resolve_model("opus").unwrap(),
                generation_config: arko_ai::GenerationConfig::default(),
            },
        )
        .role_description("A test worker".into())
        .instructions("Carry out the test task.".into())
        .build()
        .unwrap();
        let mut request = StartRequest {
            workflow_run_id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            handle: "r1".into(),
            agent: "worker".into(),
            caller_run_id: Uuid::new_v4(),
            caller_agent: MAIN.into(),
            task: "Check it".into(),
            parameters: Default::default(),
        };
        hook.prepare(&request, &mut config).unwrap();
        assert_eq!(
            config.llm_config.generation_config.anthropic_cache_ttl,
            Some(AnthropicCacheTtl::OneHour)
        );
        request.parameters.insert(CACHE_PARAM.into(), "5m".into());
        hook.prepare(&request, &mut config).unwrap();
        assert_eq!(
            config.llm_config.generation_config.anthropic_cache_ttl,
            None
        );
        request.parameters.insert(CACHE_PARAM.into(), "1h".into());
        hook.prepare(&request, &mut config).unwrap();
        assert_eq!(
            config.llm_config.generation_config.anthropic_cache_ttl,
            Some(AnthropicCacheTtl::OneHour)
        );
        request.parameters.insert(CACHE_PARAM.into(), "2h".into());
        assert!(hook.prepare(&request, &mut config).is_err());
    }
}

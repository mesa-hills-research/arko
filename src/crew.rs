//! Native subagent configuration and the session's worker status display.
//! arko owns the runs, mail, checkpoints, costs and isolated workspaces.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use arko_agents::{
    AgentConfig, AgentLLMConfig, ClaudeCodeSettings, HandoverConfig, TaskContext, Tool,
    ToolDefinition, ToolError, ToolParameterDefinition, ToolParameterType, ToolTypeDetails,
};
use arko_ai::AnthropicCacheTtl;
use arko_ai::prompt_cache::PromptCacheLifetime;
use arko_workflows::prelude::*;
use arko_workflows::subagents::{
    GitWorktreeProvider, StartRequest, SubagentEventKind, SubagentHook, SubagentRecord,
    SubagentState, SubagentsConfig,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agents::NO_LIMIT;
use crate::chooser;
use crate::remote;
use crate::ui::{self, Output};

pub use arko_workflows::subagents::{
    LIST_AGENTS_TOOL_NAME as LIST_AGENTS, MESSAGE_AGENT_TOOL_NAME as MESSAGE_AGENT,
    START_AGENT_TOOL_NAME as START_AGENT, STOP_AGENT_TOOL_NAME as STOP_AGENT,
    WAIT_FOR_AGENTS_TOOL_NAME as WAIT_FOR_AGENTS,
};
/// The tool that lists the profiles and the models workers can run on, and
/// the one that saves a profile.
pub const WORKER_OPTIONS: &str = "worker_options";
pub const SAVE_WORKER_PROFILE: &str = "save_worker_profile";
pub const LEAD_TOOL_NAMES: &[&str] = &[
    START_AGENT,
    MESSAGE_AGENT,
    WAIT_FOR_AGENTS,
    LIST_AGENTS,
    STOP_AGENT,
    "link_agents",
    WORKER_OPTIONS,
    SAVE_WORKER_PROFILE,
];
pub const WORKER_TOOL_NAMES: &[&str] = &[MESSAGE_AGENT, WAIT_FOR_AGENTS];
/// The tools that let a worker start workers of its own, the framework's
/// own four. Only a worker started with `nested` has them, and the start
/// hook gives them with the run's setup. (It has `message_agent` and
/// `wait_for_agents` as every worker does. Choosing a setup with
/// `worker_options` and saving a profile stay with the main agent, which
/// tells the worker what to use.)
pub const NESTED_TOOL_NAMES: &[&str] = &[START_AGENT, LIST_AGENTS, STOP_AGENT, "link_agents"];
/// The most workers one worker has running or queued at once.
pub const MAX_OWN_WORKERS: usize = 8;
/// `start_agent`'s parameter that lets the worker start workers of its own.
pub const NESTED_PARAM: &str = "nested";
pub const CACHE_PARAM: &str = "cache";
/// `start_agent`'s parameters for choosing the worker's setup.
pub const PROFILE_PARAM: &str = "profile";
pub const MODEL_PARAM: &str = "model";
pub const EFFORT_PARAM: &str = "effort";
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
    /// provider has no key in this session (or its key was removed while
    /// Arko runs), or Claude Code isn't installed and signed in.
    pub refuse: Option<Refuse>,
}

/// See [`Setup::refuse`]. The text is written for the main agent.
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
#[non_exhaustive]
pub struct WorkerFacts {
    /// Its ID, such as r1 or r2.
    pub id: String,
    /// Its run's state, such as running, finished, failed or stopped.
    pub state: String,
    pub task: String,
    /// The model it started on, when Arko recorded it.
    pub model: Option<String>,
    /// The reasoning effort it started at, when it reasons and Arko recorded
    /// it.
    pub effort: Option<String>,
    /// The profile it started with, if any.
    pub profile: Option<String>,
    /// The worker that started it, when a worker did and not the main agent.
    pub parent: Option<String>,
    /// How far it is from the main agent, 1 for the main agent's workers.
    pub depth: u32,
    /// What its model calls billed and would cost at API list prices, in
    /// dollars, when the session counted them.
    pub cost: Option<(f64, f64)>,
    /// The model it started on, when new workers start on another. Apps can
    /// show it. `/workers` says what every worker runs, so it doesn't.
    pub other_model: Option<String>,
    /// How long its prompt cache lasts, when that is known.
    pub cache_lifetime: Option<Duration>,
    /// The worktree it works in, with its branch.
    pub worktree: Option<(PathBuf, Option<String>)>,
    /// Its report, or why it failed.
    pub report: Option<String>,
}

/// What a worker run starts with that its setup decides: the model with
/// its generation settings (the prompt cache's lifetime too), Claude Code's
/// settings, the handover size, and what its profile adds. The start hook
/// works it out from the call, the profile and the session's settings,
/// records it with the run, and gives the same again every time the run is
/// resumed or continued.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSetup {
    pub llm_config: AgentLLMConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code: Option<ClaudeCodeSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handover: Option<HandoverConfig>,
    /// Text for the top of the run's first message, after the project's
    /// instructions: the profile's instructions, and the extra rules for a
    /// small model that the registered worker didn't get. It is copied here
    /// when the run starts, so an edit or the deletion of the profile's file
    /// changes nothing for a run that exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_added: Option<String>,
    /// Tools the run is offered besides the registered worker's (the start
    /// tools, for a worker that nests).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_added: Vec<String>,
    /// The profile the run started with, for the lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The reasoning effort the run started at, for the lists (none for a
    /// model that doesn't reason, and for a run recorded before it was kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<crate::agents::Effort>,
}

impl WorkerSetup {
    /// What `config` (a worker's configuration) starts a run with.
    pub fn of(config: &AgentConfig) -> Self {
        Self {
            llm_config: config.llm_config.clone(),
            claude_code: config.claude_code.clone(),
            handover: config.handover.clone(),
            context_added: None,
            tools_added: Vec::new(),
            profile: None,
            effort: None,
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

    /// Gives `config` (the registered worker's, or the configuration another
    /// run was saved with) this run's setup. The result depends on the
    /// setup only, so the same run gets the same configuration every time.
    pub(crate) fn apply(&self, config: &mut AgentConfig) {
        config.llm_config = self.llm_config.clone();
        config.claude_code = self.claude_code.clone();
        config.handover = self.handover.clone();
        // The start tools are the run's own: none a configuration of
        // another run had, and the ones this run's setup adds.
        let tools = &mut config.tool_config.available_tool_names;
        tools.retain(|name| !NESTED_TOOL_NAMES.contains(&name.as_str()));
        for name in &self.tools_added {
            if !tools.contains(name) {
                tools.push(name.clone());
            }
        }
    }
}

/// The model new workers start on and its prompt cache's lifetime, for
/// `/workers`, and what the start hook chooses each worker's setup from.
#[derive(Debug, Clone)]
pub struct NewWorkers {
    pub model: WorkerSetup,
    pub cache_lifetime: PromptCacheLifetime,
    /// What the start hook chooses a worker's setup from.
    pub basis: std::sync::Arc<chooser::Basis>,
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
struct WorkerRecords {
    /// The record, in the session's folder.
    path: PathBuf,
    new: Option<NewWorkers>,
    runs: BTreeMap<Uuid, WorkerSetup>,
    /// The crew is attached to its workflow, whose start hook calls are
    /// starts. Before (while a checkpoint is read back), a call for a run
    /// without a record is the check of a run saved before records
    /// existed: it gets the configuration it started with, as then.
    attached: bool,
}

/// The file in the session's folder that holds each worker run's model.
const WORKER_MODELS_FILE: &str = "worker-models.json";

impl WorkerRecords {
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

/// The start hook. A run seen before gets the setup it started with, always.
/// A start works its setup out ([`chooser::choose`]) and records it before
/// the run is saved. A run saved before records existed, checked as a
/// checkpoint is read back, gets the registered worker's configuration with
/// its prompt cache.
fn prepare(
    models: &Mutex<WorkerRecords>,
    workflow: &Mutex<Weak<Workflow>>,
    flight: &Mutex<HashMap<Uuid, (Uuid, Instant)>>,
    feed: Option<&remote::Feed>,
    refuse: Option<&Refuse>,
    default_1h: bool,
    request: &StartRequest,
    config: &mut AgentConfig,
) -> Result<(), String> {
    // (The lock is held briefly: choosing may read files, and Claude Code's
    // login check runs a command.)
    let (started, attached, new) = {
        let models = models.lock().unwrap();
        (
            models.runs.get(&request.run_id).cloned(),
            models.attached,
            models.new.clone(),
        )
    };
    if let Some(started) = started {
        started.apply(config);
        return Ok(());
    }
    if !attached {
        return apply_cache(request, config, default_1h);
    }
    let setup = match new {
        Some(new) => {
            let runs = workflow
                .lock()
                .unwrap()
                .upgrade()
                .map(|workflow| workflow.subagents())
                .unwrap_or_default();
            // Counting and keeping a place for this start are one step, so
            // starts made together can't each find a free place.
            let (family, parent) = {
                let mut flight = flight.lock().unwrap();
                flight.retain(|run, (_, since)| {
                    since.elapsed() < FLIGHT_LIMIT && !runs.iter().any(|other| other.run_id == *run)
                });
                let held: Vec<(Uuid, Uuid)> = flight
                    .iter()
                    .map(|(run, (caller, _))| (*run, *caller))
                    .collect();
                let found = family_of(&runs, &held, request);
                if found.1.is_some() {
                    flight.insert(request.run_id, (request.caller_run_id, Instant::now()));
                }
                found
            };
            let setup = chooser::choose_in(&new.basis, request, refuse, family);
            let setup = match setup {
                Ok(setup) => setup,
                Err(why) => {
                    flight.lock().unwrap().remove(&request.run_id);
                    return Err(why);
                }
            };
            // The clients put its line where its parent is.
            if let (Some(feed), Some(parent)) = (feed, parent) {
                feed.worker_under(&request.handle, &parent);
            }
            setup
        }
        None => {
            // Not told what new workers start on: the registered worker.
            if let Some(why) = refuse.and_then(|refuse| refuse(&config.llm_config.model)) {
                return Err(why);
            }
            apply_cache(request, config, default_1h)?;
            WorkerSetup::of(config)
        }
    };
    setup.apply(config);
    let mut models = models.lock().unwrap();
    models.runs.insert(request.run_id, setup);
    models.save()
}

/// How long a start that passed the hook keeps its place while its record
/// isn't there. A start that fails after the hook leaves no record, so its
/// place is given back by the time.
const FLIGHT_LIMIT: Duration = Duration::from_secs(30);

/// Where in the tree of workers the start `request` happens, and the handle
/// of its starter when a worker started it: how deep the new worker is, and
/// how many workers its starter has running or queued. `runs` are the
/// session's workers, and `held` the starts (their run and their starter)
/// that passed the hook and have no record yet, which count as running.
/// (The main agent is no worker, so its starts are at the first level.)
pub(crate) fn family_of(
    runs: &[SubagentRecord],
    held: &[(Uuid, Uuid)],
    request: &StartRequest,
) -> (chooser::Family, Option<String>) {
    let Some(caller) = runs.iter().find(|run| run.run_id == request.caller_run_id) else {
        return (chooser::Family::MAIN, None);
    };
    let running = runs
        .iter()
        .filter(|run| run.caller_run_id == caller.run_id && run.state.is_active())
        .count();
    let starting = held
        .iter()
        .filter(|(run, starter)| {
            *starter == caller.run_id
                && *run != request.run_id
                && !runs.iter().any(|other| other.run_id == *run)
        })
        .count();
    (
        chooser::Family {
            depth: caller.depth + 1,
            live: running + starting,
        },
        Some(caller.handle.clone()),
    )
}

/// The handle of the worker among `runs` that started the worker `run`,
/// when a worker did (and not the main agent).
pub(crate) fn started_by(
    runs: &[arko_workflows::subagents::SubagentRecord],
    run: Uuid,
) -> Option<String> {
    let caller = runs.iter().find(|other| other.run_id == run)?.caller_run_id;
    runs.iter()
        .find(|other| other.run_id == caller)
        .map(|parent| parent.handle.clone())
}

/// The handles of the workers among `runs` that were started by the run
/// `root`, or by one of those, in the order they started.
pub(crate) fn below(runs: &[arko_workflows::subagents::SubagentRecord], root: Uuid) -> Vec<String> {
    let mut family = vec![root];
    let mut found = Vec::new();
    for run in runs {
        // (A worker starts after the one that starts it, so one pass finds
        // them all.)
        if family.contains(&run.caller_run_id) {
            family.push(run.run_id);
            found.push(run.handle.clone());
        }
    }
    found
}

/// How many of the `active` workers stop when the stops of the workers
/// `accepted` take effect: those, and the workers that they started, which
/// stop with them and report no stop of their own.
fn stopped_by(active: &[SubagentRecord], accepted: &[Uuid]) -> usize {
    let mut stopped: Vec<Uuid> = Vec::new();
    for root in accepted {
        let mut family = vec![*root];
        for run in active {
            if family.contains(&run.caller_run_id) {
                family.push(run.run_id);
            }
        }
        for run in active.iter().filter(|run| family.contains(&run.run_id)) {
            if !stopped.contains(&run.run_id) {
                stopped.push(run.run_id);
            }
        }
    }
    stopped.len()
}

/// The start hook: a worker's setup as it starts, and which workers may
/// start workers.
struct StartHook {
    models: Arc<Mutex<WorkerRecords>>,
    workflow: Arc<Mutex<Weak<Workflow>>>,
    /// The starts that passed the hook and have no record yet, by run, with
    /// their starter and since when.
    flight: Mutex<HashMap<Uuid, (Uuid, Instant)>>,
    feed: Option<remote::Feed>,
    refuse: Option<Refuse>,
    default_1h: bool,
}

impl SubagentHook for StartHook {
    fn prepare(&self, request: &StartRequest, config: &mut AgentConfig) -> Result<(), String> {
        prepare(
            &self.models,
            &self.workflow,
            &self.flight,
            self.feed.as_ref(),
            self.refuse.as_ref(),
            self.default_1h,
            request,
            config,
        )
    }

    /// Only a worker whose setup, as recorded when it started, gives it the
    /// start tools, so the answer is the same each time for a run, also when
    /// the session resumes and whatever the settings are by then. A run
    /// recorded before nesting, or not at all, starts no workers.
    fn offers_start_tools(&self, record: &SubagentRecord) -> bool {
        self.models
            .lock()
            .unwrap()
            .runs
            .get(&record.run_id)
            .is_some_and(|setup| setup.tools_added.iter().any(|tool| tool == START_AGENT))
    }
}

/// The text a run's first message opens with: the project's instructions,
/// then what the run's setup adds (its profile's instructions).
fn first_message_context(
    project: &Mutex<Option<String>>,
    records: &Mutex<WorkerRecords>,
    request: &StartRequest,
) -> Option<String> {
    let project = project.lock().unwrap().clone();
    let added = records
        .lock()
        .unwrap()
        .runs
        .get(&request.run_id)
        .and_then(|setup| setup.context_added.clone());
    match (project, added) {
        (Some(project), Some(added)) => Some(format!("{}\n\n{added}", project.trim_end())),
        (project, added) => project.or(added),
    }
}

/// The parameters of the host's own that `start_agent` takes: the prompt
/// cache, the worker's profile, model and effort, and whether it may start
/// workers of its own.
fn start_parameters() -> Vec<ToolParameterDefinition> {
    let text = |name: &str, description: &str| ToolParameterDefinition {
        name: name.into(),
        description: description.into(),
        param_type: ToolParameterType::String,
        required: false,
        is_path: false,
        flag: None,
        default_value: None,
    };
    vec![
        text(
            CACHE_PARAM,
            "1h or 5m, how long the worker's prompt cache lasts after its last step. Only Claude workers on an API key or OpenRouter take it. ChatGPT keeps its own 30-minute cache, and Claude Code an hour.",
        ),
        text(
            PROFILE_PARAM,
            "A worker profile by name. `default` is the session's worker model.",
        ),
        text(
            MODEL_PARAM,
            "A model such as opus, sonnet, haiku, sol, luna, gemini, cc:opus or openrouter:<slug>. Overrides the profile's.",
        ),
        text(
            EFFORT_PARAM,
            "low, medium, high, xhigh or max, as the model takes it. Overrides the profile's.",
        ),
        ToolParameterDefinition {
            name: NESTED_PARAM.into(),
            description: "true lets the worker start workers of its own.".into(),
            param_type: ToolParameterType::Bool,
            required: false,
            is_path: false,
            flag: None,
            default_value: None,
        },
    ]
}

/// A worker's lines for `/workers`.
pub(crate) fn describe_worker(worker: &WorkerFacts) -> String {
    let mut line = format!(
        "{}  {}  {}",
        worker.id,
        worker.state,
        ui::cut(&worker.task, 100)
    );
    if let Some(parent) = &worker.parent {
        line.push_str(&format!("\n  started by {parent}"));
    }
    if let Some(model) = &worker.model {
        line.push_str(&format!("\n  runs {model}"));
        if let Some(effort) = &worker.effort {
            line.push_str(&format!(" at {effort} effort"));
        }
        if let Some(profile) = &worker.profile {
            line.push_str(&format!(", profile {profile}"));
        }
    }
    // What it has cost, once it has cost something.
    if let Some((billed, list)) = worker.cost.filter(|(_, list)| *list >= 0.005) {
        if list > billed + 0.005 {
            line.push_str(&format!(
                "\n  ≈ ${list:.2} at API prices, ${billed:.2} billed"
            ));
        } else {
            line.push_str(&format!("\n  cost ${billed:.2}"));
        }
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
}

/// A view of one workflow's subagents. It has no worker runtime or mailbox.
pub struct Crew {
    setup: Setup,
    models: Arc<Mutex<WorkerRecords>>,
    /// The project's instructions new workers get in their first message,
    /// if any (see `project`).
    project: Arc<Mutex<Option<String>>>,
    workflow: Arc<Mutex<Weak<Workflow>>>,
    lead: Mutex<Option<Uuid>>,
    wake: mpsc::UnboundedSender<()>,
    events: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Crew {
    pub fn new(setup: Setup) -> (Arc<Self>, mpsc::UnboundedReceiver<()>) {
        let (wake, woken) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                models: Arc::new(Mutex::new(WorkerRecords::load(&setup.session_dir))),
                project: Arc::default(),
                setup,
                workflow: Arc::default(),
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
        let records = self.models.clone();
        let workflow = self.workflow.clone();
        let feed = self.setup.feed.clone();
        let refuse = self.setup.refuse.clone();
        let project = self.project.clone();
        let mut config = SubagentsConfig::default()
            .offer_blocking_tool(false)
            // The deepest a session can allow. How deep workers go is the
            // session's `worker_depth`, which the start hook holds each
            // start to.
            .max_depth(crate::agents::MAX_WORKER_DEPTH)
            .questions_to_lead(true)
            .workspace_provider(Arc::new(
                GitWorktreeProvider::new(self.setup.session_dir.join("worktrees"))
                    .branch_prefix(format!("arko/{}/", self.setup.session_id)),
            ))
            .hook(Arc::new(StartHook {
                models,
                workflow,
                flight: Mutex::default(),
                feed,
                refuse,
                default_1h,
            }))
            // Asked as each worker starts: a run keeps its first message.
            .start_context(Arc::new(move |request: &StartRequest| {
                first_message_context(&project, &records, request)
            }));
        for parameter in start_parameters() {
            config = config.start_parameter(parameter);
        }
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

    /// Tells the user something, in the terminal and on the page.
    pub fn notice(&self, level: remote::Level, text: &str) {
        let out = &self.setup.output;
        out.echo(&match level {
            remote::Level::Info => out.dim(text),
            remote::Level::Warn => out.warn(text),
            remote::Level::Error => out.error(text),
        });
        if let Some(feed) = &self.setup.feed {
            feed.notice(level, text);
        }
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
                models.runs.insert(run.run_id, WorkerSetup::of(&config));
                added = true;
            }
        }
        if added {
            models.save()?;
        }
        Ok(started.len())
    }

    /// The model worker run `run` started on, when Arko recorded it.
    pub fn run_model(&self, run: Uuid) -> Option<WorkerSetup> {
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
        // owned by the previous lead. Keep that owner until lead_run transfers
        // it. (The lead is the main agent's: the owner of the latest worker
        // that no worker started.)
        let started = workflow.subagents();
        *self.lead.lock().unwrap() = started
            .iter()
            .rev()
            .find(|run| {
                !started
                    .iter()
                    .any(|other| other.run_id == run.caller_run_id)
            })
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
            let runs = workflow.subagents();
            for run in &runs {
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
                // What it started with and what it cost before.
                feed.worker_info(&run.handle, self.worker_info(workflow, run, &runs, true));
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
                // (A worker a worker started says whose it is.)
                let name = crew.named(&event.handle, event.run_id);
                let text = match event.kind.clone() {
                    SubagentEventKind::Queued => Some(format!("{name} queued")),
                    SubagentEventKind::Started => Some(format!("{name} started")),
                    SubagentEventKind::Resumed => Some(format!("{name} resumed")),
                    SubagentEventKind::Finished { summary } => {
                        Some(format!("{name} reported. {}", ui::cut(&summary, 180)))
                    }
                    SubagentEventKind::Failed { error } => Some(format!("{name} failed ({error})")),
                    SubagentEventKind::Stopped { reason, .. } => {
                        Some(format!("{name} stopped ({reason})"))
                    }
                    SubagentEventKind::WorkspaceRemoved => Some(format!("{name} worktree removed")),
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

    /// The worker `handle` (run `run`) as a notice names it: `r2`, or `r2
    /// (under r1)` when a worker started it.
    fn named(&self, handle: &str, run: Uuid) -> String {
        let Some(workflow) = self.workflow() else {
            return handle.to_string();
        };
        let runs = workflow.subagents();
        match started_by(&runs, run) {
            Some(parent) => format!("{handle} (under {parent})"),
            None => handle.to_string(),
        }
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
                feed.notice(
                    remote::Level::Info,
                    &format!("{} worktree removed", self.named(handle, run)),
                );
                return;
            }
            _ => return,
        };
        let workflow = self.workflow();
        let runs = workflow
            .as_ref()
            .map(|workflow| workflow.subagents())
            .unwrap_or_default();
        let record = runs.iter().find(|record| record.run_id == run);
        if let (Some(workflow), Some(record)) = (&workflow, record) {
            feed.worker_info(handle, self.worker_info(workflow, record, &runs, false));
        }
        let task = record.map(|record| record.task.clone()).unwrap_or_default();
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

    /// What a worker's summary says of it: the setup it started with, its
    /// place in the family, and (`with_costs`) what its saved runs cost, for
    /// a session that resumes. The costs of a worker that starts add up from
    /// its events.
    ///
    /// `runs` are all the session's workers, which the caller reads once, so
    /// a long list isn't read again for each of its workers.
    fn worker_info(
        &self,
        workflow: &Workflow,
        run: &arko_workflows::subagents::SubagentRecord,
        runs: &[arko_workflows::subagents::SubagentRecord],
        with_costs: bool,
    ) -> remote::WorkerInfo {
        let setup = self.run_model(run.run_id);
        let parent = started_by(runs, run.run_id);
        let costs = with_costs.then(|| {
            let results = workflow.context().agent_results.read();
            results
                .iter()
                .filter(|result| result.agent_run_id == run.run_id)
                .flat_map(|result| result.task.usage_history.iter())
                .fold((0.0, 0.0), |(billed, list), usage| {
                    let paid = usage.billed();
                    (
                        billed + paid,
                        list + crate::providers::at_api_prices(usage, paid),
                    )
                })
        });
        let mut info = remote::WorkerInfo::new();
        info.model = setup
            .as_ref()
            .map(|setup| crate::providers::model_name(&setup.llm_config.model));
        info.effort = setup
            .as_ref()
            .and_then(|setup| setup.effort)
            .map(|effort| effort.to_string());
        info.profile = setup.and_then(|setup| setup.profile);
        info.parent = parent;
        info.depth = run.depth;
        info.costs = costs;
        info
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
    /// that ended goes on with it. The worker that started it (the main
    /// agent, for most) gets a copy with its next news. Returns the
    /// worker's ID, or why the message can't go.
    pub fn message(&self, to: &str, text: &str) -> Result<String, String> {
        let workflow = self.workflow().ok_or("there are no workers")?;
        let runs = workflow.subagents();
        let run = runs
            .iter()
            .find(|run| run.handle.eq_ignore_ascii_case(to.trim()))
            .ok_or_else(|| format!("there is no worker {}", to.trim()))?;
        let id = run.handle.clone();
        // A worker that started it reads the copy as news about its own
        // worker, and is told so.
        let copy = if runs.iter().any(|lead| lead.run_id == run.caller_run_id) {
            format!(
                "The user sent {id} a message directly (a copy for you, since you started {id}):"
            )
        } else {
            format!("The user sent {id} a message directly (a copy for you):")
        };
        workflow.message_subagent(&id, text, "The user sent you a message directly:", &copy)?;
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
            .map(describe_worker)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The session's workers, as [`describe`](Self::describe) tells them.
    pub fn facts(&self) -> Vec<WorkerFacts> {
        let Some(workflow) = self.workflow() else {
            return Vec::new();
        };
        let new = self.new_workers();
        let runs = workflow.subagents();
        let facts: Vec<WorkerFacts> = runs
            .clone()
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
                    (Some(started), Some(new)) => {
                        // (The ChatGPT login is the session's way to OpenAI
                        // models, for every worker on one.)
                        let model = &started.llm_config.model;
                        let chatgpt = model.provider == arko_ai::Provider::OpenAI
                            && new.basis.providers.openai_auth
                                != Some(crate::providers::OpenAiAuth::ApiKey);
                        arko_ai::prompt_cache::prompt_cache_lifetime(
                            model,
                            &started.llm_config.generation_config,
                            chatgpt,
                        )
                    }
                    (None, Some(new)) => new.cache_lifetime,
                    _ => self.setup.cache_lifetime,
                };
                // A run Arko didn't record: the cache it asked for, on the
                // registered worker's model.
                if started.is_none()
                    && workflow
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
                    model: started
                        .as_ref()
                        .map(|started| crate::providers::model_name(&started.llm_config.model)),
                    effort: started
                        .as_ref()
                        .and_then(|started| started.effort)
                        .map(|effort| effort.to_string()),
                    profile: started.as_ref().and_then(|started| started.profile.clone()),
                    parent: started_by(&runs, run.run_id),
                    depth: run.depth,
                    cost: self
                        .setup
                        .feed
                        .as_ref()
                        .and_then(|feed| feed.worker_summary(&run.handle))
                        .map(|summary| (summary.cost_billed, summary.cost_list)),
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
            .collect();
        // A worker's workers follow it.
        let order = remote::family_order_of(
            facts
                .iter()
                .map(|fact| (fact.id.as_str(), fact.parent.as_deref())),
        );
        let mut facts: Vec<Option<WorkerFacts>> = facts.into_iter().map(Some).collect();
        order
            .into_iter()
            .filter_map(|at| facts[at].take())
            .collect()
    }

    /// Stops the worker `id` (r1, r2, ...), when it is running, else says
    /// why not. The workers it started stop with it, and theirs in turn:
    /// their IDs are returned.
    pub fn stop(&self, id: &str, reason: &str) -> Result<Vec<String>, String> {
        let workflow = self
            .workflow()
            .ok_or_else(|| "No worker is running.".to_string())?;
        let active = workflow.active_subagents();
        if let Some(run) = active.iter().find(|run| run.handle == id)
            && workflow.stop_subagent(run.run_id, StopRequest::operator(reason))
        {
            return Ok(below(&active, run.run_id));
        }
        Err(if workflow.subagents().iter().any(|run| run.handle == id) {
            format!("{id} isn't running.")
        } else {
            format!("There is no worker {id}. /workers lists them.")
        })
    }

    /// Stops every worker that runs or waits to run, those started by
    /// workers too. Returns how many this stopped, which leaves out a worker
    /// already stopping.
    pub fn stop_all(&self, reason: &str) -> usize {
        self.workflow().map_or(0, |workflow| {
            let active = workflow.active_subagents();
            let accepted: Vec<Uuid> = active
                .iter()
                .filter(|run| workflow.stop_subagent(run.run_id, StopRequest::operator(reason)))
                .map(|run| run.run_id)
                .collect();
            stopped_by(&active, &accepted)
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

    /// A run is offered the start tools only when its own setup adds them.
    /// Applying a setup takes them off a configuration that carries them
    /// (another run's, saved with them), and puts back only the ones the
    /// setup lists.
    #[test]
    fn applying_a_setup_leaves_the_start_tools_only_when_it_adds_them() {
        let worker = luna();
        let config =
            crate::agents::worker_config(&crate::agents::AgentSettings::for_test(&worker, &worker))
                .unwrap();
        let setup = WorkerSetup::of(&config);
        let mut carrying = config.clone();
        for name in NESTED_TOOL_NAMES {
            carrying
                .tool_config
                .available_tool_names
                .push(name.to_string());
        }
        setup.apply(&mut carrying);
        let tools = &carrying.tool_config.available_tool_names;
        for name in NESTED_TOOL_NAMES {
            assert!(!tools.iter().any(|tool| tool == name), "{name} stays");
        }
        for name in WORKER_TOOL_NAMES {
            assert!(tools.iter().any(|tool| tool == name), "{name} goes");
        }
        // A setup that adds some gives those, once, and no others.
        let mut adding = setup.clone();
        adding.tools_added = vec![START_AGENT.to_string(), START_AGENT.to_string()];
        let mut given = carrying.clone();
        adding.apply(&mut given);
        let tools = &given.tool_config.available_tool_names;
        assert_eq!(tools.iter().filter(|tool| *tool == START_AGENT).count(), 1);
        assert!(!tools.iter().any(|tool| tool == LIST_AGENTS));
    }

    /// A crew over `session_dir` whose workers run `worker` unless told
    /// otherwise, attached as if its workflow were open. Profiles are in
    /// `root`.
    fn crew_for(
        session_dir: &Path,
        root: &Path,
        worker: &crate::agents::ModelSettings,
    ) -> (Arc<Crew>, AgentConfig) {
        let (crew, _) = Crew::new(Setup {
            session_id: "example".into(),
            session_dir: session_dir.into(),
            output: Arc::new(Output::new(None, true)),
            max_running: 4,
            cache_1h: false,
            cache_lifetime: PromptCacheLifetime::Known(Duration::from_secs(300)),
            feed: None,
            refuse: None,
        });
        let registered =
            crate::agents::worker_config(&crate::agents::AgentSettings::for_test(worker, worker))
                .unwrap();
        crew.set_new_workers(NewWorkers {
            model: WorkerSetup::of(&registered),
            cache_lifetime: PromptCacheLifetime::Known(Duration::from_secs(300)),
            basis: Arc::new(chooser::Basis {
                roots: crate::profiles::Roots::new(root).session(session_dir),
                worker: worker.clone(),
                providers: crate::providers::Setup::new(None),
                thinking_text: true,
                cc_compact: true,
                handover_at: 200_000,
                choice: crate::agents::ModelChoice::Agent,
                worker_depth: 2,
                extra_rules: crate::agents::ExtraRules::Off,
                rules_text: None,
            }),
        });
        crew.models.lock().unwrap().attached = true;
        (crew, registered)
    }

    fn start(handle: &str, parameters: &[(&str, &str)]) -> StartRequest {
        StartRequest {
            workflow_run_id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            handle: handle.into(),
            agent: "worker".into(),
            caller_run_id: Uuid::new_v4(),
            caller_agent: MAIN.into(),
            task: "Check it".into(),
            parameters: parameters
                .iter()
                .map(|(name, value)| (name.to_string(), Value::String(value.to_string())))
                .collect(),
        }
    }

    /// A record of a worker `handle`, started by the run `caller`.
    fn record(handle: &str, caller: Uuid) -> arko_workflows::subagents::SubagentRecord {
        let now = chrono::Utc::now();
        arko_workflows::subagents::SubagentRecord {
            run_id: Uuid::new_v4(),
            handle: handle.to_string(),
            agent: "worker".into(),
            caller_run_id: caller,
            caller_agent: "main".into(),
            depth: 1,
            task: String::new(),
            workspace: PathBuf::new(),
            isolated: None,
            parameters: Default::default(),
            peers: Vec::new(),
            all_peers: false,
            state: SubagentState::Running,
            created: now,
            since: now,
            report: None,
            error: None,
            pending: None,
            stopped_by_caller: None,
            stop_requested: None,
            asking: false,
            detached: false,
        }
    }

    fn luna() -> crate::agents::ModelSettings {
        let model = crate::agents::resolve_model("luna").unwrap();
        crate::agents::ModelSettings {
            model: model.id.clone(),
            effort: crate::agents::Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: Some(crate::agents::default_handover(&model, None)),
        }
    }

    fn judge(root: &Path, text: &str) {
        let folder = root.join("profiles");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("judge.md"), text).unwrap();
    }

    /// The configuration a hook gives `request`, from `base`, as JSON.
    fn prepared(crew: &Crew, request: &StartRequest, base: &AgentConfig) -> serde_json::Value {
        let hook = crew.config().hook.unwrap();
        let mut config = base.clone();
        hook.prepare(request, &mut config).unwrap();
        serde_json::to_value(&config).unwrap()
    }

    /// A worker that starts workers keeps that, and the paragraph about them,
    /// whatever happens to its profile or to the settings, and is the only
    /// run the hook says may start workers. This is what lets a saved
    /// session with a tree of workers resume.
    #[test]
    fn a_recorded_nesting_run_gets_the_same_configuration_after_its_profile_changes() {
        let temp = tempfile::tempdir().unwrap();
        let (session, root) = (temp.path().join("session"), temp.path().join("arko"));
        std::fs::create_dir_all(&session).unwrap();
        judge(
            &root,
            "---\ndescription: Judges\nnested: true\n---\nJudge it in parts.\n",
        );
        let (crew, registered) = crew_for(&session, &root, &luna());
        let nests = start("r1", &[("profile", "judge")]);
        let plain = start("r2", &[]);
        let before = prepared(&crew, &nests, &registered);
        let plain_before = prepared(&crew, &plain, &registered);
        let tools = |config: &serde_json::Value| -> Vec<String> {
            config["tool_config"]["available_tool_names"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap().to_string())
                .collect()
        };
        for tool in NESTED_TOOL_NAMES {
            assert!(tools(&before).iter().any(|name| name == tool), "{tool}");
            assert!(
                !tools(&plain_before).iter().any(|name| name == tool),
                "{tool}"
            );
        }
        let ask = crew.config().start_context.unwrap();
        assert!(ask(&nests).unwrap().contains("# Workers of your own"));
        assert_eq!(ask(&plain), None);
        // Only the run that nests may start workers.
        let hook = crew.config().hook.unwrap();
        let record = |request: &StartRequest| {
            let mut record = record(&request.handle, request.caller_run_id);
            record.run_id = request.run_id;
            record
        };
        assert!(hook.offers_start_tools(&record(&nests)));
        assert!(!hook.offers_start_tools(&record(&plain)));
        assert!(!hook.offers_start_tools(&record(&start("r9", &[]))));

        // The profile is edited and deleted, a Claude Code login changes
        // nothing: the same configuration, from the registered worker and
        // from another run's saved one, and the same answer.
        judge(&root, "---\ndescription: Judges\n---\nJudge it alone.\n");
        assert_eq!(prepared(&crew, &nests, &registered), before);
        std::fs::remove_file(root.join("profiles").join("judge.md")).unwrap();
        assert_eq!(prepared(&crew, &nests, &registered), before);
        let saved: AgentConfig = serde_json::from_value(before.clone()).unwrap();
        assert_eq!(prepared(&crew, &nests, &saved), before);
        assert_eq!(prepared(&crew, &plain, &saved), plain_before);
        assert!(ask(&nests).unwrap().contains("Judge it in parts."));
        assert!(hook.offers_start_tools(&record(&nests)));

        // Arko restarts: a new crew on the same folder reads the record.
        let (again, _) = crew_for(&session, &root, &luna());
        let hook = again.config().hook.unwrap();
        assert!(hook.offers_start_tools(&record(&nests)));
        assert!(!hook.offers_start_tools(&record(&plain)));
        // A run recorded before nesting has no start tools.
        let old = std::fs::read_to_string(session.join("worker-models.json")).unwrap();
        let mut old: serde_json::Value = serde_json::from_str(&old).unwrap();
        for setup in old.as_object_mut().unwrap().values_mut() {
            setup.as_object_mut().unwrap().remove("tools_added");
        }
        std::fs::write(session.join("worker-models.json"), old.to_string()).unwrap();
        let (older, _) = crew_for(&session, &root, &luna());
        assert!(
            !older
                .config()
                .hook
                .unwrap()
                .offers_start_tools(&record(&nests))
        );
    }

    /// A worker's setup is worked out once, as it starts. The hook then
    /// gives the same configuration for the run again and again, after its
    /// profile's file is edited or deleted, from another run's saved
    /// configuration as the registered one (as a resumed session has it),
    /// and after Arko restarts, which is what lets a saved session resume.
    #[test]
    fn a_recorded_run_gets_the_same_configuration_after_its_profile_changes() {
        let temp = tempfile::tempdir().unwrap();
        let (session, root) = (temp.path().join("session"), temp.path().join("arko"));
        std::fs::create_dir_all(&session).unwrap();
        judge(
            &root,
            "---\ndescription: Judges\nmodel: sol\neffort: max\n---\nJudge version one.\n",
        );
        let (crew, registered) = crew_for(&session, &root, &luna());
        let first = start("r1", &[("profile", "judge")]);
        let before = prepared(&crew, &first, &registered);
        assert_eq!(before["llm_config"]["model"]["id"], "gpt-6.1-sol");
        let opening = |crew: &Crew, request: &StartRequest| {
            let ask = crew.config().start_context.unwrap();
            ask(request)
        };
        assert_eq!(
            opening(&crew, &first).as_deref(),
            Some("# Your profile (judge)\nJudge version one.")
        );

        // Edited: the run keeps what it started with.
        judge(
            &root,
            "---\ndescription: Judges\nmodel: astra\neffort: low\n---\nJudge version two.\n",
        );
        assert_eq!(prepared(&crew, &first, &registered), before);
        assert_eq!(
            opening(&crew, &first).as_deref(),
            Some("# Your profile (judge)\nJudge version one.")
        );
        // A worker started now gets the edit.
        let second = start("r2", &[("profile", "judge")]);
        let after = prepared(&crew, &second, &registered);
        assert_eq!(after["llm_config"]["model"]["id"], "gpt-6-astra");
        assert_eq!(
            opening(&crew, &second).as_deref(),
            Some("# Your profile (judge)\nJudge version two.")
        );

        // The registered worker is another run's saved configuration, as in
        // a resumed session: the same result for each run.
        let saved: AgentConfig = serde_json::from_value(after.clone()).unwrap();
        assert_eq!(prepared(&crew, &first, &saved), before);
        assert_eq!(prepared(&crew, &second, &saved), after);

        // Deleted: the same.
        std::fs::remove_file(root.join("profiles").join("judge.md")).unwrap();
        assert_eq!(prepared(&crew, &first, &registered), before);
        assert_eq!(prepared(&crew, &second, &registered), after);
        let missing = start("r3", &[("profile", "judge")]);
        let hook = crew.config().hook.unwrap();
        let mut config = registered.clone();
        assert!(
            hook.prepare(&missing, &mut config)
                .unwrap_err()
                .starts_with("There is no worker profile `judge`.")
        );

        // Arko restarts, with the settings changed and the profile gone: a
        // new crew on the same folder reads the record. Before its workflow
        // is attached, the hook is only checking the saved runs.
        let mut other = luna();
        other.model = crate::agents::resolve_model("sol").unwrap().id;
        let (crew, registered) = crew_for(&session, &root, &other);
        crew.models.lock().unwrap().attached = false;
        assert_eq!(prepared(&crew, &first, &registered), before);
        assert_eq!(prepared(&crew, &second, &registered), after);
        crew.models.lock().unwrap().attached = true;
        assert_eq!(prepared(&crew, &first, &registered), before);
        assert_eq!(
            opening(&crew, &first).as_deref(),
            Some("# Your profile (judge)\nJudge version one.")
        );
    }

    /// A record from before profiles (a model, Claude Code's settings and
    /// the handover size, nothing more) loads and gives the run the same
    /// configuration as then.
    #[test]
    fn an_old_record_without_the_new_fields_is_applied_as_before() {
        let temp = tempfile::tempdir().unwrap();
        let (session, root) = (temp.path().join("session"), temp.path().join("arko"));
        std::fs::create_dir_all(&session).unwrap();
        let mut sol = luna();
        sol.model = crate::agents::resolve_model("sol").unwrap().id;
        let on_sol =
            crate::agents::worker_config(&crate::agents::AgentSettings::for_test(&sol, &sol))
                .unwrap();
        let run = Uuid::new_v4();
        let old = serde_json::json!({
            run.to_string(): {
                "llm_config": on_sol.llm_config,
                "handover": on_sol.handover,
            }
        });
        std::fs::write(
            session.join(WORKER_MODELS_FILE),
            serde_json::to_string_pretty(&old).unwrap(),
        )
        .unwrap();
        let (crew, registered) = crew_for(&session, &root, &luna());
        let recorded = crew.run_model(run).unwrap();
        assert_eq!(recorded.context_added, None);
        assert!(recorded.tools_added.is_empty());
        assert_eq!(recorded.profile, None);
        let mut request = start("r1", &[]);
        request.run_id = run;
        let applied = prepared(&crew, &request, &registered);
        assert_eq!(applied["llm_config"]["model"]["id"], "gpt-6.1-sol");
        let mut expected = registered.clone();
        expected.llm_config = on_sol.llm_config.clone();
        expected.handover = on_sol.handover.clone();
        assert_eq!(applied, serde_json::to_value(&expected).unwrap());
        assert_eq!(crew.config().start_context.unwrap()(&request), None);
        // A record written now has the old fields and the effort, which an
        // older Arko ignores, when nothing else is added.
        let mut fresh = start("r2", &[]);
        fresh.run_id = Uuid::new_v4();
        prepared(&crew, &fresh, &registered);
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(session.join(WORKER_MODELS_FILE)).unwrap(),
        )
        .unwrap();
        let keys: Vec<&String> = written[fresh.run_id.to_string()]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(keys, ["effort", "handover", "llm_config"]);
    }

    /// The project's instructions come first in a worker's first message,
    /// then what its profile adds.
    #[test]
    fn the_first_message_opens_with_the_project_and_then_the_profile() {
        let temp = tempfile::tempdir().unwrap();
        let (session, root) = (temp.path().join("session"), temp.path().join("arko"));
        std::fs::create_dir_all(&session).unwrap();
        judge(&root, "---\ndescription: Judges\n---\nJudge.\n");
        let (crew, registered) = crew_for(&session, &root, &luna());
        let with = start("r1", &[("profile", "judge")]);
        let without = start("r2", &[]);
        prepared(&crew, &with, &registered);
        prepared(&crew, &without, &registered);
        let ask = crew.config().start_context.unwrap();
        assert_eq!(ask(&without), None);
        crew.set_project(Some("# Project instructions\nUse tabs.\n".into()));
        let ask = crew.config().start_context.unwrap();
        assert_eq!(
            ask(&without).as_deref(),
            Some("# Project instructions\nUse tabs.\n")
        );
        assert_eq!(
            ask(&with).as_deref(),
            Some("# Project instructions\nUse tabs.\n\n# Your profile (judge)\nJudge.")
        );
    }

    /// Start parameters for choosing the setup, and for letting the worker
    /// start workers.
    #[test]
    fn start_agent_takes_a_profile_a_model_an_effort_and_nested() {
        let names: Vec<String> = start_parameters().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["cache", "profile", "model", "effort", "nested"]);
        assert!(
            start_parameters()
                .iter()
                .all(|parameter| !parameter.required)
        );
        let temp = tempfile::tempdir().unwrap();
        let (crew, _) = crew_for(&temp.path().join("s"), temp.path(), &luna());
        let config = crew.config();
        let offered: Vec<&str> = config
            .start_parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect();
        assert_eq!(offered, ["cache", "profile", "model", "effort", "nested"]);
        // It is a flag, not text.
        let nested = &config.start_parameters[4];
        assert_eq!(nested.param_type, ToolParameterType::Bool);
        assert_eq!(config.max_depth, 4);
    }

    /// `/workers` says what each worker runs, what it has cost once that is
    /// worth a line, and what it did.
    #[test]
    fn a_workers_lines_say_what_it_runs_and_what_it_cost() {
        let mut worker = WorkerFacts {
            id: "r2".into(),
            state: "running".into(),
            task: "Judge the render".into(),
            model: Some("gpt-6.1-sol".into()),
            effort: Some("high".into()),
            profile: Some("3d-judge".into()),
            parent: None,
            depth: 1,
            cost: Some((0.1234, 0.1234)),
            other_model: None,
            cache_lifetime: Some(Duration::from_secs(300)),
            worktree: None,
            report: Some("Done.".into()),
        };
        assert_eq!(
            describe_worker(&worker),
            "r2  running  Judge the render\n  runs gpt-6.1-sol at high effort, profile 3d-judge\n  cost $0.12\n  prompt cache of 5 minutes\n  Done."
        );
        // A plan covered the calls: they bill nothing, and the list prices
        // say what they are worth.
        worker.cost = Some((0.0, 0.31));
        worker.other_model = Some("gpt-6-luna".into());
        worker.profile = None;
        let text = describe_worker(&worker);
        assert!(
            text.contains(
                "\n  runs gpt-6.1-sol at high effort\n  ≈ $0.31 at API prices, $0.00 billed\n"
            ),
            "{text}"
        );
        // Under half a cent is no line, and a worker with no record names no
        // model.
        worker.cost = Some((0.0, 0.004));
        worker.model = None;
        let text = describe_worker(&worker);
        assert!(
            !text.contains("cost") && !text.contains("runs") && !text.contains("API"),
            "{text}"
        );
        // A worker that a worker started says whose it is.
        worker.parent = Some("r1".into());
        worker.depth = 2;
        assert!(
            describe_worker(&worker)
                .starts_with("r2  running  Judge the render\n  started by r1\n"),
            "{}",
            describe_worker(&worker)
        );
    }

    /// How many workers a starter has counts only those running or waiting
    /// to run, with the starts that passed the hook and have no record yet,
    /// and a start at the first level is the main agent's.
    #[test]
    fn a_starters_workers_count_only_while_they_run_or_wait() {
        let main = Uuid::new_v4();
        let mut r1 = record("r1", main);
        r1.depth = 1;
        let mut children = Vec::new();
        for n in 0..8 {
            let mut child = record(&format!("r{}", n + 2), r1.run_id);
            child.depth = 2;
            child.state = SubagentState::Finished;
            children.push(child);
        }
        let mut other = record("r10", main);
        other.depth = 1;
        let asking = |starter: Uuid| StartRequest {
            caller_run_id: starter,
            ..start("r11", &[])
        };
        let family = |runs: &[arko_workflows::subagents::SubagentRecord],
                      held: &[(Uuid, Uuid)],
                      request: &StartRequest| family_of(runs, held, request);
        let mut runs = vec![r1.clone(), other.clone()];
        runs.extend(children.clone());
        // Eight finished workers count for nothing, and the new one is at
        // the level below its starter's.
        let (found, parent) = family(&runs, &[], &asking(r1.run_id));
        assert_eq!((found.depth, found.live), (2, 0));
        assert_eq!(parent.as_deref(), Some("r1"));
        // A running one and a queued one count, a stopped one doesn't.
        runs[2].state = SubagentState::Running;
        runs[3].state = SubagentState::Queued;
        runs[4].state = SubagentState::Stopped;
        runs[5].state = SubagentState::Failed;
        runs[6].state = SubagentState::Interrupted;
        assert_eq!(family(&runs, &[], &asking(r1.run_id)).0.live, 2);
        // Starts that passed the hook and have no record count too, those of
        // another starter don't, and neither does the start being asked.
        let request = asking(r1.run_id);
        let held = [
            (Uuid::new_v4(), r1.run_id),
            (Uuid::new_v4(), r1.run_id),
            (Uuid::new_v4(), other.run_id),
            (request.run_id, r1.run_id),
            // (Its record is there already, so it counts once.)
            (children[0].run_id, r1.run_id),
        ];
        assert_eq!(family(&runs, &held, &request).0.live, 4);
        // The main agent's start, and a starter no record names, are at the
        // first level and have no starter.
        let (found, parent) = family(&runs, &held, &asking(main));
        assert_eq!((found, parent), (chooser::Family::MAIN, None));
        assert_eq!(
            family(&[], &[], &asking(r1.run_id)).0,
            chooser::Family::MAIN
        );
        // A deeper worker's starts are one level further.
        let mut deeper = record("r12", r1.run_id);
        deeper.depth = 2;
        runs.push(deeper.clone());
        assert_eq!(family(&runs, &[], &asking(deeper.run_id)).0.depth, 3);
    }

    /// "Stopping N workers" counts the workers a stop reaches, those that
    /// stop with a worker included and one already stopping left out.
    #[test]
    fn stopping_all_counts_what_the_stops_reach() {
        let main = Uuid::new_v4();
        let r1 = record("r1", main);
        let r2 = record("r2", r1.run_id);
        let r3 = record("r3", r2.run_id);
        let r4 = record("r4", main);
        let r5 = record("r5", main);
        let active = [r1.clone(), r2, r3, r4.clone(), r5.clone()];
        // The stop of r1 reaches r2 and r3 as well. r4 and r5 were stopping
        // already, so their stops changed nothing.
        assert_eq!(stopped_by(&active, &[r1.run_id]), 3);
        assert_eq!(stopped_by(&active, &[r1.run_id, r4.run_id]), 4);
        assert_eq!(stopped_by(&active, &[r1.run_id, r4.run_id, r5.run_id]), 5);
        assert_eq!(stopped_by(&active, &[]), 0);
        // A worker stopped on its own and one of its workers stopped before
        // it count once.
        assert_eq!(stopped_by(&active, &[active[1].run_id, r1.run_id]), 3);
    }

    /// The worker that started a worker is known from the records, and the
    /// workers below a worker are found through its workers.
    #[test]
    fn a_worker_started_by_a_worker_names_its_starter() {
        let lead = Uuid::new_v4();
        let r1 = record("r1", lead);
        let r2 = record("r2", lead);
        let r3 = record("r3", r1.run_id);
        let r4 = record("r4", r3.run_id);
        let records = [r1.clone(), r2.clone(), r3.clone(), r4.clone()];
        assert_eq!(started_by(&records, r1.run_id), None);
        assert_eq!(started_by(&records, r3.run_id).as_deref(), Some("r1"));
        assert_eq!(started_by(&records, r4.run_id).as_deref(), Some("r3"));
        assert_eq!(below(&records, r1.run_id), ["r3", "r4"]);
        assert!(below(&records, r2.run_id).is_empty());
    }
}

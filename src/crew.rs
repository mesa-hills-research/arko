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
use crate::billing;
use crate::chooser;
use crate::kept_notes::KeptNotes;
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
/// `start_agent`'s parameters for narrowing the worker's skills and MCP
/// servers to a list of names.
pub const SKILLS_PARAM: &str = "skills";
pub const MCP_PARAM: &str = "mcp";
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
    /// The long prompt cache for workers on the session's worker model.
    pub cache_1h: bool,
    /// How the session reaches OpenAI models.
    pub openai_auth: Option<crate::providers::OpenAiAuth>,
    /// The client's cache lifetime for the worker model and authentication.
    pub cache_lifetime: PromptCacheLifetime,
    pub feed: Option<remote::Feed>,
    /// Why new workers can't start on a model, when they can't: its
    /// provider has no key in this session (or its key was removed while
    /// Arko runs), or Claude Code isn't installed and signed in.
    pub refuse: Option<Refuse>,
    /// Who asks the user to allow a worker billed per token (none: no one
    /// can).
    pub(crate) asker: Option<Arc<crate::inputs::Asker>>,
    /// Arko's folder, where the sessions share the day's API spend (none:
    /// the session counts its own only).
    pub(crate) root: Option<PathBuf>,
}

/// What the start hook holds a worker's billing to (see [`billing`]): the
/// models the user allowed for the session, the day's API spend, and who
/// asks the user.
struct Spending {
    allowed: billing::Allowed,
    spend: billing::DaySpend,
    asker: Option<Arc<crate::inputs::Asker>>,
    output: Arc<Output>,
    feed: Option<remote::Feed>,
}

impl Spending {
    /// Tells the user, once a day, that the day's API spend `spent` reached
    /// the budget of `basis`, when workers are held to it.
    fn tell_budget(&self, basis: &chooser::Basis, spent: f64) {
        let Some(budget) = basis.api_budget else {
            return;
        };
        if basis.billing == billing::WorkerBilling::Plans
            || spent < budget
            || !self.spend.tell_once()
        {
            return;
        }
        let text = billing::budget_notice(basis.billing, spent, budget);
        self.output.echo(&self.output.warn(&text));
        if let Some(feed) = &self.feed {
            feed.notice(remote::Level::Warn, &text);
        }
    }

    /// Asks the user to allow the worker `request` starts on `model` in the
    /// approval tray, with the day's spend (`spent`, when the budget's check
    /// read it) and the budget it reached (`over`), and waits for the
    /// answer. The wait ends without one when the turn stops or the workflow
    /// does. Words the user wrote beside an approval reach the main agent
    /// as a note.
    fn ask(
        &self,
        request: &StartRequest,
        model: &arko_ai::Model,
        spent: Option<f64>,
        over: Option<(f64, f64)>,
        workflow: &Weak<Workflow>,
    ) -> Result<billing::Decision, String> {
        let approvals = self
            .asker
            .as_ref()
            .map(|asker| asker.approvals.clone())
            .filter(crate::approvals::Approvals::can_answer)
            .ok_or_else(|| billing::cant_ask(model))?;
        // (The start hook can't wait asynchronously, so it waits on a thread
        // the runtime can do without.)
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime)
                if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
            {
                runtime
            }
            _ => return Err(billing::cant_ask(model)),
        };
        let spent = spent.unwrap_or_else(|| self.spend.today(workflow.upgrade().as_deref(), true));
        let approval = billing::approval(model, &request.task, Some(spent), over);
        let mut waiter = approvals
            .ask(
                request.caller_run_id,
                None,
                crate::approvals::Approval::WorkerBilling(approval),
            )
            .ok_or_else(|| billing::cant_ask(model))?;
        let feed = self.feed.clone();
        let stopped = move || {
            feed.as_ref().is_some_and(remote::Feed::turn_is_stopping)
                || workflow
                    .upgrade()
                    .is_some_and(|workflow| workflow.is_cancelled())
        };
        let answer = tokio::task::block_in_place(|| {
            runtime.block_on(async {
                let stops = async {
                    while !stopped() {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                };
                tokio::select! {
                    answer = waiter.receiver() => answer.unwrap_or(crate::approvals::Answer::Unanswered),
                    () = stops => crate::approvals::Answer::Unanswered,
                }
            })
        });
        drop(waiter);
        if let crate::approvals::Answer::Approved {
            words: Some(words), ..
        } = &answer
            && let Some(workflow) = workflow.upgrade()
        {
            let note = arko_workflows::notes::Note::new(billing::allowed_with(model, words))
                .source(arko_workflows::notes::NoteSource::Host)
                .to([crate::agents::MAIN])
                .for_run(request.caller_run_id);
            if let Err(error) = workflow.post_note(note) {
                log::warn!("could not pass the user's words on to the main agent: {error}");
            }
        }
        Ok(billing::Decision::of(&answer))
    }
}

/// See [`Setup::refuse`]. The text is written for the main agent.
pub type Refuse = Arc<dyn Fn(&arko_ai::Model) -> Option<String> + Send + Sync>;

/// What opens the user's message to a worker in the worker's mail. Like the
/// main agent's notes (see `agents::operator_note_hint`) it says text written
/// only in thinking may never reach the user. The user watches the worker's
/// own view, which shows the text it writes and its report.
pub const USER_MESSAGE_HEADING: &str = "The user sent you a message directly. Text you write only in your thinking may never reach them, so answer in your text or in your report:";

/// How long each worker of the session ran, by its handle (milliseconds),
/// in its folder.
const RUN_TIMES_FILE: &str = "worker-times.json";

/// How many calls each worker of the session made, by its handle, in its
/// folder.
const TOOL_CALLS_FILE: &str = "worker-calls.json";

/// What `file` of the session in `dir` kept of each worker (none: a session
/// from before it was kept).
fn load_kept<T: serde::de::DeserializeOwned>(
    dir: &std::path::Path,
    file: &str,
) -> BTreeMap<String, T> {
    std::fs::read_to_string(dir.join(file))
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
    /// The skills it has, when that is a list and not everything its
    /// starter has.
    pub skills: Option<Vec<String>>,
    /// The MCP servers it reaches, when that is a list and not everything
    /// its starter has.
    pub mcp: Option<Vec<String>>,
    /// The worker that started it, when a worker did and not the main agent.
    pub parent: Option<String>,
    /// How far it is from the main agent, 1 for the main agent's workers.
    pub depth: u32,
    /// What its model calls billed and would cost at API list prices, in
    /// dollars, when the session counted them.
    pub cost: Option<(f64, f64)>,
    /// What it and the workers under it (and theirs) cost together, billed
    /// and at API list prices, when the workers under it cost anything.
    pub family_cost: Option<(f64, f64)>,
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
    /// The skills the run has, by name. None is all those its starter has
    /// that aren't for the main agent alone (also for a run recorded before
    /// skills were narrowed), and a list is only those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// The MCP servers the run reaches, by name, in the same way as
    /// `skills`. [`Crew::mcp_servers_of`] gives it to the MCP bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<Vec<String>>,
    /// The names its profile asked for that its starter couldn't give, as a
    /// sentence for the start result and the lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_out: Option<String>,
    /// The names the run's model goes by, which skills' `models` are
    /// matched against for its skill list (see
    /// [`skills::model_names`](crate::skills::model_names)). None for a run
    /// recorded before they were kept, which keeps the names its
    /// configuration has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_names: Option<Vec<String>>,
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
            skills: None,
            mcp: None,
            left_out: None,
            model_names: None,
        }
    }

    /// Whether the run may start workers of its own: its setup adds the
    /// start tools.
    pub(crate) fn nests(&self) -> bool {
        self.tools_added.iter().any(|tool| tool == START_AGENT)
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
        // The skills are the run's own too, also over a list that a
        // configuration of another run was saved with. A worker never has
        // the skills kept to the main agent, also from a configuration saved
        // before that was said in it.
        if let Some(skills) = &mut config.skills_config {
            skills.subagent = true;
            skills.only = self.skills.clone();
            // Its skill list is for its own model.
            if let Some(names) = &self.model_names {
                skills.model_names = names.clone();
            }
            // A worker given no skills has nothing to load, now or later.
            if self.skills.as_ref().is_some_and(Vec::is_empty) {
                skills.offer_when_empty = false;
            }
        }
        // The start tools are the run's own: none a configuration of
        // another run had, and the ones this run's setup adds.
        // The same goes for the MCP tool, which only a worker that reaches a
        // server is offered.
        let tools = &mut config.tool_config.available_tool_names;
        tools.retain(|name| {
            !NESTED_TOOL_NAMES.contains(&name.as_str()) && name != arko_agents::mcp::MCP_TOOL_NAME
        });
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

/// Whether `start_agent`'s `cache` (or a profile's) asks for the long prompt
/// cache: `long`, or `1h` as it was first called, against `short` or `5m`.
pub(crate) fn long_cache_asked(text: &str) -> Result<bool, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "long" | "1h" => Ok(true),
        "short" | "5m" => Ok(false),
        _ => Err(format!("cache is long or short, not {text}")),
    }
}

/// The longest skill or MCP server name a list takes, in characters.
const MAX_NAME_LENGTH: usize = 100;

/// Why `name` can't be in a list of skills or MCP servers, when it can't.
pub(crate) fn name_problem(name: &str) -> Option<&'static str> {
    if name.trim().is_empty() {
        Some("has an empty name")
    } else if name.chars().count() > MAX_NAME_LENGTH {
        Some("has a name over 100 characters")
    } else if name.contains(',') || name.chars().any(char::is_control) {
        Some("has a name with a comma or a control character")
    } else {
        None
    }
}

/// The names that `start_agent`'s `skills` or `mcp`, or a profile's, give:
/// text with the names separated by commas, a JSON array of them, or `none`
/// for no names at all. `None` for blank text, which gives nothing. A name
/// given twice counts once. The error says what is wrong, to follow the
/// parameter's name.
pub(crate) fn names_asked(text: &str) -> Result<Option<Vec<String>>, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if text.eq_ignore_ascii_case("none") {
        return Ok(Some(Vec::new()));
    }
    let json = text.starts_with('[');
    let listed: Vec<String> = if json {
        serde_json::from_str(text).map_err(|_| NAMES_FORM)?
    } else {
        text.split(',').map(str::to_string).collect()
    };
    let mut names: Vec<String> = Vec::new();
    for name in listed {
        let name = name.trim();
        // (A comma at the end or two in a row leave no name between them.)
        if name.is_empty() && !json {
            continue;
        }
        if let Some(problem) = name_problem(name) {
            return Err(problem);
        }
        if !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    if names.is_empty() && !json {
        return Err(NAMES_FORM);
    }
    Ok(Some(names))
}

/// What a list of names can be, for the error of one that is none of them.
const NAMES_FORM: &str = "isn't names separated by commas, a JSON array or none";

/// The names `names` as a sentence's list, cut after a dozen.
pub(crate) fn names_words(names: &[String]) -> String {
    const SHOWN: usize = 12;
    if names.len() <= SHOWN {
        return crate::and_list(names);
    }
    format!(
        "{}, and {} more",
        names[..SHOWN].join(", "),
        names.len() - SHOWN
    )
}

/// What a narrowed set of `noun`s (`skill`, `MCP server`) is as a phrase:
/// `only the skills ponytail and release-notes`, or `no MCP servers`.
pub(crate) fn only_words(noun: &str, names: &[String]) -> String {
    match names {
        [] => format!("no {noun}s"),
        [one] => format!("only the {noun} {one}"),
        many => format!("only the {noun}s {}", names_words(many)),
    }
}

/// The phrases that say a worker's narrowed sets, one for each set that is
/// narrowed. A set that is everything its starter has says nothing.
pub(crate) fn narrowing_phrases(skills: Option<&[String]>, mcp: Option<&[String]>) -> Vec<String> {
    [(skills, "skill"), (mcp, "MCP server")]
        .into_iter()
        .filter_map(|(names, noun)| names.map(|names| only_words(noun, names)))
        .collect()
}

/// The prompt cache a worker run asks for in `start_agent`'s `cache`, else
/// `default_long`, on `config`. Models without a long cache ignore it.
fn apply_cache(
    request: &StartRequest,
    config: &mut AgentConfig,
    default_long: bool,
) -> Result<(), String> {
    let chosen = request
        .parameters
        .get(CACHE_PARAM)
        .and_then(Value::as_str)
        .map(long_cache_asked)
        .transpose()?;
    let generation = &mut config.llm_config.generation_config;
    generation.long_prompt_cache = chosen.unwrap_or(default_long).then_some(true);
    if chosen == Some(false) {
        crate::providers::ask_short_cache(&config.llm_config.model, generation);
    }
    Ok(())
}

/// The start hook. A run seen before gets the setup it started with, always.
/// A start works its setup out ([`chooser::choose`]) and records it before
/// the run is saved. A run saved before records existed, checked as a
/// checkpoint is read back, gets the registered worker's configuration with
/// its prompt cache.
fn prepare(
    hook: &StartHook,
    request: &StartRequest,
    config: &mut AgentConfig,
) -> Result<(), String> {
    let StartHook {
        models,
        workflow,
        flight,
        feed,
        refuse,
        default_long,
        spending,
        hub,
        ..
    } = hook;
    let (feed, refuse, default_long) = (feed.as_ref(), refuse.as_ref(), *default_long);
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
        return apply_cache(request, config, default_long);
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
            // What its starter has, which the worker's skills and servers
            // are chosen from (the main agent has no record: everything).
            let starter = models
                .lock()
                .unwrap()
                .runs
                .get(&request.caller_run_id)
                .cloned();
            let servers = servers_of(hub);
            let setup = chooser::choose_in(
                &new.basis,
                request,
                refuse,
                family,
                starter.as_ref(),
                &servers,
            )
            .and_then(|setup| {
                let weak = workflow.lock().unwrap().clone();
                pay_for(&setup, &new.basis, request, family, refuse, spending, &weak)?;
                Ok(setup)
            });
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
            apply_cache(request, config, default_long)?;
            WorkerSetup::of(config)
        }
    };
    setup.apply(config);
    let mut models = models.lock().unwrap();
    models.runs.insert(request.run_id, setup);
    models.save()
}

/// Holds the start `request` of a worker with `setup` (at `family`) to the
/// session's worker billing ([`billing::gate`]). A model billed per token
/// may be refused, or wait here for the user's OK.
fn pay_for(
    setup: &WorkerSetup,
    basis: &chooser::Basis,
    request: &StartRequest,
    family: chooser::Family,
    refuse: Option<&Refuse>,
    spending: &Spending,
    workflow: &Weak<Workflow>,
) -> Result<(), String> {
    let model = &setup.llm_config.model;
    let spent = std::cell::Cell::new(None);
    let gate = billing::gate(
        basis.billing,
        basis.api_budget,
        model,
        basis.providers.openai_auth,
        family.depth > 1,
        spending.allowed.has(model),
        || {
            let today = spending.spend.today(workflow.upgrade().as_deref(), true);
            spent.set(Some(today));
            today
        },
        || chooser::plan_models(basis, refuse),
    );
    if let Some(spent) = spent.get() {
        spending.tell_budget(basis, spent);
    }
    match gate {
        billing::Gate::Go => Ok(()),
        billing::Gate::Refuse(why) => Err(why),
        billing::Gate::Ask { over } => {
            match spending.ask(request, model, spent.get(), over, workflow)? {
                billing::Decision::Allow => Ok(()),
                billing::Decision::AllowForSession => {
                    spending.allowed.add(model);
                    Ok(())
                }
                other => Err(billing::refusal(&other, model)),
            }
        }
    }
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

/// The worker `handle` (run `run`) as a notice names it among `runs`: `r2`,
/// or `r2 (under r1)` when a worker started it.
pub(crate) fn named_in(runs: &[SubagentRecord], handle: &str, run: Uuid) -> String {
    match started_by(runs, run) {
        Some(parent) => format!("{handle} (under {parent})"),
        None => handle.to_string(),
    }
}

/// The runs a `wait_for_agents` without ids waits for, among the `active`
/// ones of the session's workers `all`: those of the worker `waiting` that
/// started them, or, for the main agent (`None`), those that no worker started.
pub(crate) fn waited_runs(
    active: &[SubagentRecord],
    all: &[SubagentRecord],
    waiting: Option<Uuid>,
) -> Vec<String> {
    active
        .iter()
        .filter(|run| match waiting {
            Some(waiting) => run.caller_run_id == waiting,
            None => !all.iter().any(|other| other.run_id == run.caller_run_id),
        })
        .map(|run| run.handle.clone())
        .collect()
}

/// The workers in the order a list shows a family: each worker followed by
/// the workers it started.
fn in_family_order(facts: Vec<WorkerFacts>) -> Vec<WorkerFacts> {
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

/// What `/stop-workers NAME` says once it stopped `worker`: the workers that
/// stop with it, and who can restart it, the agent that started it. `starter`
/// is the worker that started it, none for the main agent's workers, whose
/// starter is the main agent.
pub(crate) fn stopping_words(worker: &str, starter: Option<&str>, with: &[String]) -> String {
    let with_it = match with {
        [] => String::new(),
        [one] => format!(" Its worker {one} stops with it."),
        many => format!(" Its workers {} stop with it.", crate::and_list(many)),
    };
    format!(
        "Stopping {worker}.{with_it} A message from {} resumes {}.",
        starter.unwrap_or("the agent"),
        if with.is_empty() { "it" } else { worker }
    )
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
    requests: Arc<crate::worker_cache::Requests>,
    workflow: Arc<Mutex<Weak<Workflow>>>,
    /// The starts that passed the hook and have no record yet, by run, with
    /// their starter and since when.
    flight: Mutex<HashMap<Uuid, (Uuid, Instant)>>,
    feed: Option<remote::Feed>,
    refuse: Option<Refuse>,
    default_long: bool,
    spending: Arc<Spending>,
    /// How the session reaches OpenAI models, which says whether one is
    /// billed per token.
    openai_auth: Option<crate::providers::OpenAiAuth>,
    /// The session's MCP servers, once it has them (see [`Crew::set_mcp_hub`]).
    hub: HubSlot,
}

/// The prompt cache of the worker run `record`, once it has ended, from its
/// setup in `models` and its latest request in `requests`. `None` while it
/// runs, and when its cache lifetime or latest request isn't known.
fn cache_of(
    models: &Mutex<WorkerRecords>,
    requests: &crate::worker_cache::Requests,
    record: &SubagentRecord,
    openai_auth: Option<crate::providers::OpenAiAuth>,
) -> Option<crate::worker_cache::WorkerCache> {
    if record.state.is_active() {
        return None;
    }
    let llm = models
        .lock()
        .unwrap()
        .runs
        .get(&record.run_id)?
        .llm_config
        .clone();
    let request = requests.get(record.run_id)?;
    let at = chrono::DateTime::from_timestamp_millis(request.at)?;
    crate::worker_cache::of(&record.handle, &llm, at, request.input, openai_auth)
}

/// What the crew's events task received next: an agent's event (`None` once
/// they ended), or a worker's.
enum Next {
    Agent(Option<Box<arko_workflows::WorkflowEvent>>),
    Worker(
        Result<arko_workflows::subagents::SubagentEvent, tokio::sync::broadcast::error::RecvError>,
    ),
}

/// Where the crew reads the agents' events: the workflow's lossless stream
/// (or a test's).
trait AgentEvents {
    /// The next event, waiting for one. `None` once they ended.
    async fn next(&mut self) -> Option<arko_workflows::WorkflowEvent>;
    /// How many events wait.
    fn waiting(&self) -> usize;
    /// The next event if one waits.
    fn take(&mut self) -> Option<arko_workflows::WorkflowEvent>;
}

impl AgentEvents for arko_workflows::EventReceiver {
    async fn next(&mut self) -> Option<arko_workflows::WorkflowEvent> {
        self.recv().await
    }

    fn waiting(&self) -> usize {
        self.len()
    }

    fn take(&mut self) -> Option<arko_workflows::WorkflowEvent> {
        self.try_recv()
    }
}

/// The next of the agents' `events`, and never once they ended (`None`).
async fn next_agent_event<A: AgentEvents>(
    events: &mut Option<A>,
) -> Option<Box<arko_workflows::WorkflowEvent>> {
    match events {
        Some(events) => events.next().await.map(Box::new),
        None => std::future::pending().await,
    }
}

/// Follows the agents' events (`agents`) and the workers' (`workers`) for
/// the crew. `request` takes each agent event (it keeps the workers'
/// requests), and `worker` each worker event, until either says to stop
/// (false). The two take turns, so a busy agent doesn't hold up the
/// workers' news, and a long run of agent events lets other tasks run.
///
/// A worker's event comes after the agent events that were waiting when it
/// came. The engine publishes a run's events before its end, so its last
/// request is in the record before its end reads it.
async fn follow<A: AgentEvents>(
    agents: A,
    mut workers: tokio::sync::broadcast::Receiver<arko_workflows::subagents::SubagentEvent>,
    mut request: impl FnMut(&arko_workflows::WorkflowEvent) -> bool,
    mut worker: impl FnMut(
        Result<arko_workflows::subagents::SubagentEvent, tokio::sync::broadcast::error::RecvError>,
    ) -> bool,
) {
    let mut agents = Some(agents);
    loop {
        let received = tokio::select! {
            event = next_agent_event(&mut agents) => Next::Agent(event),
            event = workers.recv() => Next::Worker(event),
        };
        match received {
            Next::Agent(Some(event)) => {
                if !request(&event) {
                    return;
                }
                tokio::task::coop::consume_budget().await;
            }
            Next::Agent(None) => agents = None,
            Next::Worker(received) => {
                if let Some(agents) = agents.as_mut() {
                    for _ in 0..agents.waiting() {
                        let Some(event) = agents.take() else { break };
                        if !request(&event) {
                            return;
                        }
                    }
                }
                if !worker(received) {
                    return;
                }
            }
        }
    }
}

/// Where the crew and its start hook share the session's MCP hub.
type HubSlot = Arc<Mutex<Option<Arc<arko_agents::McpHub>>>>;

/// The servers of the hub in `slot` now (none without a hub).
fn servers_of(slot: &HubSlot) -> Vec<arko_agents::mcp::ServerFacts> {
    let hub = slot.lock().unwrap().clone();
    hub.map(|hub| hub.servers()).unwrap_or_default()
}

impl SubagentHook for StartHook {
    fn prepare(&self, request: &StartRequest, config: &mut AgentConfig) -> Result<(), String> {
        prepare(self, request, config)
    }

    /// What the worker runs and its profile, for its starter: the start's
    /// result and `list_agents` say it, so a starter that chose a model sees
    /// what it got and one that lost track of its workers sees what each is.
    /// A model billed per token says so.
    fn describe(&self, record: &SubagentRecord) -> Option<String> {
        let models = self.models.lock().unwrap();
        let setup = models.runs.get(&record.run_id)?;
        let model = &setup.llm_config.model;
        let mut text = runs_words(
            &crate::providers::model_name(model),
            setup.effort.map(|effort| effort.to_string()).as_deref(),
            setup.profile.as_deref(),
        );
        let narrowed = narrowing_phrases(setup.skills.as_deref(), setup.mcp.as_deref());
        if !narrowed.is_empty() {
            text.push_str(&format!(", with {}", narrowed.join(" and ")));
        }
        if crate::providers::plan_of(model, self.openai_auth).is_none() {
            text.push_str(&format!(
                ", billed per token ({})",
                billing::api_label(model)
            ));
        }
        if let Some(left_out) = &setup.left_out {
            text.push_str(&format!(". {left_out}"));
        }
        drop(models);
        // Once it has ended, what continuing it re-reads, so that the
        // starter weighs that before continuing it.
        if let Some(cache) = cache_of(&self.models, &self.requests, record, self.openai_auth) {
            text.push_str(&format!(", {}", cache.line(chrono::Utc::now())));
        }
        Some(text)
    }

    /// What the worker used, as the last line of the news its starter gets
    /// when it ends, from its recorded result (so a run the session restores
    /// has one too). Nothing when its run reported no tokens.
    fn report_note(&self, record: &SubagentRecord, result: Option<&AgentResult>) -> Option<String> {
        let models = self.models.lock().unwrap();
        let provider = &models.runs.get(&record.run_id)?.llm_config.model.provider;
        crate::providers::run_usage_line(&record.handle, &result?.info().usage, provider)
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
            .is_some_and(WorkerSetup::nests)
    }
}

/// The text a run's first message opens with: the project's instructions,
/// then what the run's setup adds (its profile's instructions).
fn first_message_context(
    project: &Mutex<Option<String>>,
    records: &Mutex<WorkerRecords>,
    kept: &KeptNotes,
    request: &StartRequest,
) -> Option<String> {
    let project = project.lock().unwrap().clone();
    let added = records
        .lock()
        .unwrap()
        .runs
        .get(&request.run_id)
        .and_then(|setup| setup.context_added.clone());
    [project, added, kept.for_worker(&request.handle)]
        .into_iter()
        .flatten()
        .reduce(|text, part| format!("{}\n\n{part}", text.trim_end()))
}

/// The parameters of the host's own that `start_agent` takes: the prompt
/// cache, the worker's profile, model and effort, the skills and MCP servers
/// it gets, and whether it may start workers of its own.
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
            "`long` or `short` (`1h` and `5m` work too), how long the worker's prompt cache lasts after its last step. `long` is an hour on Claude models and 12 hours on OpenAI GPT-5.6 and later with an API key, and costs more to write. `short` is 5 minutes on Claude and 30 minutes on OpenAI. Claude Code uses an hour on a Claude plan when you choose none. ChatGPT keeps its own 30-minute cache.",
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
        text(
            SKILLS_PARAM,
            "Skill names separated by commas, from your own. The worker gets only these, and none gives it no skills. Left out, it gets yours except those marked for the main agent alone.",
        ),
        text(
            MCP_PARAM,
            "MCP server names separated by commas, from your own. The worker reaches only these, and none gives it no servers. Left out, it gets yours except those marked for the main agent alone.",
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

/// What a worker runs, as a sentence: "runs sol at high effort, profile judge".
fn runs_words(model: &str, effort: Option<&str>, profile: Option<&str>) -> String {
    let mut text = format!("runs {model}");
    if let Some(effort) = effort {
        text.push_str(&format!(" at {effort} effort"));
    }
    if let Some(profile) = profile {
        text.push_str(&format!(", profile {profile}"));
    }
    text
}

/// A worker's lines for `/workers`, indented two columns for each level it is
/// below the main agent's workers, as the agent list is.
pub(crate) fn describe_worker(worker: &WorkerFacts) -> String {
    let indent = "  ".repeat(worker.depth.saturating_sub(1) as usize);
    describe_lines(worker)
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_lines(worker: &WorkerFacts) -> String {
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
        line.push_str(&format!(
            "\n  {}",
            runs_words(model, worker.effort.as_deref(), worker.profile.as_deref())
        ));
    }
    // The skills and servers it was narrowed to.
    for phrase in narrowing_phrases(worker.skills.as_deref(), worker.mcp.as_deref()) {
        line.push_str(&format!("\n  {phrase}"));
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
    // What it cost with its workers, once that is more than its own.
    if let Some((billed, list)) = worker
        .family_cost
        .filter(|(_, list)| *list >= worker.cost.map_or(0.0, |(_, own)| own) + 0.005)
    {
        line.push_str(&format!(
            "\n  {} with its workers",
            if list > billed + 0.005 {
                format!("≈ ${list:.2}")
            } else {
                format!("${billed:.2}")
            }
        ));
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
    /// Each worker run's latest model request, which its prompt cache lasts
    /// from.
    requests: Arc<crate::worker_cache::Requests>,
    /// The project's instructions new workers get in their first message,
    /// if any (see `project`).
    project: Arc<Mutex<Option<String>>>,
    /// The notes the main agent keeps, which new workers get in their first
    /// message.
    kept: Arc<KeptNotes>,
    workflow: Arc<Mutex<Weak<Workflow>>>,
    lead: Mutex<Option<Uuid>>,
    /// How many failed model requests in a row stop a worker (see
    /// `config`), if any do.
    retry_cap: Mutex<Option<u32>>,
    /// What workers billed per token may spend.
    spending: Arc<Spending>,
    /// The session's MCP servers, which a worker's set is chosen from.
    hub: HubSlot,
    wake: mpsc::UnboundedSender<()>,
    events: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Crew {
    pub fn new(setup: Setup) -> (Arc<Self>, mpsc::UnboundedReceiver<()>) {
        let (wake, woken) = mpsc::unbounded_channel();
        let spending = Arc::new(Spending {
            allowed: billing::Allowed::load(&setup.session_dir),
            spend: billing::DaySpend::new(setup.root.as_deref(), &setup.session_id),
            asker: setup.asker.clone(),
            output: setup.output.clone(),
            feed: setup.feed.clone(),
        });
        (
            Arc::new(Self {
                models: Arc::new(Mutex::new(WorkerRecords::load(&setup.session_dir))),
                requests: Arc::new(crate::worker_cache::Requests::load(&setup.session_dir)),
                project: Arc::default(),
                kept: KeptNotes::new(&setup.session_dir),
                spending,
                hub: Arc::default(),
                setup,
                workflow: Arc::default(),
                lead: Mutex::new(None),
                retry_cap: Mutex::new(None),
                wake,
                events: Mutex::new(None),
            }),
            woken,
        )
    }

    pub fn config(&self) -> SubagentsConfig {
        let default_long = self.setup.cache_1h;
        let models = self.models.clone();
        let records = self.models.clone();
        let workflow = self.workflow.clone();
        let feed = self.setup.feed.clone();
        let refuse = self.setup.refuse.clone();
        let project = self.project.clone();
        let kept = self.kept.clone();
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
                requests: self.requests.clone(),
                workflow,
                flight: Mutex::default(),
                feed,
                refuse,
                default_long,
                spending: self.spending.clone(),
                openai_auth: self.setup.openai_auth,
                hub: self.hub.clone(),
            }))
            // Asked as each worker starts: a run keeps its first message.
            .start_context(Arc::new(move |request: &StartRequest| {
                first_message_context(&project, &records, &kept, request)
            }));
        for parameter in start_parameters() {
            config = config.start_parameter(parameter);
        }
        config = if self.setup.max_running == NO_LIMIT {
            config.unlimited()
        } else {
            config.max_running(self.setup.max_running as usize)
        };
        // The framework reads the cap when the workflow is built.
        if let Some(cap) = *self.retry_cap.lock().unwrap() {
            config = config.max_failed_requests(cap);
        }
        // A worker's workspace may be outside the lead's only while outside
        // access is on, which can change while the session runs: the
        // `outside` hook refuses the others.
        config.workspaces_anywhere()
    }

    /// Stops a worker after `cap` failed model requests in a row, in the
    /// workflow built from now on (none: no cap).
    pub fn set_retry_cap(&self, cap: Option<u32>) {
        *self.retry_cap.lock().unwrap() = cap;
    }

    /// The model new workers start on from now on.
    pub fn set_new_workers(&self, new: NewWorkers) {
        self.models.lock().unwrap().new = Some(new);
    }

    /// The day's API spend across the sessions of Arko's folder, once the
    /// session's workflow is there.
    pub(crate) fn spent_today(&self) -> Option<f64> {
        let workflow = self.workflow()?;
        Some(self.spending.spend.today(Some(&workflow), true))
    }

    /// Shares the session's API spend of the day with the folder's other
    /// sessions when it changed, and tells the user once a day that the
    /// spend reached the budget. Called as the status is read.
    pub(crate) fn track_spend(&self) {
        let Some(workflow) = self.workflow() else {
            return;
        };
        let spent = self.spending.spend.today(Some(&workflow), false);
        if let Some(new) = self.new_workers() {
            self.spending.tell_budget(&new.basis, spent);
        }
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

    /// Gives the crew the session's MCP servers, which a start names a
    /// worker's from. Without them no worker has any, and none is offered
    /// the MCP tool.
    // (The session's engine calls it once the MCP bridge is wired in.)
    #[allow(dead_code)]
    pub fn set_mcp_hub(&self, hub: Arc<arko_agents::McpHub>) {
        *self.hub.lock().unwrap() = Some(hub);
    }

    /// The session's MCP servers now.
    pub(crate) fn mcp_servers(&self) -> Vec<arko_agents::mcp::ServerFacts> {
        servers_of(&self.hub)
    }

    /// The MCP servers the run `run` reaches, by name. `None` is all of them,
    /// for the main agent, whose runs have no record. A worker's is always a
    /// list: the one it started with, without any server the hub marks
    /// `workers: false`, or, for a worker that has what its starter has, the
    /// servers workers may reach now, so one added later counts. It is
    /// the same after the session resumes.
    // (The MCP bridge's access check calls it.)
    pub fn mcp_servers_of(&self, run: Uuid) -> Option<Vec<String>> {
        let recorded = self.models.lock().unwrap().runs.get(&run)?.mcp.clone();
        let servers = self.mcp_servers();
        Some(match recorded {
            Some(list) => list
                .into_iter()
                .filter(|name| servers.iter().all(|s| s.name != *name || s.workers))
                .collect(),
            None => servers
                .iter()
                .filter(|server| server.workers)
                .map(|server| server.name.clone())
                .collect(),
        })
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
        let events = workflow.subscribe_subagents();
        let requests = workflow.subscribe_events_lossless();
        let runs = workflow.subagents();
        self.reconcile_requests(workflow, &runs);
        if let Some(feed) = &self.setup.feed {
            // The workers of a resumed session, for the clients' lists, with
            // how long they ran and how many calls they made before, and the
            // prompt caches of those that ended.
            let ran: BTreeMap<String, u64> = load_kept(&self.setup.session_dir, RUN_TIMES_FILE);
            let calls: BTreeMap<String, u32> = load_kept(&self.setup.session_dir, TOOL_CALLS_FILE);
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
                let cache = cache_of(&self.models, &self.requests, run, self.setup.openai_auth);
                feed.worker_restored_with_cache(
                    &run.handle,
                    &run.task,
                    state,
                    run.report.as_deref(),
                    run.error.as_deref(),
                    run.since.timestamp_millis(),
                    ran.get(&run.handle).copied().unwrap_or(0),
                    calls.get(&run.handle).copied().unwrap_or(0),
                    cache.as_ref(),
                );
                // Who started it, what it started with and what it cost before.
                if let Some(parent) = started_by(&runs, run.run_id) {
                    feed.worker_under(&run.handle, &parent);
                }
                feed.worker_info(&run.handle, self.worker_info(workflow, run, true));
            }
        }
        if self.has_mail() {
            let _ = self.wake.send(());
        }
        let (weak, also) = (Arc::downgrade(self), Arc::downgrade(self));
        *self.events.lock().unwrap() = Some(tokio::spawn(follow(
            requests,
            events,
            move |event| {
                weak.upgrade()
                    .map(|crew| crew.note_request(event))
                    .is_some()
            },
            move |received| {
                also.upgrade()
                    .is_some_and(|crew| crew.worker_event(received))
            },
        )));
    }

    /// What the crew does with a worker's event `received`: says it, tells
    /// the feed, and wakes the session. False once the workers' events end.
    fn worker_event(
        &self,
        received: Result<
            arko_workflows::subagents::SubagentEvent,
            tokio::sync::broadcast::error::RecvError,
        >,
    ) -> bool {
        let event = match received {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                let _ = self.wake.send(());
                return true;
            }
            Err(_) => return false,
        };
        // (A worker a worker started says whose it is.)
        let name = self.named(&event.handle, event.run_id);
        let text = match event.kind.clone() {
            SubagentEventKind::Queued => Some(format!("{name} queued")),
            SubagentEventKind::Started => Some(format!("{name} started")),
            SubagentEventKind::Resumed => Some(format!("{name} resumed")),
            SubagentEventKind::Finished { summary } => {
                Some(format!("{name} reported. {}", ui::cut(&summary, 180)))
            }
            SubagentEventKind::Failed { error } => Some(format!("{name} failed ({error})")),
            SubagentEventKind::Stopped { reason, .. } => Some(format!("{name} stopped ({reason})")),
            SubagentEventKind::WorkspaceRemoved => Some(format!("{name} worktree removed")),
            _ => None,
        };
        if let Some(text) = text {
            // (The feed has the worker's summary.)
            self.setup
                .output
                .echo(&self.setup.output.dim(&format!("  ↳ {text}")));
        }
        if let Some(feed) = &self.setup.feed {
            self.tell_feed(feed, &event.handle, event.run_id, &event.kind);
        }
        let _ = self.wake.send(());
        true
    }

    /// The worker `handle` (run `run`) as a notice names it: `r2`, or `r2
    /// (under r1)` when a worker started it.
    fn named(&self, handle: &str, run: Uuid) -> String {
        let Some(workflow) = self.workflow() else {
            return handle.to_string();
        };
        named_in(&workflow.subagents(), handle, run)
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
            feed.worker_info(handle, self.worker_info(workflow, record, false));
        }
        let task = record.map(|record| record.task.clone()).unwrap_or_default();
        let ended = matches!(
            change,
            WorkerChange::Finished { .. }
                | WorkerChange::Failed { .. }
                | WorkerChange::Stopped { .. }
        );
        // Its prompt cache from its latest request on, once it ended, which
        // a run that starts again renews. An end is told with its cache in
        // place, so no client sees it ended without one. Its end seals its
        // record as its run's last.
        if ended
            && let Some(result) = workflow
                .as_ref()
                .and_then(|workflow| workflow.agent_result(run))
        {
            self.requests.seal(run, result.info().model_calls);
        }
        let cache = record.filter(|_| ended).and_then(|record| {
            cache_of(&self.models, &self.requests, record, self.setup.openai_auth)
        });
        if ended {
            feed.worker_cache(handle, cache.as_ref());
        }
        feed.worker(handle, &task, change);
        if !ended {
            feed.worker_cache(handle, None);
        }
        if ended {
            self.save_worker_figures();
        }
    }

    /// What a worker's summary says of it: the setup it started with, its
    /// depth, and (`with_costs`) what its saved runs cost, for a session that
    /// resumes. The costs of a worker that starts add up from its events. Who
    /// started it is told with `Feed::worker_under`.
    fn worker_info(
        &self,
        workflow: &Workflow,
        run: &arko_workflows::subagents::SubagentRecord,
        with_costs: bool,
    ) -> remote::WorkerInfo {
        let setup = self.run_model(run.run_id);
        let costs = with_costs.then(|| {
            let results = workflow.context().agent_results.read();
            // Each run's usage added up, without reading its task. A run
            // whose calls mixed plan and metered billing counts what it was
            // billed at list prices too.
            results
                .iter()
                .filter(|result| result.agent_run_id == run.run_id)
                .map(|result| &result.info().usage)
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
        info.nested = setup.as_ref().is_some_and(WorkerSetup::nests);
        info.skills = setup.as_ref().and_then(|setup| setup.skills.clone());
        info.mcp = setup.as_ref().and_then(|setup| setup.mcp.clone());
        info.profile = setup.and_then(|setup| setup.profile);
        info.depth = run.depth;
        info.costs = costs;
        info
    }

    /// Keeps how long each worker ran and how many calls it made with the
    /// session, to go on from when it opens again.
    fn save_worker_figures(&self) {
        let Some(feed) = &self.setup.feed else {
            return;
        };
        self.keep(RUN_TIMES_FILE, feed.worker_run_times());
        self.keep(TOOL_CALLS_FILE, feed.worker_tool_calls());
    }

    /// Writes `figures`, one for each worker, to `file` in the session's
    /// folder.
    fn keep<T: serde::Serialize>(&self, file: &str, figures: Vec<(String, T)>) {
        let figures: BTreeMap<String, T> = figures.into_iter().collect();
        let written = serde_json::to_vec_pretty(&figures)
            .map_err(std::io::Error::other)
            .and_then(|text| {
                crate::session::write_whole(&self.setup.session_dir.join(file), &text)
            });
        if let Err(error) = written {
            log::warn!("could not keep {file} ({error})");
        }
    }

    pub(crate) fn workflow(&self) -> Option<Arc<Workflow>> {
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
    /// The workers running, each with the ID of the worker that started it
    /// (none for the main agent's).
    pub fn running_with_parents(&self) -> Vec<(String, Option<String>)> {
        let Some(workflow) = self.workflow() else {
            return Vec::new();
        };
        // (The active ones first, so a worker that starts in between is in
        // `runs` too.)
        let active = workflow.active_subagents();
        let runs = workflow.subagents();
        active
            .into_iter()
            .map(|run| (run.handle, started_by(&runs, run.run_id)))
            .collect()
    }
    /// The user's message to worker `to` (`r1`), which goes as its lead's
    /// message does: a running worker reads it at its next step, and one
    /// that ended goes on with it. With `copy`, the worker that started it
    /// (the main agent, for most) gets a copy with its next news, and
    /// without, nothing reaches it. Returns the worker's ID, or why the
    /// message can't go.
    pub fn message(&self, to: &str, text: &str, copy: bool) -> Result<String, String> {
        let workflow = self.workflow().ok_or("there are no workers")?;
        let runs = workflow.subagents();
        let run = runs
            .iter()
            .find(|run| run.handle.eq_ignore_ascii_case(to.trim()))
            .ok_or_else(|| format!("there is no worker {}", to.trim()))?;
        let id = run.handle.clone();
        // A worker that started it reads the copy as news about its own
        // worker, and is told so. (An empty heading sends no copy.)
        let copy = if !copy {
            String::new()
        } else if runs.iter().any(|lead| lead.run_id == run.caller_run_id) {
            format!(
                "The user sent {id} a message directly (a copy for you, since you started {id}):"
            )
        } else {
            format!("The user sent {id} a message directly (a copy for you):")
        };
        workflow.message_subagent(&id, text, USER_MESSAGE_HEADING, &copy)?;
        Ok(id)
    }

    /// The prompt cache of worker `handle`, once it has ended, and what
    /// continuing it costs (see [`worker_cache`](crate::worker_cache)).
    pub fn worker_cache(&self, handle: &str) -> Option<crate::worker_cache::WorkerCache> {
        let workflow = self.workflow()?;
        let record = workflow
            .subagents()
            .into_iter()
            .find(|run| run.handle.eq_ignore_ascii_case(handle.trim()))?;
        cache_of(
            &self.models,
            &self.requests,
            &record,
            self.setup.openai_auth,
        )
    }

    /// Keeps a worker run's model request as its latest once it ended: when
    /// it went out, and its input. (The requests of Claude Code's own
    /// subagents aren't the worker's conversation.)
    pub(crate) fn note_request(&self, event: &arko_workflows::WorkflowEvent) {
        match &event.event {
            arko_agents::AgentEvent::ModelRequestStarted { subagent: None, .. }
                if self
                    .models
                    .lock()
                    .unwrap()
                    .runs
                    .contains_key(&event.agent_run_id) =>
            {
                self.requests
                    .started(event.agent_run_id, event.timestamp.timestamp_millis());
            }
            arko_agents::AgentEvent::ModelCallUsageReported(usage) if usage.subagent.is_none() => {
                self.requests.ended(event.agent_run_id, usage.input_tokens);
            }
            // A fresh conversation, which no cache holds yet.
            arko_agents::AgentEvent::ConversationHandedOver { .. }
            | arko_agents::AgentEvent::ConversationCompacted { .. } => {
                self.requests.forget(event.agent_run_id);
            }
            _ => {}
        }
    }

    /// Brings the record of each ended worker run in `runs` up to its task
    /// where the record may be behind it. That is a record its run's end
    /// didn't seal (a crash can leave one a request behind, and a session an
    /// older Arko saved has none), or one sealed with fewer model calls than
    /// the task has. The record keeps the larger input, its own or the
    /// task's latest. When the task went further, the task's end stands for
    /// its latest request, the nearest the task says. Each such task is read
    /// once, as the session opens, and its record sealed.
    fn reconcile_requests(&self, workflow: &Workflow, runs: &[SubagentRecord]) {
        let mut reconciled = Vec::new();
        for run in runs.iter().filter(|run| {
            !run.state.is_active() && self.models.lock().unwrap().runs.contains_key(&run.run_id)
        }) {
            let Some(result) = workflow.agent_result(run.run_id) else {
                continue;
            };
            let calls = result.info().model_calls;
            let kept = self.requests.get(run.run_id);
            if kept.is_some_and(|kept| kept.calls == Some(calls)) {
                continue;
            }
            let latest = result
                .task()
                .ok()
                .and_then(|task| task.current_conversation_context_size());
            let end = result
                .info()
                .end_time
                .unwrap_or(run.since)
                .timestamp_millis();
            let request = match (kept, latest) {
                (Some(kept), latest) => {
                    let newer = match kept.calls {
                        Some(sealed) => sealed < calls,
                        None => latest.is_some_and(|latest| latest != kept.input),
                    };
                    crate::worker_cache::LastRequest {
                        at: if newer { end.max(kept.at) } else { kept.at },
                        input: latest.map_or(kept.input, |latest| latest.max(kept.input)),
                        calls: Some(calls),
                    }
                }
                (None, Some(latest)) => crate::worker_cache::LastRequest {
                    at: end,
                    input: latest,
                    calls: Some(calls),
                },
                (None, None) => continue,
            };
            reconciled.push((run.run_id, request));
        }
        if !reconciled.is_empty() {
            self.requests.put(reconciled);
        }
    }

    /// Has worker `handle`'s prompt cache last `lifetime` after its run's
    /// last request, so a test sees it expire.
    #[cfg(test)]
    pub(crate) fn shorten_cache(&self, handle: &str, lifetime: Duration) {
        let workflow = self.workflow().unwrap();
        let record = workflow
            .subagents()
            .into_iter()
            .find(|run| run.handle == handle)
            .unwrap();
        self.models
            .lock()
            .unwrap()
            .runs
            .get_mut(&record.run_id)
            .unwrap()
            .llm_config
            .generation_config
            .prompt_cache_lifetime_seconds = Some(lifetime.as_secs());
        if let Some(feed) = &self.setup.feed {
            let cache = cache_of(
                &self.models,
                &self.requests,
                &record,
                self.setup.openai_auth,
            );
            feed.worker_cache(handle, cache.as_ref());
        }
    }

    /// Why a `message_agent` call with `params` doesn't continue its worker:
    /// the worker has ended, its cache expired and continuing it re-reads
    /// enough to matter, and the call didn't confirm it. `None` sends it.
    pub fn uncached_refusal(&self, params: &HashMap<String, Value>) -> Option<String> {
        let confirmed = match params.get(crate::worker_cache::CONFIRM_PARAM) {
            Some(Value::Bool(yes)) => *yes,
            Some(Value::String(text)) => text.trim().eq_ignore_ascii_case("true"),
            _ => false,
        };
        if confirmed {
            return None;
        }
        let to = params.get("to")?.as_str()?;
        let cache = self.worker_cache(to)?;
        let now = chrono::Utc::now();
        cache.asks(now).then(|| cache.refusal(now))
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
    /// Whether the news waiting for the lead holds mail that reached it
    /// before but not whole, and goes out again.
    pub fn has_resent_mail(&self) -> bool {
        let lead = *self.lead.lock().unwrap();
        lead.zip(self.workflow()).is_some_and(|(lead, workflow)| {
            workflow.pending_news(lead).iter().any(|item| item.resent)
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
                    && let Some(config) = workflow.context().agent_configs.get(&run.agent)
                    && let Some(Ok(long)) = run
                        .parameters
                        .get(CACHE_PARAM)
                        .and_then(Value::as_str)
                        .map(long_cache_asked)
                {
                    let model = &config.llm_config.model;
                    let mut generation = config.llm_config.generation_config.clone();
                    generation.long_prompt_cache = Some(long);
                    let chatgpt = model.provider == arko_ai::Provider::OpenAI
                        && self.setup.openai_auth != Some(crate::providers::OpenAiAuth::ApiKey);
                    lifetime =
                        arko_ai::prompt_cache::prompt_cache_lifetime(model, &generation, chatgpt);
                }
                let summary = self
                    .setup
                    .feed
                    .as_ref()
                    .and_then(|feed| feed.worker_summary(&run.handle));
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
                    skills: started.as_ref().and_then(|started| started.skills.clone()),
                    mcp: started.as_ref().and_then(|started| started.mcp.clone()),
                    parent: started_by(&runs, run.run_id),
                    depth: run.depth,
                    cost: summary
                        .as_ref()
                        .map(|summary| (summary.cost_billed, summary.cost_list)),
                    family_cost: summary
                        .as_ref()
                        .map(|summary| (summary.family_cost_billed, summary.family_cost_list)),
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
        in_family_order(facts)
    }

    /// The worker that started the worker `id`, when a worker did (and not
    /// the main agent).
    pub fn starter(&self, id: &str) -> Option<String> {
        let runs = self.workflow()?.subagents();
        let run = runs.iter().find(|run| run.handle == id)?;
        started_by(&runs, run.run_id)
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
            // How long they ran and what they called, the runs just stopped
            // too.
            self.save_worker_figures();
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
            read_only: true,
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
    use arko_ai::AnthropicCacheTtl;

    #[test]
    fn native_worker_options_preserve_cache_choices_and_queue_limits() {
        let temp = tempfile::tempdir().unwrap();
        let (crew, _) = Crew::new(Setup {
            session_id: "example".into(),
            session_dir: temp.path().into(),
            output: Arc::new(Output::new(None, true)),
            max_running: 2,
            cache_1h: true,
            openai_auth: Some(crate::providers::OpenAiAuth::ApiKey),
            cache_lifetime: PromptCacheLifetime::Known(Duration::from_secs(3600)),
            feed: None,
            refuse: None,
            asker: None,
            root: None,
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
        let ttl = |config: &AgentConfig| config.llm_config.generation_config.claude_cache_ttl();
        hook.prepare(&request, &mut config).unwrap();
        assert_eq!(ttl(&config), Some(AnthropicCacheTtl::OneHour));
        // `long` and `short`, and the names they had first.
        for (cache, expected) in [
            ("5m", None),
            ("1h", Some(AnthropicCacheTtl::OneHour)),
            ("short", None),
            ("Long", Some(AnthropicCacheTtl::OneHour)),
        ] {
            request.parameters.insert(CACHE_PARAM.into(), cache.into());
            hook.prepare(&request, &mut config).unwrap();
            assert_eq!(ttl(&config), expected, "{cache}");
        }
        request.parameters.insert(CACHE_PARAM.into(), "2h".into());
        let error = hook.prepare(&request, &mut config).unwrap_err();
        assert!(error.contains("long or short"), "{error}");
        // Claude Code chooses its own cache unless one is asked for, so the
        // short one is asked for outright.
        config.llm_config.model = crate::agents::resolve_model("cc:opus").unwrap();
        for (cache, expected) in [
            ("short", Some(AnthropicCacheTtl::FiveMinutes)),
            ("5m", Some(AnthropicCacheTtl::FiveMinutes)),
            ("long", Some(AnthropicCacheTtl::OneHour)),
        ] {
            request.parameters.insert(CACHE_PARAM.into(), cache.into());
            hook.prepare(&request, &mut config).unwrap();
            assert_eq!(ttl(&config), expected, "{cache}");
        }
        // On OpenAI the long cache is the 12-hour one.
        config.llm_config.model = crate::agents::resolve_model("sol").unwrap();
        for (cache, long) in [("long", Some(true)), ("1h", Some(true)), ("short", None)] {
            request.parameters.insert(CACHE_PARAM.into(), cache.into());
            hook.prepare(&request, &mut config).unwrap();
            assert_eq!(
                config.llm_config.generation_config.long_prompt_cache, long,
                "{cache}"
            );
        }
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
            openai_auth: None,
            cache_lifetime: PromptCacheLifetime::Known(Duration::from_secs(300)),
            feed: None,
            refuse: None,
            asker: None,
            root: None,
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
                billing: crate::billing::WorkerBilling::Api,
                api_budget: None,
                available: Default::default(),
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
            group: None,
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
        // What each runs is told to its starter, and nothing of a run that
        // was not recorded.
        assert_eq!(
            hook.describe(&record(&nests)).as_deref(),
            Some("runs gpt-6-luna at high effort, profile judge")
        );
        assert_eq!(
            hook.describe(&record(&plain)).as_deref(),
            Some("runs gpt-6-luna at high effort")
        );
        assert_eq!(hook.describe(&record(&start("r9", &[]))), None);

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

    /// A skills folder with the skills alpha, beta and gamma and one for the
    /// main agent alone, and a crew whose workers run with them and with
    /// the MCP servers github and sentry. Returns the crew and the
    /// registered worker's configuration.
    fn crew_with_skills(session: &Path, root: &Path, folder: &Path) -> (Arc<Crew>, AgentConfig) {
        for (name, extra) in [
            ("alpha", ""),
            ("beta", ""),
            ("gamma", ""),
            ("private", "workers: false\n"),
        ] {
            let dir = folder.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!(
                    "---\nname: {name}\ndescription: The {name} skill.\n{extra}---\n\nDo {name}.\n"
                ),
            )
            .unwrap();
        }
        let (crew, _) = crew_for(session, root, &luna());
        let folders = [folder.to_path_buf()];
        let worker = luna();
        let settings = crate::agents::AgentSettings {
            skill_folders: &folders,
            ..crate::agents::AgentSettings::for_test(&worker, &worker)
        };
        let registered = crate::agents::worker_config(&settings).unwrap();
        let mut new = crew.new_workers().unwrap();
        new.basis = Arc::new(chooser::Basis {
            available: chooser::Available {
                skills: crate::skills::config(&folders, settings.builtin_skills),
                workspace: root.into(),
            },
            worker_depth: 3,
            ..(*new.basis).clone()
        });
        crew.set_new_workers(new);
        crew.set_mcp_hub(servers_hub());
        (crew, registered)
    }

    /// A hub with the servers github and sentry, and one for the main agent
    /// alone, none of them started.
    fn servers_hub() -> Arc<arko_agents::McpHub> {
        let server = |name: &str, workers: bool| {
            let mut config = arko_agents::McpServerConfig::new(name, "true");
            config.workers = workers;
            config
        };
        arko_agents::McpHub::new(vec![
            server("github", true),
            server("sentry", true),
            server("internal", false),
        ])
    }

    /// The skills a configuration lets its run load, as the framework
    /// would filter them.
    fn skill_filter(config: &serde_json::Value) -> (bool, serde_json::Value) {
        let skills = &config["skills_config"];
        (
            skills["subagent"].as_bool().unwrap_or(false),
            skills["only"].clone(),
        )
    }

    /// A worker's skills and servers are worked out once, as it starts, and
    /// recorded. The run then gets the same ones every time it is resumed
    /// or continued, from the registered configuration and from another
    /// run's saved one, and after Arko restarts.
    #[test]
    fn a_recorded_run_gets_the_same_skills_and_servers_after_a_resume() {
        let temp = tempfile::tempdir().unwrap();
        let (session, root) = (temp.path().join("session"), temp.path().join("arko"));
        std::fs::create_dir_all(&session).unwrap();
        let skills = temp.path().join("skills");
        let (crew, registered) = crew_with_skills(&session, &root, &skills);
        let narrow = start("r1", &[("skills", "alpha, beta"), ("mcp", "github")]);
        let free = start("r2", &[]);
        let before = prepared(&crew, &narrow, &registered);
        let free_before = prepared(&crew, &free, &registered);
        // A worker never has the skills for the main agent alone, and a list
        // holds only what was asked.
        assert_eq!(
            skill_filter(&before),
            (true, serde_json::json!(["alpha", "beta"]))
        );
        assert_eq!(skill_filter(&free_before), (true, serde_json::Value::Null));
        assert_eq!(
            crew.mcp_servers_of(narrow.run_id),
            Some(vec!["github".to_string()])
        );
        // A worker with everything has the servers workers may reach, and
        // not the one for the main agent alone.
        assert_eq!(
            crew.mcp_servers_of(free.run_id),
            Some(vec!["github".to_string(), "sentry".to_string()])
        );
        // The main agent has no record, so all of its servers.
        assert_eq!(crew.mcp_servers_of(narrow.caller_run_id), None);

        // Another run's saved configuration, with a list of its own: each
        // run gets its own again.
        let saved: AgentConfig = serde_json::from_value(before.clone()).unwrap();
        assert_eq!(prepared(&crew, &narrow, &saved), before);
        assert_eq!(prepared(&crew, &free, &saved), free_before);
        let saved: AgentConfig = serde_json::from_value(free_before.clone()).unwrap();
        assert_eq!(prepared(&crew, &narrow, &saved), before);
        assert_eq!(prepared(&crew, &free, &saved), free_before);

        // A skill saved or deleted since, and the user's changes to the
        // folders, change nothing for the run.
        std::fs::remove_dir_all(skills.join("alpha")).unwrap();
        assert_eq!(prepared(&crew, &narrow, &registered), before);

        // Arko restarts, before and after the workflow is attached.
        let (again, registered) = crew_with_skills(&session, &root, &skills);
        again.models.lock().unwrap().attached = false;
        assert_eq!(prepared(&again, &narrow, &registered), before);
        again.models.lock().unwrap().attached = true;
        assert_eq!(prepared(&again, &narrow, &registered), before);
        assert_eq!(
            again.mcp_servers_of(narrow.run_id),
            Some(vec!["github".to_string()])
        );

        // A record from before skills were narrowed has everything.
        let old = std::fs::read_to_string(session.join(WORKER_MODELS_FILE)).unwrap();
        let mut old: serde_json::Value = serde_json::from_str(&old).unwrap();
        for setup in old.as_object_mut().unwrap().values_mut() {
            let setup = setup.as_object_mut().unwrap();
            setup.remove("skills");
            setup.remove("mcp");
        }
        std::fs::write(session.join(WORKER_MODELS_FILE), old.to_string()).unwrap();
        let (older, registered) = crew_with_skills(&session, &root, &skills);
        assert_eq!(
            skill_filter(&prepared(&older, &narrow, &registered)),
            (true, serde_json::Value::Null)
        );
        assert_eq!(
            older.mcp_servers_of(narrow.run_id),
            Some(vec!["github".to_string(), "sentry".to_string()])
        );
    }

    /// A worker on another model than the session's worker model lists the
    /// skills for its own model, and gets the same names again when it
    /// resumes. A run recorded before the names were kept keeps those of
    /// its configuration, so it resumes as it was saved.
    #[test]
    fn a_worker_lists_the_skills_of_its_own_model() {
        let temp = tempfile::tempdir().unwrap();
        let (session, root) = (temp.path().join("session"), temp.path().join("arko"));
        std::fs::create_dir_all(&session).unwrap();
        let skills = temp.path().join("skills");
        let (crew, registered) = crew_with_skills(&session, &root, &skills);
        std::fs::create_dir_all(skills.join("quirks")).unwrap();
        std::fs::write(
            skills.join("quirks/SKILL.md"),
            "---\nname: quirks\ndescription: Haiku's quirks.\nmodels: haiku\n---\n\nMind them.\n",
        )
        .unwrap();
        let on_haiku = start("r1", &[("model", "haiku")]);
        let on_luna = start("r2", &[]);
        let haiku = prepared(&crew, &on_haiku, &registered);
        let luna = prepared(&crew, &on_luna, &registered);
        assert_eq!(
            haiku["skills_config"]["model_names"],
            serde_json::json!(["claude-haiku-5-5", "haiku"])
        );
        assert_eq!(
            luna["skills_config"]["model_names"],
            serde_json::json!(["gpt-6-luna", "luna"])
        );
        let listed = |config: &serde_json::Value| -> Vec<String> {
            let config: AgentConfig = serde_json::from_value(config.clone()).unwrap();
            arko_agents::SkillManager::load(config.skills_config.as_ref().unwrap(), &root)
                .unwrap()
                .prompt_context()
                .on_demand_skills
                .iter()
                .map(|skill| skill.name.clone())
                .collect()
        };
        assert!(listed(&haiku).contains(&"quirks".to_string()));
        assert!(!listed(&luna).contains(&"quirks".to_string()));
        // load_skill is there for a skill saved later, except for a worker
        // given none.
        assert_eq!(luna["skills_config"]["offer_when_empty"], true);
        let none = prepared(&crew, &start("r3", &[("skills", "none")]), &registered);
        assert_eq!(
            none["skills_config"]["offer_when_empty"],
            serde_json::Value::Null
        );
        // Each gets its own again, also from the other's saved
        // configuration.
        let saved: AgentConfig = serde_json::from_value(haiku.clone()).unwrap();
        assert_eq!(prepared(&crew, &on_luna, &saved), luna);
        assert_eq!(prepared(&crew, &on_haiku, &registered), haiku);

        // A record without the names keeps the configuration's.
        let old = std::fs::read_to_string(session.join(WORKER_MODELS_FILE)).unwrap();
        let mut old: serde_json::Value = serde_json::from_str(&old).unwrap();
        for setup in old.as_object_mut().unwrap().values_mut() {
            setup.as_object_mut().unwrap().remove("model_names");
        }
        std::fs::write(session.join(WORKER_MODELS_FILE), old.to_string()).unwrap();
        let (older, registered) = crew_with_skills(&session, &root, &skills);
        let mut saved_before = registered.clone();
        saved_before.skills_config.as_mut().unwrap().model_names = Vec::new();
        let resumed = prepared(&older, &on_haiku, &saved_before);
        assert_eq!(
            resumed["skills_config"]["model_names"],
            serde_json::Value::Null
        );
        assert_eq!(
            prepared(&older, &on_haiku, &registered)["skills_config"]["model_names"],
            serde_json::json!(["gpt-6-luna", "luna"])
        );
    }

    /// The MCP tool is in a worker's configuration only when its setup adds
    /// it, which it does for a worker that reaches a server when it starts.
    /// A configuration another run was saved with doesn't leak it, and a run
    /// gets the same again when it resumes.
    #[test]
    fn a_worker_is_offered_the_mcp_tool_only_when_its_setup_adds_it() {
        let temp = tempfile::tempdir().unwrap();
        let (session, root) = (temp.path().join("session"), temp.path().join("arko"));
        std::fs::create_dir_all(&session).unwrap();
        let (crew, registered) = crew_with_skills(&session, &root, &temp.path().join("skills"));
        let mut carrying = registered.clone();
        carrying
            .tool_config
            .available_tool_names
            .push(arko_agents::mcp::MCP_TOOL_NAME.to_string());
        let offered = |config: &serde_json::Value| {
            config["tool_config"]["available_tool_names"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == arko_agents::mcp::MCP_TOOL_NAME)
        };
        let (all, some, none, only_main) = (
            start("r1", &[]),
            start("r2", &[("mcp", "sentry")]),
            start("r3", &[("mcp", "none")]),
            start("r4", &[("mcp", "[]")]),
        );
        for (request, expected) in [
            (&all, true),
            (&some, true),
            (&none, false),
            (&only_main, false),
        ] {
            let first = prepared(&crew, request, &registered);
            assert_eq!(offered(&first), expected, "{}", request.handle);
            // The same from a configuration that carries the tool, and
            // when the run is prepared again.
            assert_eq!(
                prepared(&crew, request, &carrying),
                first,
                "{}",
                request.handle
            );
            assert_eq!(prepared(&crew, request, &registered), first);
        }
        // Without a hub no worker reaches a server.
        let (bare, base) = crew_for(&session.join("bare"), &root, &luna());
        std::fs::create_dir_all(session.join("bare")).unwrap();
        let mut base_with_tool = base.clone();
        base_with_tool
            .tool_config
            .available_tool_names
            .push("mcp".into());
        assert!(!offered(&prepared(
            &bare,
            &start("r1", &[]),
            &base_with_tool
        )));
        assert_eq!(bare.mcp_servers_of(all.run_id), None);
    }

    /// The start hook takes a worker's starter from its record, so a
    /// worker started by a worker gets from its starter's set, also after
    /// the starter's setup is read back from the file.
    #[test]
    fn a_worker_started_by_a_worker_gets_from_its_starters_record() {
        let temp = tempfile::tempdir().unwrap();
        let (session, root) = (temp.path().join("session"), temp.path().join("arko"));
        std::fs::create_dir_all(&session).unwrap();
        let (crew, registered) = crew_with_skills(&session, &root, &temp.path().join("skills"));
        let r1 = start("r1", &[("skills", "alpha, beta"), ("mcp", "github")]);
        prepared(&crew, &r1, &registered);
        let under = |handle: &str, starter: &StartRequest, parameters: &[(&str, &str)]| {
            let mut request = start(handle, parameters);
            request.caller_run_id = starter.run_id;
            let hook = crew.config().hook.unwrap();
            let mut config = registered.clone();
            hook.prepare(&request, &mut config).map(|()| request)
        };
        // It inherits its starter's list, narrows it, and can't widen it.
        let r2 = under("r2", &r1, &[]).unwrap();
        assert_eq!(
            crew.run_model(r2.run_id).unwrap().skills,
            Some(vec!["alpha".to_string(), "beta".to_string()])
        );
        assert_eq!(
            crew.mcp_servers_of(r2.run_id),
            Some(vec!["github".to_string()])
        );
        let r3 = under("r3", &r2, &[("skills", "beta")]).unwrap();
        assert_eq!(
            crew.run_model(r3.run_id).unwrap().skills,
            Some(vec!["beta".to_string()])
        );
        let refused = under("r4", &r3, &[("skills", "alpha")]).unwrap_err();
        assert!(refused.ends_with("You can give beta."), "{refused}");
        let refused = under("r4", &r1, &[("mcp", "sentry")]).unwrap_err();
        assert!(refused.ends_with("You can give github."), "{refused}");
        // A refused start leaves no record.
        assert!(
            crew.run_model(
                under("r5", &r1, &[("skills", "gamma")]).map_or(Uuid::nil(), |r| r.run_id)
            )
            .is_none()
        );
    }

    /// What a start result and `list_agents` say of a worker's skills and
    /// servers: nothing when it has its starter's, else the narrowed sets
    /// and the names its profile asked for that it couldn't get.
    #[test]
    fn the_lists_say_the_narrowed_skills_and_servers() {
        let temp = tempfile::tempdir().unwrap();
        let (session, root) = (temp.path().join("session"), temp.path().join("arko"));
        std::fs::create_dir_all(&session).unwrap();
        judge(
            &root,
            "---\ndescription: Judges\nskills: beta, ghost\nmcp: none\n---\nJudge it.\n",
        );
        let (crew, registered) = crew_with_skills(&session, &root, &temp.path().join("skills"));
        let hook = crew.config().hook.unwrap();
        let record_of = |request: &StartRequest| {
            let mut record = record(&request.handle, request.caller_run_id);
            record.run_id = request.run_id;
            record
        };
        let said = |parameters: &[(&str, &str)]| {
            let request = start("r1", parameters);
            prepared(&crew, &request, &registered);
            hook.describe(&record_of(&request)).unwrap()
        };
        assert_eq!(said(&[]), "runs gpt-6-luna at high effort");
        assert_eq!(
            said(&[("skills", "alpha, beta")]),
            "runs gpt-6-luna at high effort, with only the skills alpha and beta"
        );
        assert_eq!(
            said(&[("skills", "none"), ("mcp", "github")]),
            "runs gpt-6-luna at high effort, with no skills and only the MCP server github"
        );
        assert_eq!(
            said(&[("mcp", "github, sentry")]),
            "runs gpt-6-luna at high effort, with only the MCP servers github and sentry"
        );
        assert_eq!(
            said(&[("profile", "judge")]),
            "runs gpt-6-luna at high effort, profile judge, with only the skill beta and no MCP servers. The profile also names the skill `ghost`, which this worker can't get."
        );
    }

    /// `/workers` puts each narrowed set on a line of its own, after what
    /// the worker runs.
    #[test]
    fn workers_lines_say_the_narrowed_skills_and_servers() {
        let mut worker = WorkerFacts {
            id: "r2".into(),
            state: "running".into(),
            task: "Judge the render".into(),
            model: Some("gpt-6.1-sol".into()),
            effort: Some("high".into()),
            profile: Some("3d-judge".into()),
            skills: None,
            mcp: None,
            parent: None,
            depth: 1,
            cost: None,
            family_cost: None,
            other_model: None,
            cache_lifetime: None,
            worktree: None,
            report: None,
        };
        let plain = describe_worker(&worker);
        assert_eq!(
            plain,
            "r2  running  Judge the render\n  runs gpt-6.1-sol at high effort, profile 3d-judge"
        );
        worker.skills = Some(vec!["ponytail".into(), "release-notes".into()]);
        worker.mcp = Some(Vec::new());
        assert_eq!(
            describe_worker(&worker),
            format!("{plain}\n  only the skills ponytail and release-notes\n  no MCP servers")
        );
        worker.skills = Some(vec!["ponytail".into()]);
        worker.mcp = Some(vec!["github".into(), "sentry".into(), "figma".into()]);
        worker.depth = 2;
        assert!(
            describe_worker(&worker).ends_with(
                "\n    only the skill ponytail\n    only the MCP servers github, sentry, and figma"
            ),
            "{}",
            describe_worker(&worker)
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
        // A record written now has the old fields, and the effort and the
        // model's names, which an older Arko ignores, when nothing else is
        // added.
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
        assert_eq!(keys, ["effort", "handover", "llm_config", "model_names"]);
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
        assert_eq!(
            names,
            [
                "cache", "profile", "model", "effort", "skills", "mcp", "nested"
            ]
        );
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
        assert_eq!(
            offered,
            [
                "cache", "profile", "model", "effort", "skills", "mcp", "nested"
            ]
        );
        // It is a flag, not text.
        let nested = &config.start_parameters[6];
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
            skills: None,
            mcp: None,
            parent: None,
            depth: 1,
            cost: Some((0.1234, 0.1234)),
            family_cost: Some((0.1234, 0.1234)),
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
        // What it cost with its workers shows once that is more than its own.
        worker.cost = Some((0.12, 0.12));
        worker.family_cost = Some((0.62, 0.62));
        assert!(
            describe_worker(&worker).contains("\n  cost $0.12\n  $0.62 with its workers"),
            "{}",
            describe_worker(&worker)
        );
        worker.family_cost = Some((0.0, 0.9));
        worker.cost = Some((0.0, 0.31));
        assert!(
            describe_worker(&worker).contains("\n  ≈ $0.90 with its workers"),
            "{}",
            describe_worker(&worker)
        );
        worker.family_cost = Some((0.3, 0.3));
        worker.cost = Some((0.3, 0.3));
        assert!(!describe_worker(&worker).contains("with its workers"));
        // A worker that a worker started says whose it is.
        worker.parent = Some("r1".into());
        worker.depth = 2;
        assert!(
            describe_worker(&worker)
                .starts_with("  r2  running  Judge the render\n    started by r1\n"),
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

    /// The four tools that let a worker start workers are the framework's,
    /// and `worker_options` and `save_worker_profile` stay with the main agent.
    #[test]
    fn the_start_tools_a_nested_worker_gets_are_the_four_of_the_framework() {
        assert_eq!(
            NESTED_TOOL_NAMES,
            ["start_agent", "list_agents", "stop_agent", "link_agents"]
        );
        for name in [WORKER_OPTIONS, SAVE_WORKER_PROFILE] {
            assert!(!NESTED_TOOL_NAMES.contains(&name));
            assert!(LEAD_TOOL_NAMES.contains(&name));
        }
    }

    /// A wait without ids waits for the runs of the agent that waits: the
    /// main agent's own, and not the workers of its workers, or the worker's.
    #[test]
    fn a_wait_without_ids_waits_for_the_runs_of_the_agent_that_waits() {
        let main = Uuid::new_v4();
        let (r1, r2) = (record("r1", main), record("r2", main));
        let r3 = record("r3", r1.run_id);
        let r4 = record("r4", r3.run_id);
        let all = [r1.clone(), r2.clone(), r3.clone(), r4.clone()];
        let active = |runs: &[&arko_workflows::subagents::SubagentRecord]| -> Vec<_> {
            runs.iter().map(|run| (*run).clone()).collect()
        };
        let running = active(&[&r1, &r2, &r3, &r4]);
        assert_eq!(waited_runs(&running, &all, None), ["r1", "r2"]);
        assert_eq!(waited_runs(&running, &all, Some(r1.run_id)), ["r3"]);
        assert_eq!(waited_runs(&running, &all, Some(r3.run_id)), ["r4"]);
        assert!(waited_runs(&running, &all, Some(r2.run_id)).is_empty());
        // Only those still running count.
        assert_eq!(waited_runs(&active(&[&r2, &r4]), &all, None), ["r2"]);
    }

    /// A notice names the worker that started a worker, and the lists put
    /// a worker's workers after it though they started later.
    #[test]
    fn notices_and_lists_follow_the_family() {
        let main = Uuid::new_v4();
        let (r1, r2) = (record("r1", main), record("r2", main));
        let r3 = record("r3", r1.run_id);
        let runs = [r1.clone(), r2.clone(), r3.clone()];
        assert_eq!(named_in(&runs, "r1", r1.run_id), "r1");
        assert_eq!(named_in(&runs, "r3", r3.run_id), "r3 (under r1)");
        let fact = |id: &str, parent: Option<&str>| WorkerFacts {
            id: id.into(),
            state: "running".into(),
            task: String::new(),
            model: None,
            effort: None,
            profile: None,
            skills: None,
            mcp: None,
            parent: parent.map(str::to_string),
            depth: 1,
            cost: None,
            family_cost: None,
            other_model: None,
            cache_lifetime: None,
            worktree: None,
            report: None,
        };
        let ids: Vec<String> = in_family_order(vec![
            fact("r1", None),
            fact("r2", None),
            fact("r3", Some("r1")),
        ])
        .into_iter()
        .map(|fact| fact.id)
        .collect();
        assert_eq!(ids, ["r1", "r3", "r2"]);
    }

    /// What `/stop-workers NAME` says: the workers that stop with the worker,
    /// and who can restart it, which is the worker that started it.
    #[test]
    fn stopping_a_worker_says_what_stops_with_it_and_who_resumes_it() {
        let with = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            stopping_words("r1", None, &[]),
            "Stopping r1. A message from the agent resumes it."
        );
        assert_eq!(
            stopping_words("r4", Some("r1"), &[]),
            "Stopping r4. A message from r1 resumes it."
        );
        assert_eq!(
            stopping_words("r1", None, &with(&["r2"])),
            "Stopping r1. Its worker r2 stops with it. A message from the agent resumes r1."
        );
        assert_eq!(
            stopping_words("r2", Some("r1"), &with(&["r3", "r4", "r5"])),
            "Stopping r2. Its workers r3, r4, and r5 stop with it. A message from r1 resumes r2."
        );
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

    /// A crew over `session_dir` whose new workers run luna on the ChatGPT
    /// login under `billing` and `budget`, sharing the day's spend in
    /// `root`, with someone to ask (`can_ask`). Its feed has the approval
    /// tray and what the user is told.
    fn billed_crew(
        session_dir: &Path,
        root: &Path,
        billing: billing::WorkerBilling,
        budget: Option<f64>,
        can_ask: bool,
    ) -> (Arc<Crew>, AgentConfig, remote::Feed) {
        std::fs::create_dir_all(session_dir).unwrap();
        let feed = remote::Feed::new(MAIN);
        let (crew, _) = Crew::new(Setup {
            session_id: session_dir.display().to_string(),
            session_dir: session_dir.into(),
            output: Arc::new(Output::new(None, true)),
            max_running: 4,
            cache_1h: false,
            openai_auth: None,
            cache_lifetime: PromptCacheLifetime::Known(Duration::from_secs(300)),
            feed: Some(feed.clone()),
            refuse: None,
            asker: Some(Arc::new(crate::inputs::Asker {
                questions: None,
                feed: None,
                approvals: crate::approvals::Approvals::new(Some(feed.clone()), can_ask, None),
            })),
            root: Some(root.into()),
        });
        let worker = luna();
        let registered =
            crate::agents::worker_config(&crate::agents::AgentSettings::for_test(&worker, &worker))
                .unwrap();
        crew.set_new_workers(NewWorkers {
            model: WorkerSetup::of(&registered),
            cache_lifetime: PromptCacheLifetime::Known(Duration::from_secs(300)),
            basis: Arc::new(chooser::Basis {
                roots: crate::profiles::Roots::new(root).session(session_dir),
                worker,
                providers: crate::providers::Setup::new(None),
                thinking_text: true,
                cc_compact: true,
                handover_at: 200_000,
                choice: crate::agents::ModelChoice::Agent,
                worker_depth: 2,
                extra_rules: crate::agents::ExtraRules::Off,
                rules_text: None,
                billing,
                api_budget: budget,
                available: Default::default(),
            }),
        });
        crew.models.lock().unwrap().attached = true;
        (crew, registered, feed)
    }

    /// Starts a worker on `model` through `crew`'s start hook, on a thread of
    /// the runtime's, as `start_agent` does.
    fn start_on(
        crew: &Crew,
        base: &AgentConfig,
        model: &str,
    ) -> tokio::task::JoinHandle<Result<(), String>> {
        let hook = crew.config().hook.unwrap();
        let mut config = base.clone();
        let request = start("r1", &[(MODEL_PARAM, model)]);
        tokio::spawn(async move { hook.prepare(&request, &mut config) })
    }

    /// The first request in the approval tray of `feed`, in time.
    async fn asked(feed: &remote::Feed) -> crate::approvals::ApprovalRequest {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(request) = feed.approvals().into_iter().next() {
                    return request;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("asked in time")
    }

    /// Answers `request` in the tray of `feed`, as the tray's buttons do:
    /// `approve` with its option on (`option`), and the message box's
    /// `words`.
    fn answer(
        feed: &remote::Feed,
        request: &crate::approvals::ApprovalRequest,
        approve: bool,
        option: bool,
        words: Option<&str>,
    ) {
        let answered = feed.answer_approvals(&crate::approvals::ApprovalAnswers {
            choices: vec![crate::approvals::ApprovalChoice {
                id: request.id.clone(),
                approve,
                option,
            }],
            words: words.map(str::to_string),
        });
        assert_eq!(answered.len(), 1, "it waited");
    }

    /// The texts the feed shows.
    fn notices(feed: &remote::Feed) -> Vec<String> {
        feed.subscribe()
            .0
            .items
            .iter()
            .map(|item| {
                serde_json::to_value(item).unwrap()["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    /// Under Ask before API, a worker on a model billed per token waits for
    /// the user's approval in the tray, named with its model and its price.
    /// Approve starts it, and the next one asks again. Approve with the
    /// option on (Allow … for this session) starts it and the next ones
    /// without asking, also once the session is opened again. Reject, with
    /// or without words of the user's own, and a stopped turn refuse it, in
    /// words the agent can act on. A plan's model never asks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_worker_billed_per_token_starts_once_the_user_allows_it() {
        let temp = tempfile::tempdir().unwrap();
        let (session, root) = (temp.path().join("session"), temp.path().join("arko"));
        let (crew, base, feed) =
            billed_crew(&session, &root, billing::WorkerBilling::Ask, None, true);

        let starting = start_on(&crew, &base, "opus");
        let request = asked(&feed).await;
        let approval = &request.approval;
        assert_eq!(
            approval.title(),
            "Start a worker on Opus 5.5, billed per token?"
        );
        let details: Vec<String> = approval
            .details()
            .into_iter()
            .map(|detail| detail.text)
            .collect();
        assert!(details[0].starts_with("It costs $"), "{details:?}");
        assert_eq!(details[1], "Today’s API spend is $0.");
        assert_eq!(details[2], "The task is “Check it”.");
        assert_eq!(
            approval.option().as_deref(),
            Some("Allow Opus 5.5 for this session")
        );
        answer(&feed, &request, true, false, None);
        assert_eq!(starting.await.unwrap(), Ok(()));
        assert!(feed.approvals().is_empty());

        // Once only: the next start asks again, and Reject refuses it.
        let starting = start_on(&crew, &base, "opus");
        answer(&feed, &asked(&feed).await, false, false, None);
        assert_eq!(
            starting.await.unwrap(),
            Err("The user denied a worker on Opus 5.5, which is billed per token, so it didn't start. Start it on a plan model, which worker_options lists, or ask the user what they want.".into())
        );

        // The words in the message box go back to the agent.
        let starting = start_on(&crew, &base, "opus");
        answer(&feed, &asked(&feed).await, false, false, Some("Use sonnet"));
        let why = starting.await.unwrap().unwrap_err();
        assert!(
            why.contains("and wrote “Use sonnet”. Act on what they wrote."),
            "{why}"
        );

        // A turn stopped while the request waits ends the wait, and the
        // request leaves the tray.
        let starting = start_on(&crew, &base, "opus");
        asked(&feed).await;
        feed.turn_stopping("Stopping the turn…");
        let why = tokio::time::timeout(Duration::from_secs(10), starting)
            .await
            .expect("the wait ended")
            .unwrap()
            .unwrap_err();
        assert!(why.starts_with("No answer came"), "{why}");
        assert!(feed.approvals().is_empty());
        feed.turn_started();

        // Allowed for the session: it starts, and so do the next ones, with
        // no request, also in the session opened again.
        let starting = start_on(&crew, &base, "opus");
        answer(&feed, &asked(&feed).await, true, true, None);
        assert_eq!(starting.await.unwrap(), Ok(()));
        assert_eq!(start_on(&crew, &base, "opus").await.unwrap(), Ok(()));
        let (again, base, more) =
            billed_crew(&session, &root, billing::WorkerBilling::Ask, None, true);
        assert_eq!(start_on(&again, &base, "opus").await.unwrap(), Ok(()));
        // Another model billed per token asks, and a plan's never does.
        let starting = start_on(&again, &base, "sonnet");
        answer(&more, &asked(&more).await, true, false, None);
        assert_eq!(starting.await.unwrap(), Ok(()));
        for plan in ["cc:sonnet", "sol"] {
            assert_eq!(start_on(&again, &base, plan).await.unwrap(), Ok(()));
        }
        assert!(feed.approvals().is_empty() && more.approvals().is_empty());
    }

    /// Plans only refuses a worker billed per token and names the plan
    /// models to use. API allowed starts it without asking. With no one to
    /// ask, Ask before API refuses it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn each_setting_refuses_asks_or_starts_a_worker_billed_per_token() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let (plans, base, asked_plans) = billed_crew(
            &temp.path().join("plans"),
            &root,
            billing::WorkerBilling::Plans,
            None,
            true,
        );
        assert_eq!(
            start_on(&plans, &base, "gemini").await.unwrap(),
            Err("The user lets workers run only on their plans (/worker-billing plans), and Gemini 3.1 Pro Preview is billed per token. Start it on a plan model instead. Those usable now are cc:opus, cc:fable, cc:sonnet, cc:haiku, sol, astra, terra, and luna.".into())
        );
        assert_eq!(start_on(&plans, &base, "cc:haiku").await.unwrap(), Ok(()));
        // The session's own worker model, luna, is on the ChatGPT plan here.
        assert_eq!(start_on(&plans, &base, "luna").await.unwrap(), Ok(()));
        let (api, base, asked_api) = billed_crew(
            &temp.path().join("api"),
            &root,
            billing::WorkerBilling::Api,
            None,
            true,
        );
        assert_eq!(start_on(&api, &base, "opus").await.unwrap(), Ok(()));
        assert!(asked_plans.approvals().is_empty() && asked_api.approvals().is_empty());
        let (alone, base, _) = billed_crew(
            &temp.path().join("alone"),
            &root,
            billing::WorkerBilling::Ask,
            None,
            false,
        );
        let why = start_on(&alone, &base, "opus").await.unwrap().unwrap_err();
        assert!(
            why.starts_with("No one can allow a worker billed per token in this session"),
            "{why}"
        );
        // The start's line says when a worker is billed per token.
        let hook = api.config().hook.unwrap();
        let records = api
            .models
            .lock()
            .unwrap()
            .runs
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let mut record = record("r1", Uuid::new_v4());
        record.run_id = records[0];
        let said = hook.describe(&record).unwrap();
        assert!(said.starts_with("runs claude-opus-5-5 at "), "{said}");
        assert!(said.contains(", billed per token (API, $"), "{said}");
        let plan_runs = plans
            .models
            .lock()
            .unwrap()
            .runs
            .keys()
            .copied()
            .collect::<Vec<_>>();
        record.run_id = plan_runs[0];
        let said = plans.config().hook.unwrap().describe(&record).unwrap();
        assert!(!said.contains("billed per token"), "{said}");
    }

    /// Once the day's API spend across the sessions reaches the budget, a
    /// worker billed per token is refused under API allowed, and the card
    /// says so under Ask before API, also for a model allowed for the
    /// session. The user is told once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_budget_reached_refuses_or_asks_and_is_told_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        std::fs::create_dir_all(&root).unwrap();
        // Another session billed $30 today.
        std::fs::write(
            root.join(billing::SPEND_FILE),
            serde_json::json!({
                "day": chrono::Local::now().date_naive().to_string(),
                "sessions": {"other": 30.0}
            })
            .to_string(),
        )
        .unwrap();
        let (api, base, feed) = billed_crew(
            &temp.path().join("api"),
            &root,
            billing::WorkerBilling::Api,
            Some(20.0),
            true,
        );
        let why = start_on(&api, &base, "opus").await.unwrap().unwrap_err();
        assert!(
            why.starts_with(
                "Today's API spend of $30 has reached the user's daily budget of $20 (/api-budget)"
            ),
            "{why}"
        );
        assert!(start_on(&api, &base, "opus").await.unwrap().is_err());
        assert_eq!(start_on(&api, &base, "cc:opus").await.unwrap(), Ok(()));
        let told = billing::budget_notice(billing::WorkerBilling::Api, 30.0, 20.0);
        assert_eq!(
            notices(&feed).iter().filter(|text| **text == told).count(),
            1,
            "{:?}",
            notices(&feed)
        );

        let (ask, base, tray) = billed_crew(
            &temp.path().join("ask"),
            &root,
            billing::WorkerBilling::Ask,
            Some(20.0),
            true,
        );
        ask.spending
            .allowed
            .add(&crate::providers::resolve_model("opus").unwrap());
        let starting = start_on(&ask, &base, "opus");
        let request = asked(&tray).await;
        assert!(
            request.approval.details().iter().any(|detail| detail.look
                == crate::approvals::Look::Warning
                && detail.text == "Today’s API spend of $30 has reached your daily budget of $20."),
            "{:?}",
            request.approval.details()
        );
        answer(&tray, &request, true, false, None);
        assert_eq!(starting.await.unwrap(), Ok(()));
        // Raised past the spend, the saved OK holds again.
        let mut new = ask.new_workers().unwrap();
        new.basis = Arc::new(chooser::Basis {
            api_budget: Some(50.0),
            ..(*new.basis).clone()
        });
        ask.set_new_workers(new);
        assert_eq!(start_on(&ask, &base, "opus").await.unwrap(), Ok(()));
        assert!(tray.approvals().is_empty());
    }

    /// Agent events for [`follow`]: those in `waiting` wait (only taking
    /// them reaches them), and `busy` comes each time the next is asked
    /// for, as from an agent that streams without a pause. Without `busy`,
    /// none comes.
    struct Agents {
        waiting: std::collections::VecDeque<arko_workflows::WorkflowEvent>,
        busy: Option<arko_workflows::WorkflowEvent>,
    }

    impl AgentEvents for Agents {
        async fn next(&mut self) -> Option<arko_workflows::WorkflowEvent> {
            match &self.busy {
                Some(event) => Some(event.clone()),
                None => std::future::pending().await,
            }
        }

        fn waiting(&self) -> usize {
            self.waiting.len()
        }

        fn take(&mut self) -> Option<arko_workflows::WorkflowEvent> {
            self.waiting.pop_front()
        }
    }

    fn agent_event(event: arko_agents::AgentEvent) -> arko_workflows::WorkflowEvent {
        arko_workflows::WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: "worker".into(),
            agent_run_id: Uuid::nil(),
            event,
        }
    }

    /// r1's end, as the workers' events say it.
    fn r1_finished() -> arko_workflows::subagents::SubagentEvent {
        arko_workflows::subagents::SubagentEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            run_id: Uuid::nil(),
            handle: "r1".into(),
            agent_name: "worker".into(),
            caller_run_id: Uuid::nil(),
            caller_agent: MAIN.into(),
            kind: SubagentEventKind::Finished {
                summary: "Done.".into(),
            },
        }
    }

    /// While an agent streams without a pause, a worker's end still gets
    /// its turn.
    #[tokio::test]
    async fn a_busy_agent_does_not_hold_up_the_workers_news() {
        let (sender, workers) = tokio::sync::broadcast::channel(16);
        sender.send(r1_finished()).unwrap();
        let agents = Agents {
            waiting: Default::default(),
            busy: Some(agent_event(
                arko_agents::AgentEvent::AssistantResponseChunk("more".into()),
            )),
        };
        let told = std::sync::atomic::AtomicBool::new(false);
        let followed = follow(
            agents,
            workers,
            |_| true,
            |received| {
                assert_eq!(received.unwrap().handle, "r1");
                told.store(true, std::sync::atomic::Ordering::SeqCst);
                false
            },
        );
        tokio::time::timeout(Duration::from_secs(10), followed)
            .await
            .expect("the worker's end was told");
        assert!(told.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// The agent events that wait when a worker's event comes are taken
    /// first, so a run's last request is in the record before its end.
    #[tokio::test]
    async fn the_requests_that_wait_go_before_a_workers_end() {
        let (sender, workers) = tokio::sync::broadcast::channel(16);
        let usage = agent_event(arko_agents::AgentEvent::TaskStreamEnd);
        let agents = Agents {
            waiting: [usage.clone(), usage].into(),
            busy: None,
        };
        sender.send(r1_finished()).unwrap();
        let seen = Mutex::new(Vec::new());
        let followed = follow(
            agents,
            workers,
            |_| {
                seen.lock().unwrap().push("request");
                true
            },
            |_| {
                seen.lock().unwrap().push("end");
                false
            },
        );
        tokio::time::timeout(Duration::from_secs(10), followed)
            .await
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), ["request", "request", "end"]);
    }
}

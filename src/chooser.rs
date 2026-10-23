//! Choosing a worker's setup as it starts. The call to `start_agent`
//! (`profile`, `model` and `effort`, and `cache`), the profile it names and
//! the session's worker model decide the worker's model, effort, prompt
//! cache, handover size and standing instructions, and the session's
//! `worker_models` setting says what the main agent may choose.
//!
//! For each of the model, the effort and the cache, a value in the call
//! wins, else the profile's, else the session's. The session's are the
//! worker model `/model worker` sets and its cache. An effort that the
//! chosen model doesn't take is refused and not changed, so the main agent
//! isn't told something other than what ran. Only an effort the call or the
//! profile's own model gave is held to that. One that comes from further
//! away (the session's, or a profile's with the call choosing another model)
//! becomes the nearest effort the model takes, as `/model worker MODEL`
//! does.
//!
//! Fast mode is on when the worker runs the session's own worker model and
//! the session has fast mode on, with a profile or without one, and for a
//! profile that says `fast: true`. A worker on another model is a deliberate
//! setup, so it is not fast unless its profile says so, because fast mode
//! costs more.
//!
//! The start hook ([`crate::crew`]) calls [`choose`] for each start and
//! records the result with the run, so the profile's file is read once.

use std::collections::BTreeMap;

use arko_ai::Model;
use arko_workflows::subagents::StartRequest;
use serde_json::Value;

use crate::agents::{self, Effort, ExtraRules, ModelChoice, ModelSettings};
use crate::crew::{
    CACHE_PARAM, EFFORT_PARAM, MAX_OWN_WORKERS, MODEL_PARAM, NESTED_PARAM, NESTED_TOOL_NAMES,
    PROFILE_PARAM, Refuse, WorkerSetup,
};
use crate::profiles::{self, Cache, Found};
use crate::providers;

/// What a worker's setup is chosen from: the session's settings as they
/// are now. The harness gives the crew a new one whenever they change.
#[derive(Debug, Clone)]
pub struct Basis {
    /// Where the profiles are.
    pub roots: profiles::Roots,
    /// The session's worker model, which a start with no choice runs.
    pub worker: ModelSettings,
    /// How the session reaches OpenAI models, and how Claude Code runs.
    pub providers: providers::Setup,
    /// The models are asked for the text of their thinking.
    pub thinking_text: bool,
    /// Claude Code compacts the conversation at the handover size.
    pub cc_compact: bool,
    /// The handover size when the model's settings give none.
    pub handover_at: usize,
    /// Who picks a worker's model.
    pub choice: ModelChoice,
    /// How many levels of workers the session allows (`/worker-depth`).
    pub worker_depth: u32,
    /// Which agents get the extra rules for small models, and their text
    /// (none when no rule applies to this session).
    pub extra_rules: ExtraRules,
    pub rules_text: Option<String>,
}

/// The profile a start names. The built-in `default` is the session's worker
/// model.
pub const DEFAULT_PROFILE: &str = "default";

/// Where in the tree of workers a start happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Family {
    /// How deep the new worker is, 1 for a worker of the main agent and 2
    /// for a worker of that worker.
    pub depth: u32,
    /// The workers its starter has running or queued, when the starter is a
    /// worker.
    pub live: usize,
}

impl Family {
    /// A start by the main agent.
    pub const MAIN: Self = Self { depth: 1, live: 0 };
}

/// What a call to `start_agent` asks for.
#[derive(Debug, Default, PartialEq)]
struct Call {
    profile: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    cache: Option<String>,
    nested: Option<bool>,
}

impl Call {
    fn of(parameters: &BTreeMap<String, Value>) -> Result<Self, String> {
        let text = |name: &str| -> Result<Option<String>, String> {
            match parameters.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(value)) => {
                    Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty()))
                }
                Some(_) => Err(format!("{name} must be text")),
            }
        };
        Ok(Self {
            profile: text(PROFILE_PARAM)?,
            model: text(MODEL_PARAM)?,
            effort: text(EFFORT_PARAM)?,
            cache: text(CACHE_PARAM)?,
            nested: match parameters.get(NESTED_PARAM) {
                None | Some(Value::Null) => None,
                Some(Value::Bool(nested)) => Some(*nested),
                Some(_) => return Err(format!("{NESTED_PARAM} must be true or false")),
            },
        })
    }
}

/// The setup of the worker `request` starts when the main agent starts it,
/// or why it can't start.
#[cfg(test)]
pub fn choose(
    basis: &Basis,
    request: &StartRequest,
    refuse: Option<&Refuse>,
) -> Result<WorkerSetup, String> {
    choose_in(basis, request, refuse, Family::MAIN)
}

/// The setup of the worker `request` starts at `family`, or why it can't
/// start.
pub fn choose_in(
    basis: &Basis,
    request: &StartRequest,
    refuse: Option<&Refuse>,
    family: Family,
) -> Result<WorkerSetup, String> {
    let chosen = choose_at(basis, request, refuse, family);
    if family.depth > 1 {
        return chosen.map_err(|why| for_a_worker(&why));
    }
    chosen
}

/// A refusal as a worker reads it. A worker that starts workers has no
/// `worker_options` and can't ask the user, so the texts that send the main
/// agent there, or to the user, send it to its lead instead.
pub(crate) fn for_a_worker(why: &str) -> String {
    why.replace(
        ", or choose a model that worker_options lists as usable now.",
        ", or ask your lead which model to use.",
    )
    .replace(
        " worker_options lists the models usable now.",
        " Ask your lead which model to use.",
    )
    .replace(
        " worker_options lists the profiles.",
        " Ask your lead which profile to use.",
    )
    .replace(
        " worker_options lists them.",
        " Ask your lead which one to use.",
    )
    .replace(
        ", or use a profile worker_options lists.",
        ", or use another profile.",
    )
    .replace(
        ", or ask the user to allow more levels.",
        ", or tell your lead you need more levels.",
    )
    .replace("Ask the user to ", "Tell your lead to ask the user to ")
}

fn choose_at(
    basis: &Basis,
    request: &StartRequest,
    refuse: Option<&Refuse>,
    family: Family,
) -> Result<WorkerSetup, String> {
    let call = Call::of(&request.parameters)?;
    // A worker starts workers only as deep as the user allows, and no more
    // than it can keep track of.
    if family.depth > 1 {
        let level = family.depth - 1;
        if level >= basis.worker_depth {
            return Err(format!(
                "Workers can't start workers here. The user allows {} of workers (/worker-depth), and you are at level {level}. Do the work yourself, or tell your lead what you need.",
                levels(basis.worker_depth)
            ));
        }
        if family.live >= MAX_OWN_WORKERS {
            return Err(format!(
                "You already have {MAX_OWN_WORKERS} workers running or queued, the most one worker can have. Wait for some of them with wait_for_agents, or stop one you don't need with stop_agent, then start the next."
            ));
        }
    }
    // Whether the new worker is told to start workers of its own, and may.
    let may_nest = family.depth < basis.worker_depth && basis.choice != ModelChoice::Session;
    if call.nested == Some(true) && !may_nest {
        return Err(if basis.choice == ModelChoice::Session {
            "The user has every worker run the session's worker model (/worker-models session), and no worker starts workers then. Start the worker without nested.".to_string()
        } else {
            format!(
                "The worker would be at level {}, where it can't start workers of its own, because the user allows {} of workers (/worker-depth). Start it without nested, or ask the user to allow more levels.",
                family.depth,
                levels(basis.worker_depth)
            )
        });
    }
    let named = call
        .profile
        .as_deref()
        .filter(|name| !name.eq_ignore_ascii_case(DEFAULT_PROFILE));
    match basis.choice {
        ModelChoice::Agent => {}
        ModelChoice::Profiles => {
            if call.model.is_some() || call.effort.is_some() {
                return Err("The user lets you start workers with a profile only (/worker-models profiles), so choose a profile and leave out model and effort. worker_options lists the profiles.".to_string());
            }
        }
        ModelChoice::Session => {
            if named.is_some() || call.model.is_some() || call.effort.is_some() {
                return Err("The user has every worker run the session's worker model (/worker-models session), so start the worker without a profile, model or effort.".to_string());
            }
        }
    }
    let found = named.map(|name| profile(basis, name)).transpose()?;
    let profile = found.as_ref().map(|found| &found.profile);
    let profile_name = profile.map(|profile| profile.name.as_str());

    // The model: the call's, else the profile's, else the session's.
    let (model_text, from_profile) = match (&call.model, profile.and_then(|p| p.model.as_ref())) {
        (Some(text), _) => (text.clone(), false),
        (None, Some(text)) => (text.clone(), true),
        (None, None) => (basis.worker.model.clone(), false),
    };
    let model = providers::resolve_model(&model_text).map_err(|why| {
        // (Without the aliases: worker_options lists the models to use.)
        let why = providers::briefly(&why);
        match profile_name {
            Some(name) if from_profile => format!(
                "The profile `{name}` can't start a worker. {why} worker_options lists the models usable now."
            ),
            _ => format!("{why} worker_options lists the models usable now."),
        }
    })?;
    let model_name = providers::model_name(&model);
    let same_model = providers::resolve_model(&basis.worker.model)
        .is_ok_and(|session| providers::model_name(&session) == model_name);

    // Credentials come first: a model that can't start needs no more said
    // about its effort.
    if let Some(why) = refuse.and_then(|refuse| blocking(|| refuse(&model))) {
        return Err(why);
    }

    let effort = effort_for(basis, &call, profile, &model, same_model)?;
    let fast = profile.is_some_and(|profile| profile.fast) || (same_model && basis.worker.fast);
    let cache_choice = cache_for(&call, profile)?;
    let cache_1h = cache_choice.unwrap_or(basis.worker.cache_1h);
    let worker = ModelSettings {
        model: model_name,
        effort,
        fast,
        cache_1h: cache_1h && providers::takes_cache_ttl(&model),
        handover_at: if same_model {
            basis.worker.handover_at
        } else {
            Some(agents::default_handover(
                &model,
                basis.providers.openai_auth,
            ))
        },
    };
    // Fast mode is checked as the settings are built: a model without it
    // says so.
    let mut setup = agents::worker_setup(
        &worker,
        &basis.providers,
        basis.thinking_text,
        basis.handover_at,
        basis.cc_compact,
    )
    .map_err(|why| match profile_name {
        Some(name) if fast => format!("The profile `{name}` asks for fast mode. {why}"),
        _ => why,
    })?;

    // Claude Code has its own cache unless 5 minutes is asked for outright.
    if cache_choice == Some(false) {
        providers::ask_five_minutes(&model, &mut setup.llm_config.generation_config);
    }

    // What goes into the run's first message: the profile's instructions,
    // what it needs to know to start workers of its own, and the extra
    // rules for a small model when the registered worker (the session's)
    // didn't get them.
    let nests = may_nest
        && call
            .nested
            .or(profile.map(|profile| profile.nested))
            .unwrap_or(false);
    let mut context: Vec<String> = Vec::new();
    if let Some(profile) = profile
        && !profile.instructions.trim().is_empty()
    {
        context.push(format!(
            "# Your profile ({})\n{}",
            profile.name,
            profile.instructions.trim()
        ));
    }
    if nests {
        context.push(agents::NESTED_INSTRUCTIONS.trim().to_string());
        setup.tools_added = NESTED_TOOL_NAMES
            .iter()
            .map(|name| name.to_string())
            .collect();
    }
    if let Some(rules) = &basis.rules_text
        && basis.extra_rules.for_model(&worker)
        && !basis.extra_rules.for_model(&basis.worker)
    {
        context.push(rules.trim().to_string());
    }
    setup.context_added = (!context.is_empty()).then(|| context.join("\n\n"));
    setup.profile = profile_name.map(str::to_string);
    setup.effort = (!takes_none(&model)).then_some(effort);
    Ok(setup)
}

/// "1 level" or "3 levels".
fn levels(count: u32) -> String {
    format!("{count} level{}", if count == 1 { "" } else { "s" })
}

/// The profiles in force as the main agent's instructions name them: each
/// name (a valid one, nothing a file could make up) with the model it names
/// when Arko knows that model.
pub fn named_profiles(roots: &profiles::Roots) -> Vec<agents::ProfileName> {
    profiles::list(roots)
        .active
        .into_iter()
        .map(|found| agents::ProfileName {
            model: found.profile.model.filter(|model| {
                // Only what a model's name is made of, so a file's own
                // text can't reach the agent's instructions this way.
                model.chars().count() <= 40
                    && model.chars().all(|c| {
                        c.is_ascii_alphanumeric()
                            || matches!(c, '.' | '_' | ':' | '/' | '~' | '+' | '-')
                    })
                    && providers::resolve_model(model).is_ok()
            }),
            name: found.profile.name,
        })
        .collect()
}

/// Runs `work`, which may block (Claude Code's login check runs a command),
/// so that it holds up no other task on the runtime thread where the
/// runtime can move them.
fn blocking<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

/// The profile `name`, or what the main agent is told about it.
fn profile(basis: &Basis, name: &str) -> Result<Found, String> {
    profiles::read(&basis.roots, name).map_err(|error| match error {
        profiles::ReadError::Missing => format!(
            "There is no worker profile `{name}`. The profiles are {}. worker_options lists them.",
            names(basis)
        ),
        profiles::ReadError::BadName(why) => {
            format!("{why} The profiles are {}.", names(basis))
        }
        profiles::ReadError::Unreadable(problem) => format!(
            "The worker profile `{name}` can't be used ({}, {}). Ask the user to fix the file, or use a profile worker_options lists.",
            problem.file.display(),
            problem.reason
        ),
    })
}

/// The names of the profiles in force, with `default`, as a sentence's list.
/// The first [`MAX_NAMED`] of them when there are many.
pub fn names(basis: &Basis) -> String {
    let mut names = vec![DEFAULT_PROFILE.to_string()];
    let listed = profiles::list(&basis.roots).active;
    let more = listed.len().saturating_sub(MAX_NAMED);
    names.extend(
        listed
            .into_iter()
            .take(MAX_NAMED)
            .map(|found| found.profile.name),
    );
    if more > 0 {
        return format!("{}, and {more} more", names.join(", "));
    }
    crate::and_list(&names)
}

/// The most profile names a refusal lists.
const MAX_NAMED: usize = 20;

/// Whether `model` doesn't reason, so takes no effort.
fn takes_none(model: &Model) -> bool {
    providers::efforts(model).is_empty()
}

/// The effort: the call's, else the profile's, else the session's. The
/// call's and the profile's own are held to what the model takes. The
/// session's stays as it is on the session's own worker model, whatever the
/// model takes, as the registered worker runs it.
fn effort_for(
    basis: &Basis,
    call: &Call,
    profile: Option<&profiles::Profile>,
    model: &Model,
    session_model: bool,
) -> Result<Effort, String> {
    let takes = providers::efforts(model);
    let check = |effort: Effort| -> Result<Effort, String> {
        if takes.is_empty() || takes.contains(&effort) {
            Ok(effort)
        } else {
            Err(format!(
                "{} takes {} effort, not {effort}.",
                providers::model_name(model),
                providers::efforts_text(model)
            ))
        }
    };
    if let Some(text) = &call.effort {
        return check(providers::parse_effort(text)?);
    }
    if let Some(effort) = profile.and_then(|profile| profile.effort) {
        // The profile's effort goes with the model it runs (its own, or the
        // session's). When the call chose another model, it is the nearest
        // effort that one takes.
        return if call.model.is_none() {
            check(effort).map_err(|why| {
                format!(
                    "The profile `{}` asks for {effort} effort. {why} Pass an effort of your own to start it anyway.",
                    profile.map_or("", |profile| profile.name.as_str())
                )
            })
        } else {
            Ok(providers::fit_effort(model, effort))
        };
    }
    // The session's own: as it is on its own model (the registered worker
    // runs it so), else the nearest one the model takes.
    Ok(if session_model {
        basis.worker.effort
    } else {
        providers::fit_effort(model, basis.worker.effort)
    })
}

/// The prompt cache the call or the profile asks for: `Some(true)` is the
/// 1-hour one.
fn cache_for(call: &Call, profile: Option<&profiles::Profile>) -> Result<Option<bool>, String> {
    match call.cache.as_deref() {
        Some("1h") => Ok(Some(true)),
        Some("5m") => Ok(Some(false)),
        Some(other) => Err(format!("cache is 5m or 1h, not {other}")),
        None => Ok(profile
            .and_then(|profile| profile.cache)
            .map(|cache| cache == Cache::OneHour)),
    }
}

#[cfg(test)]
mod tests;

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
    CACHE_PARAM, EFFORT_PARAM, MODEL_PARAM, NESTED_TOOL_NAMES, NESTED_WORKERS, PROFILE_PARAM,
    Refuse, WorkerSetup,
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
    /// Which agents get the extra rules for small models, and their text
    /// (none when no rule applies to this session).
    pub extra_rules: ExtraRules,
    pub rules_text: Option<String>,
}

/// The profile a start names. The built-in `default` is the session's worker
/// model.
pub const DEFAULT_PROFILE: &str = "default";

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
            nested: parameters.get("nested").and_then(Value::as_bool),
        })
    }
}

/// The setup of the worker `request` starts, or why it can't start.
pub fn choose(
    basis: &Basis,
    request: &StartRequest,
    refuse: Option<&Refuse>,
) -> Result<WorkerSetup, String> {
    let call = Call::of(&request.parameters)?;
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
    let model = providers::resolve_model(&model_text).map_err(|why| match profile_name {
        Some(name) if from_profile => format!("The profile `{name}` can't start a worker. {why}"),
        _ => format!("{why} worker_options lists the models usable now."),
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
    let cache_1h = cache_for(&call, profile)?.unwrap_or(basis.worker.cache_1h);
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

    // What goes into the run's first message: the profile's instructions,
    // and the extra rules for a small model when the registered worker (the
    // session's) didn't get them.
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
    if let Some(rules) = &basis.rules_text
        && basis.extra_rules.for_model(&worker)
        && !basis.extra_rules.for_model(&basis.worker)
    {
        context.push(rules.trim().to_string());
    }
    setup.context_added = (!context.is_empty()).then(|| context.join("\n\n"));
    setup.profile = profile_name.map(str::to_string);
    setup.effort = (!takes_none(&model)).then_some(effort);
    // A worker that nests gets the start tools. None yet.
    let nested = call.nested.or(profile.map(|profile| profile.nested));
    if NESTED_WORKERS && nested == Some(true) {
        setup.tools_added = NESTED_TOOL_NAMES
            .iter()
            .map(|name| name.to_string())
            .collect();
    }
    Ok(setup)
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
                providers::resolve_model(model).is_ok() && model.chars().count() <= 40
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
            "The worker profile `{name}` can't be used. {} worker_options lists the profiles you can use.",
            unusable(&problem.reason, &problem.file)
        ),
    })
}

/// The names of the profiles in force, with `default`, as a sentence's list.
pub fn names(basis: &Basis) -> String {
    let mut names = vec![DEFAULT_PROFILE.to_string()];
    names.extend(
        profiles::list(&basis.roots)
            .active
            .into_iter()
            .map(|found| found.profile.name),
    );
    crate::and_list(&names)
}

/// A profile file's problem as a sentence.
fn unusable(reason: &str, file: &std::path::Path) -> String {
    format!("{} can't be used ({reason}).", file.display())
}

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
            check(effort)
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

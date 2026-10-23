//! The main agent's two tools for choosing a worker's setup. `worker_options`
//! lists the profiles and the models workers can run on now, and
//! `save_worker_profile` keeps a setup under a name for `start_agent`'s
//! `profile`.
//!
//! The models change while Arko runs (a key added with `/keys`, a plan's
//! usage), so the agent reads them from a tool and not from its instructions,
//! which keeps them out of the prompt cache's prefix. A profile saved for
//! the session needs no approval. One saved for the project or for every
//! session is put to the user as a question first, as `ask_user` is.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use arko_agents::{
    TaskContext, Tool, ToolDefinition, ToolError, ToolParameterDefinition, ToolParameterType,
    ToolRecovery, ToolTypeDetails,
};
use arko_ai::{Model, Provider};
use arko_workflows::user_input::{UserInputHandler, UserInputRequest};
use async_trait::async_trait;
use serde_json::Value;

use crate::agents::{Effort, ModelChoice};
use crate::chooser::Basis;
use crate::crew::{Crew, SAVE_WORKER_PROFILE, WORKER_OPTIONS};
use crate::inputs::Asker;
use crate::keys::Keys;
use crate::profiles::{self, Author, Place, Profile};
use crate::providers;
use crate::remote::Level;
use crate::ui::Stats;

/// What both tools read: the crew (the profiles' places and the session's
/// worker settings), the keys, the plans' usage, and whom to ask.
struct Context {
    crew: Arc<Crew>,
    keys: Arc<Keys>,
    stats: Arc<Mutex<Stats>>,
    asker: Arc<Asker>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Context")
    }
}

/// The two tools, for the main agent.
pub fn tools(
    crew: Arc<Crew>,
    keys: Arc<Keys>,
    stats: Arc<Mutex<Stats>>,
    asker: Arc<Asker>,
) -> Vec<Arc<dyn Tool>> {
    let context = Arc::new(Context {
        crew,
        keys,
        stats,
        asker,
    });
    vec![
        Arc::new(WorkerOptions(context.clone())),
        Arc::new(SaveWorkerProfile(context)),
    ]
}

fn text_parameter(name: &str, description: &str, required: bool) -> ToolParameterDefinition {
    ToolParameterDefinition {
        name: name.into(),
        description: description.into(),
        param_type: ToolParameterType::String,
        required,
        is_path: false,
        flag: None,
        default_value: None,
    }
}

/// What the main agent reads when it can't use a tool.
fn refused(why: impl Into<String>) -> ToolError {
    ToolError::ExecutionFailed(why.into())
}

/// The text parameter `key`, trimmed. `None` when it is left out, null or
/// blank. A value of another type is refused, so a wrong guess is not read
/// as the default.
fn optional_text<'a>(
    params: &'a HashMap<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, ToolError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.trim()).filter(|value| !value.is_empty())),
        Some(_) => Err(refused(format!("{key} must be text"))),
    }
}

#[derive(Debug)]
struct WorkerOptions(Arc<Context>);

#[async_trait]
impl Tool for WorkerOptions {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: WORKER_OPTIONS.into(),
            description: "Lists the worker profiles and the models usable now, with their efforts, prices and handover sizes.".into(),
            output_based_example: None,
            parameters: vec![text_parameter(
                "show",
                "profiles, models or all (default all).",
                false,
            )],
            parallel_safe: true,
            tool_type_details: ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: None,
        }
    }

    fn recovery_mode(&self) -> ToolRecovery {
        ToolRecovery::Rerun
    }

    async fn execute(
        &self,
        _context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let show = match optional_text(&params, "show")? {
            None | Some("all") => Show::All,
            Some("profiles") => Show::Profiles,
            Some("models") => Show::Models,
            Some(other) => {
                return Err(refused(format!(
                    "show is profiles, models or all, not {other}"
                )));
            }
        };
        let basis = self
            .0
            .crew
            .new_workers()
            .ok_or_else(|| refused("This session has no workers."))?
            .basis;
        let plans = crate::lock(&self.0.stats).plans.clone();
        let keys = self.0.keys.clone();
        // (Claude Code's login check may run a command.)
        let text = tokio::task::spawn_blocking(move || {
            options_text(&basis, show, &|model| keys.needs(model), &plans)
        })
        .await
        .map_err(|error| refused(format!("Couldn't list the options ({error})")))?;
        Ok(text)
    }
}

/// What `worker_options` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Show {
    Profiles,
    Models,
    All,
}

/// The text `worker_options` gives: the profiles in force, those that can't
/// be read, and the models usable now with those that aren't. `needs` says
/// what a model needs that the session doesn't have, and `plans` is each
/// plan's latest usage.
fn options_text(
    basis: &Basis,
    show: Show,
    needs: &dyn Fn(&Model) -> Option<String>,
    plans: &BTreeMap<String, String>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    match basis.choice {
        ModelChoice::Agent => {}
        ModelChoice::Profiles => parts.push(
            "The user lets you start workers with a profile only (/worker-models profiles), so the models below are for reference. Leave out model and effort."
                .to_string(),
        ),
        ModelChoice::Session => parts.push(
            "The user has every worker run the session's worker model (/worker-models session), so start workers without a profile, model or effort."
                .to_string(),
        ),
    }
    if show != Show::Models {
        parts.push(profiles_text(basis, needs));
    }
    if show != Show::Profiles {
        parts.push(models_text(basis, needs, plans));
    }
    parts.join("\n\n")
}

/// `sol, high effort`, or `the worker model` when the profile names none.
pub(crate) fn runs_text(model: Option<&str>, effort: Option<Effort>) -> String {
    let model = model.unwrap_or("the worker model");
    match effort {
        Some(effort) => format!("{model}, {effort} effort"),
        None => model.to_string(),
    }
}

/// The most profiles `worker_options` lists, and the most that can't be
/// read. A project's folder can hold any number of files, and the agent
/// reads this text each time it asks.
const MAX_PROFILES_LISTED: usize = 40;
const MAX_UNREADABLE_LISTED: usize = 10;

fn profiles_text(basis: &Basis, needs: &dyn Fn(&Model) -> Option<String>) -> String {
    let listed = profiles::list(&basis.roots);
    let worker = &basis.worker;
    let reasons = providers::resolve_model(&worker.model)
        .map_or(true, |model| !providers::efforts(&model).is_empty());
    let mut text = String::from("Worker profiles (pass one as start_agent's profile)\n");
    text.push_str(&format!(
        "- default · {} · the session's worker model (/model worker)",
        runs_text(Some(&worker.model), reasons.then_some(worker.effort))
    ));
    for found in listed.active.iter().take(MAX_PROFILES_LISTED) {
        let profile = &found.profile;
        let mut details = runs_text(profile.model.as_deref(), profile.effort);
        if profile.fast {
            details.push_str(", fast");
        }
        if let Some(cache) = profile.cache {
            details.push_str(&format!(", {} cache", cache.as_str()));
        }
        // A worker started with it starts workers unless the call says not.
        if profile.nested {
            details.push_str(", can start workers");
        }
        let place = if profile.made_by_agent() {
            format!("{}, made by you", found.place.name())
        } else {
            found.place.name().to_string()
        };
        text.push_str(&format!(
            "\n- {} ({place}) · {details} · {}",
            profile.name, profile.description
        ));
        let blocked = found.note.clone().or_else(|| {
            let model = providers::resolve_model(profile.model.as_deref()?).ok()?;
            needs(&model).map(|needs| format!("needs {needs}"))
        });
        if let Some(blocked) = blocked {
            text.push_str(&format!(" · can't start yet, {blocked}"));
        }
    }
    if listed.active.len() > MAX_PROFILES_LISTED {
        text.push_str(&format!(
            "\n- and {} more, which start_agent takes by name",
            listed.active.len() - MAX_PROFILES_LISTED
        ));
    }
    if !listed.unreadable.is_empty() {
        text.push_str("\nCan't be read");
        for problem in listed.unreadable.iter().take(MAX_UNREADABLE_LISTED) {
            text.push_str(&format!(
                "\n- {} ({}) · {}",
                problem.name,
                problem.place.name(),
                problem.reason
            ));
        }
        if listed.unreadable.len() > MAX_UNREADABLE_LISTED {
            text.push_str(&format!(
                "\n- and {} more",
                listed.unreadable.len() - MAX_UNREADABLE_LISTED
            ));
        }
    }
    text
}

/// The efforts a model takes as a range: `low to max`.
fn efforts_range(model: &Model) -> String {
    const ORDER: [Effort; 5] = [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
    ];
    let takes = providers::efforts(model);
    let place = |effort: &Effort| ORDER.iter().position(|other| other == effort).unwrap_or(0);
    match takes.as_slice() {
        [] => "no effort".to_string(),
        [one] => one.to_string(),
        [first, .., last]
            if takes
                .windows(2)
                .all(|pair| place(&pair[1]) == place(&pair[0]) + 1) =>
        {
            format!("{first} to {last}")
        }
        _ => takes
            .iter()
            .map(Effort::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// `5`, `1.25`, `0.1`.
fn dollars(amount: f64) -> String {
    let text = format!("{amount:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `950000` as `950k`.
fn thousands(tokens: usize) -> String {
    format!("{}k", tokens / 1000)
}

fn model_line(
    model: &Model,
    alias: &str,
    basis: &Basis,
    plans: &BTreeMap<String, String>,
) -> String {
    let name = if alias.is_empty() {
        providers::model_name(model)
    } else {
        alias.to_string()
    };
    let chatgpt = model.provider == Provider::OpenAI
        && basis.providers.openai_auth != Some(providers::OpenAiAuth::ApiKey);
    let plan = |key: &str| {
        let usage = plans.get(key).cloned().unwrap_or_else(|| key.to_string());
        format!("{usage}, shared with the user's own use")
    };
    let cost = if model.provider == Provider::ClaudeCode {
        plan("Claude plan")
    } else if chatgpt {
        plan("ChatGPT plan")
    } else {
        let pricing = model.pricing.at(std::time::SystemTime::now());
        if pricing.input_cost_per_million == 0.0 && pricing.output_cost_per_million == 0.0 {
            "billed per token".to_string()
        } else {
            format!(
                "${} / ${}",
                dollars(pricing.input_cost_per_million),
                dollars(pricing.output_cost_per_million)
            )
        }
    };
    let mut handover = format!(
        "hands over at {}",
        thousands(crate::agents::default_handover(
            model,
            basis.providers.openai_auth
        ))
    );
    if model.provider == Provider::ClaudeCode && basis.cc_compact {
        handover.push_str(", Claude Code compacts");
    }
    let mut line = format!(
        "- {name} · efforts {} · {cost} · {handover}",
        efforts_range(model)
    );
    if providers::takes_cache_ttl(model) {
        line.push_str(" · cache 5m or 1h (1h costs twice the input price to write)");
    } else if chatgpt {
        line.push_str(" · cache 30m, fixed");
    } else if model.provider == Provider::ClaudeCode {
        line.push_str(" · cache 1h, fixed");
    }
    line
}

fn models_text(
    basis: &Basis,
    needs: &dyn Fn(&Model) -> Option<String>,
    plans: &BTreeMap<String, String>,
) -> String {
    let menu = providers::model_menu(
        |sample| {
            providers::resolve_model(sample)
                .ok()
                .and_then(|model| needs(&model))
        },
        basis.providers.openai_auth,
        &[basis.worker.model.as_str()],
    );
    let mut usable = String::from(
        "Models usable now (price per million input and output tokens, and the size where a worker hands over to a fresh conversation)",
    );
    let mut unusable = String::new();
    for group in &menu {
        if let Some(missing) = &group.needs {
            let names: Vec<String> = group
                .models
                .iter()
                .map(|(model, alias)| {
                    if alias.is_empty() {
                        model.clone()
                    } else {
                        alias.to_string()
                    }
                })
                .collect();
            let how = match missing.as_str() {
                "Claude Code signed in" => "The user signs in with `claude auth login`.",
                "Claude Code installed" => "The user installs Claude Code.",
                _ => "The user adds it with /keys.",
            };
            unusable.push_str(&format!(
                "\n- {} · needs {missing}. {how}",
                names.join(", ")
            ));
            continue;
        }
        for (model, alias) in &group.models {
            let Ok(resolved) = providers::resolve_model(model) else {
                continue;
            };
            usable.push('\n');
            usable.push_str(&model_line(&resolved, alias, basis, plans));
            if resolved.provider == Provider::OpenRouter {
                usable.push_str(
                    "\n- openrouter:<slug> · any model OpenRouter serves · billed per token · hands over at 100k unless Arko knows its limit",
                );
            }
        }
    }
    if !unusable.is_empty() {
        usable.push_str("\nNot usable now");
        usable.push_str(&unusable);
    }
    usable
}

#[derive(Debug)]
struct SaveWorkerProfile(Arc<Context>);

impl SaveWorkerProfile {
    /// The profile the call describes, checked.
    fn profile_of(params: &HashMap<String, Value>) -> Result<Profile, ToolError> {
        let text = |name: &str| -> Result<Option<String>, ToolError> {
            match params.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(value)) => {
                    Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty()))
                }
                Some(_) => Err(refused(format!("{name} must be text"))),
            }
        };
        let name = text("name")?.ok_or_else(|| refused("Give the profile a name."))?;
        let description = text("description")?.ok_or_else(|| {
            refused("A profile needs a description, one line saying what it is for.")
        })?;
        let mut profile = Profile::new(name, description);
        profile.model = text("model")?;
        profile.instructions = text("instructions")?.unwrap_or_default();
        if let Some(effort) = text("effort")? {
            profile.effort = Some(providers::parse_effort(&effort).map_err(refused)?);
        }
        profile.check().map_err(|why| {
            // A model Arko doesn't know is told where the right ones are.
            let unknown = profile
                .model
                .as_deref()
                .is_some_and(|model| providers::resolve_model(model).is_err());
            if unknown && profile.model_problem().as_deref() == Some(why.as_str()) {
                refused(format!("{why} worker_options lists the models usable now."))
            } else {
                refused(why)
            }
        })?;
        Ok(profile)
    }
}

/// What the credential check says, as a warning for the agent: workers on
/// the profile's model can't start until the user has done it.
fn needs_warning(needs: &str) -> String {
    match needs {
        "Claude Code signed in" => " Workers on it can't start until the user signs in to Claude Code with `claude auth login`.".to_string(),
        "Claude Code installed" => " Workers on it can't start until the user installs Claude Code.".to_string(),
        key => format!(" Workers on it can't start until the user adds {key} with /keys."),
    }
}

/// The answer cut to a length the agent reads in a line.
fn cut(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(chars).collect::<String>())
    }
}

#[async_trait]
impl Tool for SaveWorkerProfile {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SAVE_WORKER_PROFILE.into(),
            description: "Saves a worker profile that start_agent takes by name. It is kept with the project for later sessions, unless scope says session (this session only) or user (every project). The user is asked first, except for a session profile and a project profile in a folder git ignores.".into(),
            output_based_example: None,
            parameters: vec![
                text_parameter(
                    "name",
                    "Lowercase letters, digits and hyphens, up to 32 characters, such as 3d-judge.",
                    true,
                ),
                text_parameter(
                    "description",
                    "One line, up to 120 characters, saying what the profile is for.",
                    true,
                ),
                text_parameter(
                    "model",
                    "A model as start_agent takes it. Without one, the session's worker model.",
                    false,
                ),
                text_parameter(
                    "effort",
                    "low, medium, high, xhigh or max, as the model takes it.",
                    false,
                ),
                text_parameter(
                    "instructions",
                    "Standing instructions for every worker with this profile, up to 4000 characters. The task goes in start_agent's task.",
                    false,
                ),
                text_parameter(
                    "scope",
                    "project (the default, kept with the project), session (this session only) or user (every project).",
                    false,
                ),
            ],
            parallel_safe: false,
            tool_type_details: ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: None,
        }
    }

    /// The call waits for the user's answer, which no time limit cuts short.
    fn uses_host_timeout(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let new = self
            .0
            .crew
            .new_workers()
            .ok_or_else(|| refused("This session has no workers."))?;
        let basis = new.basis;
        match basis.choice {
            ModelChoice::Agent => {}
            ModelChoice::Profiles => {
                return Err(refused(
                    "The user keeps the worker profiles themselves (/worker-models profiles). Tell them the setup you want, and they can save it with /profiles add.",
                ));
            }
            ModelChoice::Session => {
                return Err(refused(
                    "The user has every worker run the session's worker model (/worker-models session), so a profile has no use here. Don't save one.",
                ));
            }
        }
        // Checked before the user is asked, so they are never asked about a
        // profile that can't work.
        let profile = Self::profile_of(&params)?;
        let place = match optional_text(&params, "scope")? {
            None => Place::Project,
            Some(scope) => Place::parse(scope).ok_or_else(|| {
                refused(format!("scope is session, project or user, not {scope}"))
            })?,
        };
        // A profile with an effort and no model runs on the session's worker
        // model, so the effort has to suit that one.
        if profile.model.is_none()
            && let Some(effort) = profile.effort
            && let Ok(model) = providers::resolve_model(&basis.worker.model)
            && let takes = providers::efforts(&model)
            && !takes.is_empty()
            && !takes.contains(&effort)
        {
            return Err(refused(format!(
                "{} takes {} effort, not {effort}, and a profile with no model runs on it. Give the profile a model, or an effort that {} takes.",
                providers::model_name(&model),
                providers::efforts_text(&model),
                providers::model_name(&model)
            )));
        }
        profiles::check_agent_save(&basis.roots, &profile.name).map_err(refused)?;
        if profiles::needs_approval(&basis.roots, place, &profile.name) {
            if !self.0.asker.can_ask() {
                return Err(refused(
                    "No one can approve it in this session. Save it for the session, or tell the user the setup you want.",
                ));
            }
            let question = profiles::approval_text(&profile, place);
            let answer = self
                .0
                .asker
                .respond(UserInputRequest {
                    workflow_run_id: uuid::Uuid::nil(),
                    agent_run_id: context.task_id,
                    agent_name: context.agent_name.to_string(),
                    questions: question,
                })
                .await
                .map_err(refused)?;
            if !profiles::is_approval(&answer) {
                // The user sees the answer didn't save it, since what they
                // meant by "yes please, but..." is only the agent's to read.
                self.0.crew.notice(
                    Level::Info,
                    "Nothing was saved. Answer yes to save the profile as it is. Any other answer goes to the agent to change it.",
                );
                let said = answer.trim();
                return Ok(if said.is_empty() {
                    format!(
                        "The user didn't save {} for {}. Change it and ask again, or save it for this session with scope session.",
                        profile.name,
                        place.reach()
                    )
                } else {
                    format!(
                        "The user didn't save {} for {} and answered \"{}\". Only a plain yes saves it. If they asked for a change, make it and ask again. If they meant yes, ask again and tell them to answer yes. Or save it for this session with scope session.",
                        profile.name,
                        place.reach(),
                        cut(said, 500)
                    )
                });
            }
        }
        let saved =
            profiles::save(&basis.roots, place, &profile, Author::Agent).map_err(refused)?;
        let mut notice = format!(
            "The main agent saved the worker profile {} for {}.",
            profile.name,
            place.reach()
        );
        if place != Place::Session {
            notice = format!("{notice} It is in {}.", saved.file.display());
        }
        self.0.crew.notice(Level::Info, &notice);
        let kept = match place {
            Place::Session => "for this session".to_string(),
            Place::Project => "for this project".to_string(),
            Place::User => "in the user's Arko folder, for every project".to_string(),
        };
        let mut result = format!(
            "Saved the worker profile {} {kept}. start_agent takes it by name.",
            profile.name
        );
        if let Some(nearer) = saved.hidden_by {
            result.push_str(&format!(
                " A profile of the same name for {} comes first, so start_agent takes that one instead.",
                nearer.reach()
            ));
        }
        let model = providers::resolve_model(
            profile
                .model
                .as_deref()
                .unwrap_or(basis.worker.model.as_str()),
        );
        if let Some(needs) = model.ok().and_then(|model| self.0.keys.needs(&model)) {
            result.push_str(&needs_warning(&needs));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;

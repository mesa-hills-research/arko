//! Worker billing: whether a worker may start on a model billed per token
//! (an API key, Gemini, OpenRouter) or only on the user's plans (the `cc:`
//! models on a Claude plan, OpenAI models on the ChatGPT login), and the
//! daily API budget. The session's `worker_billing` setting chooses
//! (`/worker-billing`, `--worker-billing`, `ARKO_WORKER_BILLING`), and the
//! start hook ([`crate::crew`]) holds each start to it with [`gate`].
//!
//! Under [`WorkerBilling::Ask`], the default, a start on a model billed per
//! token asks the user first, as Arko asks its own questions (a card with
//! [`ALLOW`], "Allow … for this session" and [`DENY`]), and the worker
//! starts only once they allow it. An OK for the session is kept in the
//! session's folder ([`Allowed`]), so that model isn't asked about again.
//!
//! The budget counts what was billed since local midnight, across the
//! sessions of an Arko folder: each session keeps its own day's spend, from
//! its cost ledger, in a file of the folder ([`DaySpend`]), and adds up the
//! others'. The main agent's calls count too, but only new workers are held
//! to it. Workers already running go on.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use arko_ai::Model;
use arko_workflows::budget::CostSource;
use arko_workflows::prelude::Workflow;
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::providers::{self, OpenAiAuth};

/// How workers may be paid for (the `worker_billing` setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "tui", derive(clap::ValueEnum))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum WorkerBilling {
    /// Workers run only on models a plan of the user's pays for. A start on
    /// a model billed per token is refused.
    Plans,
    /// A start on a model billed per token waits for the user's OK. New
    /// sessions start with this.
    #[default]
    Ask,
    /// Workers may run on models billed per token without asking.
    Api,
}

impl WorkerBilling {
    pub const ALL: [WorkerBilling; 3] =
        [WorkerBilling::Plans, WorkerBilling::Ask, WorkerBilling::Api];

    /// The name `/worker-billing` takes and shows.
    pub fn name(self) -> &'static str {
        match self {
            WorkerBilling::Plans => "plans",
            WorkerBilling::Ask => "ask",
            WorkerBilling::Api => "api",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|billing| billing.name().eq_ignore_ascii_case(text.trim()))
    }

    /// The choice as a settings page labels it.
    pub fn label(self) -> &'static str {
        match self {
            WorkerBilling::Plans => "Plans only",
            WorkerBilling::Ask => "Ask before API",
            WorkerBilling::Api => "API allowed",
        }
    }

    /// The command that sets the choice, such as `/worker-billing plans`.
    pub fn command(self) -> String {
        format!("{COMMAND} {}", self.name())
    }

    /// What the choice means, as a sentence every screen shows under it.
    pub fn about(self) -> &'static str {
        match self {
            WorkerBilling::Plans => {
                "Workers run only on your plans, the cc: models and the ChatGPT login."
            }
            WorkerBilling::Ask => {
                "A worker on a model billed per token starts only once you allow it."
            }
            WorkerBilling::Api => "Workers may run on models billed per token without asking.",
        }
    }
}

/// The setting's name, as /settings and the web page call it.
pub const NAME: &str = "Worker billing";

/// What the setting does, before what each value does
/// ([`WorkerBilling::about`]).
pub const ABOUT: &str = "Whether workers may run on models billed per token.";

/// The command that sets it, which takes `plans`, `ask` or `api`.
pub const COMMAND: &str = "/worker-billing";

/// The daily API budget's name.
pub const BUDGET_NAME: &str = "Daily API budget";

/// The command that sets it, which takes `off`, `on` (for
/// [`DEFAULT_BUDGET`]) or an amount in dollars.
pub const BUDGET_COMMAND: &str = "/api-budget";

/// The budget `on` gives, in dollars.
pub const DEFAULT_BUDGET: f64 = 10.0;

/// The command that turns the budget on (at [`DEFAULT_BUDGET`]) or off.
pub fn budget_command(on: bool) -> String {
    format!("{BUDGET_COMMAND} {}", if on { "on" } else { "off" })
}

/// The line under the budget setting, with `budget` as it is.
pub fn budget_about(budget: Option<f64>) -> String {
    match budget {
        None => format!(
            "No daily limit. On, new workers on API models stop starting once {} is billed per token in a day, across your sessions.",
            money(DEFAULT_BUDGET)
        ),
        Some(budget) => format!(
            "Once {} is billed per token today, across your sessions, new workers on API models stop starting. Running workers go on.",
            money(budget)
        ),
    }
}

/// `budget` if a session can have it: a positive amount of dollars.
pub fn check_budget(budget: f64) -> Result<f64, String> {
    if budget.is_finite() && budget > 0.0 {
        Ok(budget)
    } else {
        Err(format!(
            "the daily API budget is an amount of dollars above 0, not {budget}"
        ))
    }
}

/// Reads `text` as `/api-budget` and `--api-budget` take it: `off`, `on`
/// for [`DEFAULT_BUDGET`], or dollars (`20`, `$7.50`).
pub fn parse_budget(text: &str) -> Result<Option<f64>, String> {
    match text.trim() {
        "off" => Ok(None),
        "on" => Ok(Some(DEFAULT_BUDGET)),
        amount => amount
            .trim_start_matches('$')
            .parse::<f64>()
            .ok()
            .and_then(|budget| check_budget(budget).ok())
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "`{amount}` isn't off, on or an amount of dollars, such as {BUDGET_COMMAND} 20."
                )
            }),
    }
}

/// An amount of dollars as people write it: `$20`, `$7.50`, `$0.25`.
pub fn money(amount: f64) -> String {
    let cents = (amount * 100.0).round();
    if cents % 100.0 == 0.0 {
        format!("${:.0}", cents / 100.0)
    } else {
        format!("${:.2}", cents / 100.0)
    }
}

/// A price per million tokens in dollars, as people write it: `5`,
/// `1.25`, `0.10`, `0.075`.
fn per_million(amount: f64) -> String {
    if amount.fract() == 0.0 {
        return format!("{amount:.0}");
    }
    let mut text = format!("{amount:.4}");
    while text.ends_with('0')
        && text
            .split_once('.')
            .is_some_and(|(_, cents)| cents.len() > 2)
    {
        text.pop();
    }
    text
}

/// `model`'s price per million input and output tokens, when Arko knows it.
fn prices(model: &Model) -> Option<(f64, f64)> {
    let pricing = model.pricing.at(std::time::SystemTime::now());
    let (input, output) = (
        pricing.input_cost_per_million,
        pricing.output_cost_per_million,
    );
    (input > 0.0 || output > 0.0).then_some((input, output))
}

/// How a model billed per token shows its billing beside its name: `API,
/// $5/$25` (per million input and output tokens), or `API` when its price
/// isn't known. A plan's model gets no label, since the lists say which
/// plan pays.
pub(crate) fn api_label(model: &Model) -> String {
    match prices(model) {
        Some((input, output)) => format!("API, ${}/${}", per_million(input), per_million(output)),
        None => "API".to_string(),
    }
}

/// How the model `name` (as `/model` takes it) is billed, for a model
/// picker beside its name, when the session reaches OpenAI models by
/// `openai_auth`: `API, $5/$25` (per million input and output tokens) for a
/// model billed per token, and `None` for one a plan pays for, which needs
/// no label beside the others.
pub fn label(name: &str, openai_auth: Option<OpenAiAuth>) -> Option<String> {
    let model = providers::resolve_model(name).ok()?;
    providers::plan_of(&model, openai_auth)
        .is_none()
        .then(|| api_label(&model))
}

/// What the start hook does with a start on a model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Gate {
    /// It starts.
    Go,
    /// The user is asked first. `over` is the day's spend and the budget it
    /// reached, when it has.
    Ask { over: Option<(f64, f64)> },
    /// It is refused, and why, for the agent that started it.
    Refuse(String),
}

/// What a start may do under `billing`, with the daily `budget`, on `model`
/// (reached by `openai_auth`). `nested` is a start by a worker, which can't
/// ask the user. `allowed` says the user allowed the model for the session.
/// `spent` is the day's API spend, asked only when a budget applies.
/// `plans` lists the plan models usable now, for a refusal.
#[allow(clippy::too_many_arguments)]
pub(crate) fn gate(
    billing: WorkerBilling,
    budget: Option<f64>,
    model: &Model,
    openai_auth: Option<OpenAiAuth>,
    nested: bool,
    allowed: bool,
    spent: impl FnOnce() -> f64,
    plans: impl FnOnce() -> Vec<String>,
) -> Gate {
    if providers::plan_of(model, openai_auth).is_some()
        || billing == WorkerBilling::Api && budget.is_none()
    {
        return Gate::Go;
    }
    let name = crate::models::display_name(&providers::model_name(model));
    let instead = |plans: Vec<String>| {
        match plans.as_slice() {
        [] => "No plan model is usable now, so ask the user to sign in to Claude Code or ChatGPT, or to change /worker-billing.".to_string(),
        models => format!(
            "Start it on a plan model instead. Those usable now are {}.",
            crate::and_list(models)
        ),
    }
    };
    if billing == WorkerBilling::Plans {
        return Gate::Refuse(format!(
            "The user lets workers run only on their plans (/worker-billing plans), and {name} is billed per token. {}",
            instead(plans())
        ));
    }
    let over = budget.and_then(|budget| {
        let spent = spent();
        (spent >= budget).then_some((spent, budget))
    });
    match (billing, over) {
        (WorkerBilling::Api, None) => Gate::Go,
        (WorkerBilling::Api, Some((spent, budget))) => Gate::Refuse(format!(
            "Today's API spend of {} has reached the user's daily budget of {} (/api-budget), so no worker starts on a model billed per token, such as {name}, until tomorrow. {}",
            money(spent),
            money(budget),
            instead(plans())
        )),
        (_, None) if allowed => Gate::Go,
        _ if nested => Gate::Refuse(format!(
            "A worker on {name} needs the user's OK, since it's billed per token, and only the main agent's starts can ask for it. {} You can also ask your lead.",
            instead(plans())
        )),
        (_, over) => Gate::Ask { over },
    }
}

/// The choice that allows one worker, and the one that refuses it.
pub(crate) const ALLOW: &str = "Allow";
pub(crate) const DENY: &str = "Deny";

/// The choice that allows the model `name` (its display name) for the
/// session.
pub(crate) fn allow_for_session(name: &str) -> String {
    format!("Allow {name} for this session")
}

/// The question that asks the user to allow a worker on `model` for `task`,
/// with the day's spend and the budget it reached (`over`), and its choices.
pub(crate) fn question(
    model: &Model,
    task: &str,
    over: Option<(f64, f64)>,
) -> (String, Vec<String>) {
    let name = crate::models::display_name(&providers::model_name(model));
    let mut text = format!("Start a worker on {name}, billed per token?");
    if let Some((input, output)) = prices(model) {
        text.push_str(&format!(
            " It costs ${} per million input tokens and ${} per million output tokens.",
            per_million(input),
            per_million(output)
        ));
    }
    if let Some((spent, budget)) = over {
        text.push_str(&format!(
            " Today’s API spend of {} has reached your daily budget of {}.",
            money(spent),
            money(budget)
        ));
    }
    let task = crate::ui::cut(task.trim(), 120);
    if !task.is_empty() {
        let end = if task.ends_with(['.', '!', '?', '…']) {
            ""
        } else {
            "."
        };
        text.push_str(&format!(" The task is “{task}”{end}"));
    }
    (
        text,
        vec![
            ALLOW.to_string(),
            allow_for_session(&name),
            DENY.to_string(),
        ],
    )
}

/// What the user decided about a worker billed per token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// This worker starts.
    Allow,
    /// It starts, and the model needs no OK again in the session.
    AllowForSession,
    /// It doesn't start, with what the user wrote, if anything.
    Deny(Option<String>),
    /// "You decide": the agent is left to choose, which means a plan model.
    LeftToAgent,
    /// "Not now".
    Dismissed,
    /// No answer came (the turn stopped), or no one can answer.
    Unanswered,
}

/// The decision the user's answer `words` makes about a worker on the model
/// `name` (its display name), as the answer panel, a message or "Not now"
/// gives it. Only a choice picked alone (or typed alone) allows, so words
/// beside it go back to the agent instead.
pub(crate) fn decision(words: &str, name: &str) -> Decision {
    let words = words.trim();
    let bare = words.trim_end_matches(['.', '!']).trim();
    if bare.is_empty() {
        Decision::Dismissed
    } else if bare.eq_ignore_ascii_case(ALLOW) {
        Decision::Allow
    } else if bare.eq_ignore_ascii_case(&allow_for_session(name)) {
        Decision::AllowForSession
    } else if bare.eq_ignore_ascii_case(DENY) {
        Decision::Deny(None)
    } else if bare == "You decide" || bare.starts_with("You decide (") {
        Decision::LeftToAgent
    } else {
        Decision::Deny(Some(words.to_string()))
    }
}

/// What the agent reads when the worker it started on `model` doesn't
/// start, after `decision`.
pub(crate) fn refusal(decision: &Decision, model: &Model) -> String {
    let name = crate::models::display_name(&providers::model_name(model));
    let plan = "Start it on a plan model, which worker_options lists";
    match decision {
        Decision::Allow | Decision::AllowForSession => String::new(),
        Decision::Deny(None) => format!(
            "The user denied a worker on {name}, which is billed per token, so it didn't start. {plan}, or ask the user what they want."
        ),
        Decision::Deny(Some(words)) => format!(
            "The user didn't allow a worker on {name}, which is billed per token, and wrote “{words}”. Act on what they wrote. A worker on a model billed per token asks them again."
        ),
        Decision::LeftToAgent => format!(
            "The user left the choice to you, so the worker didn't start on {name}, which is billed per token. {plan}."
        ),
        Decision::Dismissed => format!(
            "The user dismissed the question, so the worker didn't start on {name}, which is billed per token. {plan}, or ask the user."
        ),
        Decision::Unanswered => format!(
            "No answer came, so the worker didn't start on {name}, which is billed per token."
        ),
    }
}

/// What the agent reads when no one can allow a worker billed per token.
pub(crate) fn cant_ask(model: &Model) -> String {
    format!(
        "No one can allow a worker billed per token in this session (Arko runs without a terminal), so it didn't start on {}. Start it on a plan model, which worker_options lists. Running with --worker-billing api allows models billed per token.",
        crate::models::display_name(&providers::model_name(model))
    )
}

/// The models the user allowed for the session, kept in its folder.
pub(crate) struct Allowed {
    path: PathBuf,
    models: Mutex<BTreeSet<String>>,
}

/// The file in a session's folder that keeps the models allowed for it.
const ALLOWED_FILE: &str = "api-models.json";

impl Allowed {
    pub(crate) fn load(session_dir: &Path) -> Self {
        let path = session_dir.join(ALLOWED_FILE);
        let models = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            models: Mutex::new(models),
        }
    }

    /// Whether the user allowed `model` for the session.
    pub(crate) fn has(&self, model: &Model) -> bool {
        crate::lock(&self.models).contains(&providers::model_name(model))
    }

    /// Keeps the user's OK for `model`.
    pub(crate) fn add(&self, model: &Model) {
        let mut models = crate::lock(&self.models);
        models.insert(providers::model_name(model));
        let saved = serde_json::to_string_pretty(&*models)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                crate::session::write_whole(&self.path, text.as_bytes())
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = saved {
            log::warn!(
                "could not keep the models allowed in {} ({error})",
                self.path.display()
            );
        }
    }
}

/// The file in Arko's folder that keeps each session's API spend of the
/// day.
pub(crate) const SPEND_FILE: &str = "api-spend.json";

/// What [`SPEND_FILE`] holds: the day, and each session's spend on it.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SpendFile {
    day: String,
    #[serde(default)]
    sessions: BTreeMap<String, f64>,
}

/// The file's text `text` (empty or unreadable: none) with `session`'s spend
/// on `day` set to `amount`, and that day's spend across the sessions. A
/// file of another day is replaced.
fn recorded(text: &str, day: &str, session: &str, amount: f64) -> (String, f64) {
    let mut file = serde_json::from_str::<SpendFile>(text)
        .ok()
        .filter(|file| file.day == day)
        .unwrap_or_else(|| SpendFile {
            day: day.to_string(),
            sessions: BTreeMap::new(),
        });
    file.sessions.insert(session.to_string(), amount);
    let total = file.sessions.values().sum();
    (
        serde_json::to_string_pretty(&file).unwrap_or_default(),
        total,
    )
}

/// Local midnight at the start of `now`'s day, in UTC (the earlier one when
/// a clock change repeats it).
fn day_start(now: DateTime<Local>) -> DateTime<Utc> {
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|midnight| midnight.and_local_timezone(Local).earliest())
        .map_or_else(
            || now.with_timezone(&Utc),
            |start| start.with_timezone(&Utc),
        )
}

/// What the agents' calls in `entries` billed from `since` on.
fn billed_since(entries: &[arko_workflows::budget::CostEntry], since: DateTime<Utc>) -> f64 {
    entries
        .iter()
        .filter(|entry| {
            matches!(entry.source, CostSource::Agent { .. }) && entry.timestamp >= since
        })
        .map(|entry| entry.amount)
        .sum()
}

/// The day's API spend of a session, which it shares with the other
/// sessions of its Arko folder through [`SPEND_FILE`].
pub(crate) struct DaySpend {
    /// The file, none when the session keeps its spend to itself.
    file: Option<PathBuf>,
    session: String,
    state: Mutex<SpendState>,
}

#[derive(Default)]
struct SpendState {
    /// The day counted, and what the session's agents had billed when it
    /// began.
    day: Option<(NaiveDate, f64)>,
    /// The day shared last, the session's spend written to the file then,
    /// and the other sessions' as the file had it.
    shared: Option<NaiveDate>,
    written: f64,
    others: f64,
    /// The day the user was told the budget was reached.
    told: Option<NaiveDate>,
}

impl DaySpend {
    /// The day's spend of `session`, shared in Arko's folder `root` (none:
    /// kept to itself).
    pub(crate) fn new(root: Option<&Path>, session: &str) -> Self {
        Self {
            file: root.map(|root| root.join(SPEND_FILE)),
            session: session.to_string(),
            state: Mutex::default(),
        }
    }

    /// What the session's agents billed today, as `workflow` has it now.
    fn own(&self, workflow: &Workflow, state: &mut SpendState, now: DateTime<Local>) -> f64 {
        let total: f64 = workflow
            .cost_totals_by_agent()
            .values()
            .map(|totals| totals.billed)
            .sum();
        let today = now.date_naive();
        let start = match state.day {
            Some((day, start)) if day == today => start,
            // A new day: what was billed before it, from the ledger's times.
            _ => {
                let ledger = workflow.cost_ledger();
                let start = total - billed_since(&ledger.entries, day_start(now));
                state.day = Some((today, start));
                start
            }
        };
        (total - start).max(0.0)
    }

    /// The day's API spend across the folder's sessions, with this one's
    /// from `workflow` now (none: nothing yet). This session's is written to the file when it
    /// changed, so the others count it, and the others' are read then.
    /// `force` reads them (and writes) also when it didn't change.
    pub(crate) fn today(&self, workflow: Option<&Workflow>, force: bool) -> f64 {
        let now = Local::now();
        let mut state = crate::lock(&self.state);
        let own = workflow.map_or(0.0, |workflow| self.own(workflow, &mut state, now));
        let Some(file) = &self.file else {
            return own;
        };
        // (A new day starts from nothing.)
        if state.shared != Some(now.date_naive()) {
            state.shared = Some(now.date_naive());
            (state.written, state.others) = (0.0, 0.0);
        }
        if (state.written - own).abs() < 0.005 && !force {
            return own + state.others;
        }
        match share(file, &now.date_naive().to_string(), &self.session, own) {
            Ok(total) => {
                state.written = own;
                state.others = (total - own).max(0.0);
                own + state.others
            }
            Err(error) => {
                log::warn!(
                    "could not share the day's API spend in {} ({error})",
                    file.display()
                );
                own + state.others
            }
        }
    }

    /// Whether the user is to be told today that the budget was reached:
    /// true once a day.
    pub(crate) fn tell_once(&self) -> bool {
        let today = Local::now().date_naive();
        let mut state = crate::lock(&self.state);
        if state.told == Some(today) {
            return false;
        }
        state.told = Some(today);
        true
    }
}

/// Writes `session`'s spend `amount` on `day` in `file`, under its lock, and
/// returns that day's spend across the sessions.
fn share(file: &Path, day: &str, session: &str, amount: f64) -> Result<f64, String> {
    let failed = |error: std::io::Error| error.to_string();
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder).map_err(failed)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file.with_extension("json.lock"))
        .map_err(failed)?;
    lock.lock().map_err(failed)?;
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let (text, total) = recorded(&text, day, session, amount);
    crate::session::write_whole(file, text.as_bytes()).map_err(failed)?;
    Ok(total)
}

/// What the user is told once a day when the API spend reaches `budget`
/// under `billing`.
pub(crate) fn budget_notice(billing: WorkerBilling, spent: f64, budget: f64) -> String {
    let then = match billing {
        WorkerBilling::Ask => {
            "New workers on API models ask you first, also on models you allowed for the session."
        }
        _ => "New workers on API models don’t start until tomorrow.",
    };
    format!(
        "Today’s API spend of {} reached your daily budget of {}. {then} Running workers go on.",
        money(spent),
        money(budget)
    )
}

#[cfg(test)]
mod tests;

//! The session's figures for the status (`remote::Meter`), which the full
//! screen and the web page read, and which an app gets as they change
//! ([`Gauges::publish`]).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, broadcast, watch};

use arko_workflows::prelude::*;

use crate::agents::{ExtraRules, WORKER};
use crate::crew::Crew;
use crate::lock;
use crate::ui::Stats;
use crate::{crew, handover, keys, outside, project, providers, remote, shell};

/// The session's figures for the remote page's status, read as the page
/// asks. The workflow and its worker view are refreshed on session resume.
pub(crate) struct Gauges {
    /// The agents' models, as the harness last published them.
    pub(crate) models: Mutex<remote::Models>,
    /// The conversations, as the harness last published them.
    pub(crate) conversations: Mutex<Vec<remote::ConversationEntry>>,
    pub(crate) workflow: Mutex<Option<Arc<Workflow>>>,
    pub(crate) crew: Mutex<Option<Arc<Crew>>>,
    pub(crate) stats: Arc<Mutex<Stats>>,
    pub(crate) outside: Arc<outside::Outside>,
    pub(crate) handover: Arc<handover::Handover>,
    /// The providers the session reaches, which a key added or removed
    /// changes.
    pub(crate) keys: Arc<keys::Keys>,
    /// The main agent ends its turns with a reply in plain text, as the
    /// harness last published it.
    pub(crate) text_replies: std::sync::atomic::AtomicBool,
    /// The agents on `cc:` models compact instead of handing over, as the
    /// harness last published it.
    pub(crate) cc_compact: std::sync::atomic::AtomicBool,
    /// Which agents get the extra rules for small models, as the harness
    /// last published it.
    pub(crate) extra_rules: Mutex<ExtraRules>,
    /// Which of the project's instruction files the agents get, and whether
    /// workers do, as the harness last published them.
    pub(crate) project: Mutex<(project::Files, bool)>,
    /// When the shell's commands move to the background, and those running.
    pub(crate) background: Arc<shell::Background>,
    /// The web page, while it is served.
    pub(crate) page: Mutex<Option<remote::Page>>,
    /// The session's name, as the harness last published it.
    pub(crate) title: Mutex<String>,
    /// Rings when something the status shows changed ([`touch`](Self::touch)).
    pub(crate) changed: Notify,
    /// The status as last published.
    pub(crate) published: Published,
}

/// The status as [`Gauges::publish`] last sent it.
pub(crate) struct Published(watch::Sender<Arc<remote::Reading>>);

impl Default for Published {
    fn default() -> Self {
        Self(watch::channel(Arc::default()).0)
    }
}

/// How long the publisher gathers changes before it reads the status, so a
/// burst of them (a streamed reply) makes one status, at most twice a
/// second.
const GATHER: Duration = Duration::from_millis(500);
/// How often the status is read while the session works (a turn, workers,
/// jobs): its time and costs move without a change being told.
const WHILE_WORKING: Duration = Duration::from_secs(1);

impl Gauges {
    /// The session closed: what it ran goes (its workflow holds the
    /// terminal, whose folder lock the session releases), while a front end
    /// may still hold the status.
    pub(crate) fn release(&self) {
        *lock(&self.workflow) = None;
        *lock(&self.crew) = None;
    }

    /// Something the status shows changed (a setting, the models, the
    /// conversations, the web page): the next status goes out soon.
    pub(crate) fn touch(&self) {
        self.changed.notify_one();
    }

    /// The status, as it changes.
    pub(crate) fn watch(&self) -> watch::Receiver<Arc<remote::Reading>> {
        self.published.0.subscribe()
    }

    /// Sends the status to [`watch`](Self::watch)ers whenever it changes:
    /// when something [touches](Self::touch) it or `feed` changes, and every
    /// second while the session works. A status like the last isn't sent
    /// again. Runs until it is aborted.
    pub(crate) fn publish(self: &Arc<Self>, feed: &remote::Feed) -> tokio::task::JoinHandle<()> {
        let gauges = Arc::downgrade(self);
        let feed = feed.clone();
        let (_, mut updates) = feed.subscribe();
        tokio::spawn(async move {
            let mut working = false;
            loop {
                let Some(this) = gauges.upgrade() else {
                    break;
                };
                let wait = if working {
                    WHILE_WORKING
                } else {
                    Duration::from_secs(3600)
                };
                tokio::select! {
                    () = this.changed.notified() => {}
                    update = updates.recv() => {
                        if let Err(broadcast::error::RecvError::Closed) = update {
                            break;
                        }
                    }
                    () = tokio::time::sleep(wait) => {}
                }
                drop(this);
                tokio::time::sleep(GATHER).await;
                let Some(this) = gauges.upgrade() else {
                    break;
                };
                let reading = remote::Meter::read(this.as_ref());
                working = feed.activity().working
                    || !reading.workers_running.is_empty()
                    || !reading.jobs.is_empty();
                this.published.0.send_if_modified(|current| {
                    if **current == reading {
                        return false;
                    }
                    *current = Arc::new(reading);
                    true
                });
            }
        })
    }

    /// The agents running a shell command, as the clients name them:
    /// [`remote::MAIN_AGENT`], or a worker's ID.
    pub(crate) fn commands_running(&self) -> Vec<String> {
        let running = self.background.running();
        if running.is_empty() {
            return Vec::new();
        }
        let workers = lock(&self.workflow)
            .as_ref()
            .map(|workflow| workflow.subagents())
            .unwrap_or_default();
        // The runs that aren't workers' are the main agent's.
        let mut agents: Vec<String> = running
            .into_iter()
            .map(|run| {
                workers
                    .iter()
                    .find(|worker| worker.run_id == run)
                    .map_or_else(
                        || remote::MAIN_AGENT.to_string(),
                        |worker| worker.handle.clone(),
                    )
            })
            .collect();
        agents.sort();
        agents.dedup();
        agents
    }
}

/// What the session billed, what that would cost at list prices when a plan
/// paid for some of it, and its split between the main agent and the
/// workers: from the budget's running totals, without reading its ledger
/// (which grows with every call).
fn costs(workflow: Option<&Workflow>) -> (f64, Option<f64>, remote::CostSplit) {
    let Some(workflow) = workflow else {
        return (0.0, None, remote::CostSplit::default());
    };
    let billed = workflow.context().budget.total_spent();
    let mut split = remote::CostSplit::default();
    for (agent, totals) in workflow.cost_totals_by_agent() {
        let part = if agent == crate::agents::MAIN {
            &mut split.main
        } else {
            &mut split.workers
        };
        part.billed += totals.billed;
        part.list_price += totals.list_price;
        part.input_tokens += totals.input_tokens;
        part.output_tokens += totals.output_tokens;
        part.calls += totals.calls;
    }
    let agents_billed = split.main.billed + split.workers.billed;
    let agents_list = split.main.list_price + split.workers.list_price;
    // The costs added by hand (the search indexes' embeddings) count at
    // what they billed.
    let manual = (billed - agents_billed).max(0.0);
    // A plan covered calls when the list price is more than was billed.
    let api_price = (agents_list > agents_billed + 1e-9).then_some(agents_list + manual);
    (billed, api_price, split)
}

impl remote::Meter for Gauges {
    fn read(&self) -> remote::Reading {
        let workflow = lock(&self.workflow).clone();
        let crew = lock(&self.crew).clone();
        let stats = lock(&self.stats).clone();
        let (project_instructions, worker_project_instructions) = *lock(&self.project);
        // What new workers hand over at, on their model (as the workflow
        // has them until the crew is told).
        let worker_at = |model: &arko_ai::Model, handover: Option<&arko_agents::HandoverConfig>| {
            self.handover.worker_at(
                model,
                handover.and_then(|handover| handover.at_input_tokens),
            )
        };
        let workers_handover_at = crew
            .as_ref()
            .and_then(|crew| match crew.new_workers() {
                Some(new) => worker_at(&new.model.llm_config.model, new.model.handover.as_ref()),
                None => workflow
                    .as_ref()?
                    .context()
                    .agent_configs
                    .get(WORKER)
                    .and_then(|config| {
                        worker_at(&config.llm_config.model, config.handover.as_ref())
                    }),
            })
            .unwrap_or(0);
        // Each worker's context and handover size, on the model it started on.
        let agents = match (&workflow, &crew) {
            (Some(workflow), Some(crew)) => workflow
                .subagents()
                .into_iter()
                .map(|run| {
                    let model = crew.run_model(run.run_id);
                    let context = stats.workers.get(&run.handle).copied();
                    remote::AgentStatus {
                        context_tokens: context.map(|(tokens, _)| tokens),
                        context_window: context.and_then(|(_, window)| window),
                        handover_at: model.as_ref().and_then(|model| {
                            worker_at(&model.llm_config.model, model.handover.as_ref())
                        }),
                        compacts: model.as_ref().is_some_and(crew::WorkerModel::compacts),
                        id: run.handle,
                    }
                })
                .collect(),
            _ => Vec::new(),
        };
        let plans = stats
            .plans
            .iter()
            .map(|(plan, text)| remote::PlanStatus {
                plan: plan.clone(),
                windows: stats.plan_windows.get(plan).cloned().unwrap_or_default(),
                text: text.clone(),
            })
            .collect();
        let (cost, api_price, costs) = costs(workflow.as_deref());
        let cache = cache_status(&stats, &lock(&self.models));
        let last_request = stats.last_request.clone();
        remote::Reading {
            cost,
            api_price,
            costs,
            context_tokens: stats.context_tokens,
            plan_usage: stats.plan_usage,
            workers_running: crew.as_ref().map(|crew| crew.running()).unwrap_or_default(),
            workers: crew
                .filter(|crew| !crew.is_empty())
                .map(|crew| crew.describe()),
            settings: remote::LiveSettings {
                outside: self.outside.is_on(),
                handover_at: self.handover.at(),
                workers_handover_at,
                text_replies: self.text_replies.load(std::sync::atomic::Ordering::Relaxed),
                cc_compact: self.cc_compact.load(std::sync::atomic::Ordering::Relaxed),
                extra_rules: *lock(&self.extra_rules),
                project_instructions,
                worker_project_instructions,
                background_after: self.background.after().map_or(0, |after| after.as_secs()),
            },
            commands: self.commands_running(),
            jobs: self.background.jobs(),
            models: {
                let mut models = lock(&self.models).clone();
                models.reachable = self.keys.reachable();
                if models.openai_auth.is_none() {
                    models.openai_auth = self
                        .keys
                        .openai_auth()
                        .map(|auth| auth.describe().to_string());
                }
                models
            },
            conversations: lock(&self.conversations).clone(),
            page: lock(&self.page).clone(),
            title: lock(&self.title).clone(),
            context_window: stats.context_window,
            plans,
            agents,
            cache,
            last_request,
        }
    }
}

/// The main agent's prompt cache, from what its latest request said, priced
/// for its model.
fn cache_status(stats: &crate::ui::Stats, models: &remote::Models) -> remote::CacheStatus {
    let model = providers::resolve_model(&models.main).ok();
    let chatgpt = models.openai_auth.as_deref() == Some(providers::OpenAiAuth::ChatGpt.describe());
    let on_plan = match model.as_ref().map(|model| &model.provider) {
        Some(arko_ai::Provider::ClaudeCode) => Some("your Claude plan"),
        Some(arko_ai::Provider::OpenAI) if chatgpt => Some("your ChatGPT plan"),
        _ => None,
    };
    let tokens = stats.context_tokens;
    let (miss_cost, refresh_cost) = match (&model, tokens) {
        (Some(model), Some(tokens)) if on_plan.is_none() => {
            let pricing = &model.pricing;
            let hour = stats.cache_lifetime
                == Some(arko_ai::prompt_cache::ANTHROPIC_EXTENDED_PROMPT_CACHE_LIFETIME);
            // Where writes cost nothing extra (OpenAI, Gemini), they cost
            // input.
            let write = hour
                .then_some(pricing.cache_write_1h_cost_per_million)
                .flatten()
                .or(pricing.cache_write_cost_per_million)
                .unwrap_or(pricing.input_cost_per_million);
            let read = pricing
                .cached_input_cost_per_million
                .unwrap_or(pricing.input_cost_per_million);
            let cost = |per_million: f64| Some(tokens as f64 * per_million / 1e6);
            (cost(write), cost(read))
        }
        _ => (None, None),
    };
    remote::CacheStatus {
        lifetime: stats.cache_lifetime,
        refreshed_at: stats
            .last_request
            .as_ref()
            .map(|request| request.at)
            .max(stats.cache_refreshed_at),
        tokens,
        read: stats.cache_use.map(|(read, _)| read),
        written: stats.cache_use.map(|(_, written)| written),
        miss_cost,
        refresh_cost,
        on_plan,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache's costs follow the model's prices and its lifetime: a miss
    /// writes the conversation again, keeping it warm reads it. A plan pays
    /// for both.
    #[test]
    fn the_cache_is_priced_for_the_model() {
        let stats = Stats {
            context_tokens: Some(200_000),
            cache_lifetime: Some(arko_ai::prompt_cache::ANTHROPIC_EXTENDED_PROMPT_CACHE_LIFETIME),
            cache_use: Some((150_000, 50_000)),
            ..Stats::default()
        };
        let models = remote::Models {
            main: "claude-opus-5-5".into(),
            ..remote::Models::default()
        };
        let pricing = providers::resolve_model("opus").unwrap().pricing;
        let cache = cache_status(&stats, &models);
        let close = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-9;
        assert!(close(
            cache.miss_cost,
            0.2 * pricing.cache_write_1h_cost_per_million.unwrap()
        ));
        assert!(close(
            cache.refresh_cost,
            0.2 * pricing.cached_input_cost_per_million.unwrap()
        ));
        assert_eq!((cache.read, cache.written), (Some(150_000), Some(50_000)));
        assert_eq!(cache.on_plan, None);
        let models = remote::Models {
            main: "cc:opus".into(),
            ..models
        };
        let cache = cache_status(&stats, &models);
        assert_eq!((cache.miss_cost, cache.refresh_cost), (None, None));
        assert_eq!(cache.on_plan, Some("your Claude plan"));
    }
}

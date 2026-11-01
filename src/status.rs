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
    /// Who chooses the workers' models, and how many levels of workers the
    /// session allows, as the harness last published them.
    pub(crate) worker_models: Mutex<crate::agents::ModelChoice>,
    /// Whether workers may run on models billed per token, and the daily API
    /// budget, as the harness last published them.
    pub(crate) billing: Mutex<(crate::billing::WorkerBilling, Option<f64>)>,
    /// How long after the user's last message a monitor's warning wakes the
    /// agent, as the harness last published it.
    pub(crate) monitor_wakes: Mutex<crate::monitors::Wakes>,
    /// The keep-warm setting for every session, as the harness last
    /// published it.
    pub(crate) keep_warm: Mutex<Option<crate::keep_warm::Limits>>,
    /// What a refresh of the conversation's cache goes as, as the harness
    /// last published it.
    pub(crate) keep_warm_route: Mutex<Option<crate::keep_warm::Route>>,
    pub(crate) worker_depth: std::sync::atomic::AtomicU32,
    /// The worker retry cap, or 0 for none.
    pub(crate) worker_retry_cap: std::sync::atomic::AtomicU32,
    /// The agent that started a worker gets a copy of the user's messages to
    /// it, as the harness last published it.
    pub(crate) message_copies: std::sync::atomic::AtomicBool,
    /// Which built-in skills the agents get, as the harness last published
    /// it.
    pub(crate) builtin_skills: Mutex<crate::builtin_skills::BuiltinSkills>,
    /// The switches for what agents add, as the harness last published them.
    pub(crate) extensions: Mutex<crate::extension_prefs::Switches>,
    /// Which of the project's instruction files the agents get, and whether
    /// workers do, as the harness last published them.
    pub(crate) project: Mutex<(project::Files, bool)>,
    /// When the shell's commands move to the background, and those running.
    pub(crate) background: Arc<shell::Background>,
    /// What the clients show of the session, whose items name the calls
    /// that can move.
    pub(crate) feed: Option<remote::Feed>,
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
/// jobs), because its time and costs move without a change being told.
const WHILE_WORKING: Duration = Duration::from_secs(1);

impl Gauges {
    /// The session closed. What it ran goes (its workflow holds the
    /// terminal, whose folder lock the session releases), while a front end
    /// may still hold the status.
    pub(crate) fn release(&self) {
        *lock(&self.workflow) = None;
        *lock(&self.crew) = None;
    }

    /// Something the status shows changed (a setting, the models, the
    /// conversations, the web page). The next status goes out soon.
    pub(crate) fn touch(&self) {
        self.changed.notify_one();
    }

    /// The status, as it changes.
    pub(crate) fn watch(&self) -> watch::Receiver<Arc<remote::Reading>> {
        self.published.0.subscribe()
    }

    /// Sends the status to [`watch`](Self::watch)ers whenever it changes, meaning
    /// when something [touches](Self::touch) it or `feed` changes, and every
    /// second while the session works. A status like the last isn't sent
    /// again. Runs until it is aborted.
    pub(crate) fn publish(self: &Arc<Self>, feed: &remote::Feed) -> tokio::task::JoinHandle<()> {
        let gauges = Arc::downgrade(self);
        let feed = feed.clone();
        let (_, mut updates) = feed.subscribe();
        // A call becomes one that can move a moment after its item shows.
        let mut movable = self
            .background
            .manager()
            .map(|jobs| jobs.watch_foreground_calls());
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
                let moves = async {
                    let changed = match movable.as_mut() {
                        Some(movable) => movable.changed().await.is_ok(),
                        None => false,
                    };
                    if !changed {
                        std::future::pending::<()>().await;
                    }
                };
                tokio::select! {
                    () = this.changed.notified() => {}
                    () = moves => {}
                    update = updates.recv() => {
                        if let Err(broadcast::error::RecvError::Closed) = update {
                            break;
                        }
                    }
                    () = tokio::time::sleep(wait) => {}
                }
                drop(this);
                tokio::time::sleep(GATHER).await;
                // One reading for all the updates so far. Once nothing more
                // changes, it waits.
                if !drain(&mut updates) {
                    break;
                }
                let Some(this) = gauges.upgrade() else {
                    break;
                };
                // (The day's API spend is shared with the other sessions as
                // it grows.)
                let crew = lock(&this.crew).clone();
                if let Some(crew) = crew {
                    crew.track_spend();
                }
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

    /// The agent whose run `run` is, as the clients name it, either
    /// [`remote::MAIN_AGENT`], or a worker's ID.
    pub(crate) fn agent_of(&self, run: uuid::Uuid) -> String {
        let workflow = lock(&self.workflow).clone();
        workflow
            .and_then(|workflow| {
                workflow
                    .subagents()
                    .into_iter()
                    .find(|worker| worker.run_id == run)
                    .map(|worker| worker.handle)
            })
            .unwrap_or_else(|| remote::MAIN_AGENT.to_string())
    }

    /// The agents running a call that can move to the background (a shell
    /// command, an MCP tool's call), as the clients name them, either
    /// [`remote::MAIN_AGENT`], or a worker's ID.
    pub(crate) fn commands_running(&self) -> Vec<String> {
        let running = self.background.movable();
        if running.is_empty() {
            return Vec::new();
        }
        let workflow = lock(&self.workflow).clone();
        let workers = workflow
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
/// workers, from the budget's running totals, without reading its ledger
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
    /// The status now. Each lock is taken in a statement of its own, and
    /// nothing else is called while it's held. A guard made inside a larger
    /// expression (an argument, a field of the `Reading` built at the end)
    /// would stay held until that whole expression ends, so two reads at
    /// once (the full screen's, the publisher's, the page's) could each hold
    /// a lock the other waits for.
    fn read(&self) -> remote::Reading {
        let workflow = lock(&self.workflow).clone();
        let crew = lock(&self.crew).clone();
        let stats = lock(&self.stats).clone();
        let (project_instructions, worker_project_instructions) = *lock(&self.project);
        let builtin_skills = lock(&self.builtin_skills).clone();
        let extensions = *lock(&self.extensions);
        let (worker_billing, api_budget) = *lock(&self.billing);
        let models = lock(&self.models).clone();
        let keep_warm = *lock(&self.keep_warm);
        let route = lock(&self.keep_warm_route).clone();
        let extra_rules = *lock(&self.extra_rules);
        let worker_models = *lock(&self.worker_models);
        let monitor_wakes = *lock(&self.monitor_wakes);
        let conversations = lock(&self.conversations).clone();
        let page = lock(&self.page).clone();
        let title = lock(&self.title).clone();
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
                        compacts: model.as_ref().is_some_and(crew::WorkerSetup::compacts),
                        id: run.handle,
                    }
                })
                .collect(),
            _ => Vec::new(),
        };
        let plans = stats.plan_status.values().cloned().collect();
        let (cost, api_price, costs) = costs(workflow.as_deref());
        let turn = self
            .feed
            .as_ref()
            .is_some_and(|feed| feed.activity().working);
        let cache = cache_status(&stats, &models, route.as_ref(), keep_warm, turn);
        let main_plan = {
            let model = providers::resolve_model(&models.main).ok();
            main_plan(model.as_ref(), &models).map(|plan| providers::plan_name(Some(plan)))
        };
        let last_request = stats.last_request.clone();
        remote::Reading {
            cost,
            api_price,
            costs,
            context_tokens: stats.context_tokens,
            plan_usage: stats.plan_usage,
            workers_running: crew
                .as_ref()
                .map(|crew| crew.running_with_parents())
                .unwrap_or_default(),
            workers: crew
                .filter(|crew| !crew.is_empty())
                .map(|crew| crew.describe()),
            settings: remote::LiveSettings {
                outside: self.outside.is_on(),
                handover_at: self.handover.at(),
                workers_handover_at,
                text_replies: self.text_replies.load(std::sync::atomic::Ordering::Relaxed),
                cc_compact: self.cc_compact.load(std::sync::atomic::Ordering::Relaxed),
                extra_rules,
                worker_models,
                worker_billing,
                api_budget,
                worker_depth: self.worker_depth.load(std::sync::atomic::Ordering::Relaxed),
                worker_retry_cap: Some(
                    self.worker_retry_cap
                        .load(std::sync::atomic::Ordering::Relaxed),
                )
                .filter(|cap| *cap > 0),
                message_copies: self
                    .message_copies
                    .load(std::sync::atomic::Ordering::Relaxed),
                builtin_skills: !builtin_skills.none,
                builtin_skill_list: crate::builtin_skills::names()
                    .map(|name| remote::BuiltinSkillSetting {
                        name: name.to_string(),
                        on: !builtin_skills.off.iter().any(|off| off == name),
                    })
                    .to_vec(),
                ask_before_adding_mcp: extensions.ask_before_adding_mcp,
                review_project_skills: extensions.review_project_skills,
                keep_agents_files_out_of_git: extensions.keep_out_of_git,
                keep_warm,
                project_instructions,
                worker_project_instructions,
                background_after: self.background.after().map_or(0, |after| after.as_secs()),
                monitor_wakes,
                monitor_wakes_within: monitor_wakes
                    .within(stats.cache_lifetime)
                    .map(|within| within.as_secs()),
            },
            commands: self.commands_running(),
            movable: match (&self.feed, self.background.manager()) {
                (Some(feed), Some(jobs)) => crate::jobs::movable(feed, &jobs),
                _ => Vec::new(),
            },
            // As the quit popup names them, which a reading replaces.
            jobs: crate::jobs::named(
                self.feed.as_ref(),
                self.background.manager().as_deref(),
                self.background.jobs(),
            ),
            models: {
                let mut models = models;
                models.reachable = self.keys.reachable();
                if models.openai_auth.is_none() {
                    models.openai_auth = self
                        .keys
                        .openai_auth()
                        .map(|auth| auth.describe().to_string());
                }
                models
            },
            conversations,
            questions: self
                .feed
                .as_ref()
                .map(remote::Feed::open_questions)
                .unwrap_or_default(),
            approvals: self
                .feed
                .as_ref()
                .map(remote::Feed::approvals)
                .unwrap_or_default(),
            page,
            title,
            context_window: stats.context_window,
            plans,
            main_plan,
            agents,
            cache,
            last_request,
        }
    }
}

/// Takes the feed's updates waiting in `updates`, also those it fell behind
/// on, and false once the feed is gone.
fn drain(updates: &mut broadcast::Receiver<remote::Update>) -> bool {
    loop {
        match updates.try_recv() {
            Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(broadcast::error::TryRecvError::Empty) => return true,
            Err(broadcast::error::TryRecvError::Closed) => return false,
        }
    }
}

/// The plan that pays for the main agent's calls to `model`, when one does, meaning
/// the Claude plan through Claude Code, or the ChatGPT plan through a
/// ChatGPT login.
fn main_plan(
    model: Option<&arko_ai::Model>,
    models: &remote::Models,
) -> Option<arko_ai::SubscriptionPlan> {
    let chatgpt = models.openai_auth.as_deref() == Some(providers::OpenAiAuth::ChatGpt.describe());
    match model.map(|model| &model.provider) {
        Some(arko_ai::Provider::ClaudeCode) => Some(arko_ai::SubscriptionPlan::ClaudeCode),
        Some(arko_ai::Provider::OpenAI) if chatgpt => Some(arko_ai::SubscriptionPlan::ChatGpt),
        _ => None,
    }
}

/// The main agent's prompt cache, from what its latest request said, priced
/// for its model, with keep-warm while it is on (under the keep-warm
/// `setting`) and a refresh of the conversation (`route`) goes out of band.
/// A `turn` running sends no refresh.
pub(crate) fn cache_status(
    stats: &crate::ui::Stats,
    models: &remote::Models,
    route: Option<&crate::keep_warm::Route>,
    setting: Option<crate::keep_warm::Limits>,
    turn: bool,
) -> remote::CacheStatus {
    let route = route.filter(|route| crate::keep_warm::refreshes_out_of_band(&route.model));
    let keep_warm = stats
        .keep_warm
        .limits(setting)
        .zip(route)
        .map(|(limits, route)| {
            let cache = crate::keep_warm::Cache {
                used: stats.cache_used(),
                lifetime: stats.cache_lifetime,
                quote: refresh_quote(stats, route),
            };
            stats.keep_warm.status(
                &limits,
                cache,
                stats.cache_refreshed_at.is_some(),
                turn,
                crate::keep_warm::Moment::now(),
            )
        });
    let model = providers::resolve_model(&models.main).ok();
    let on_plan = main_plan(model.as_ref(), models).map(|plan| match plan {
        arko_ai::SubscriptionPlan::ClaudeCode => "your Claude plan",
        arko_ai::SubscriptionPlan::ChatGpt => "your ChatGPT plan",
    });
    let tokens = stats.context_tokens;
    let (miss_cost, refresh_cost) = match (&model, tokens) {
        (Some(model), Some(tokens)) if on_plan.is_none() => {
            let long = long_cache(stats.cache_lifetime);
            let (write, read) = providers::cache_prices(model, tokens, models.main_fast, long);
            let cost = |per_million: f64| Some(tokens as f64 * per_million / 1e6);
            (cost(write), cost(read))
        }
        _ => (None, None),
    };
    remote::CacheStatus {
        lifetime: stats.cache_lifetime,
        refreshed_at: stats.cache_refreshed_at,
        tokens,
        read: stats.cache_use.map(|(read, _)| read),
        written: stats.cache_use.map(|(_, written)| written),
        miss_cost,
        refresh_cost,
        on_plan,
        keep_warm,
    }
}

/// Whether a cache of `lifetime` is a long one (Claude's hour, OpenAI's 12
/// hours), which is written at its own price.
fn long_cache(lifetime: Option<Duration>) -> bool {
    lifetime.is_some_and(|lifetime| {
        lifetime == arko_ai::prompt_cache::ANTHROPIC_EXTENDED_PROMPT_CACHE_LIFETIME
            || lifetime == arko_ai::prompt_cache::OPENAI_EXTENDED_PROMPT_CACHE_LIFETIME
    })
}

/// The most output a keep-warm refresh writes on a key, meaning arko_agents' output
/// limit for one whose provider refused the smaller limit.
const REFRESH_OUTPUT_TOKENS: usize = 1_024;

/// The ping a refresh asks after the conversation, with what goes around
/// it, in tokens (generously).
const PING_TOKENS: usize = 64;

/// What keep-warm's next refresh of the conversation costs at API prices,
/// sent as `route`, from the conversation now. The latest request's input is
/// read from the cache, and what came after it (the reply, and the ping) is
/// written to it, at the cache's lifetime's rate, with the refresh's output.
/// On a key the output is capped, so that's a ceiling. On a plan nothing caps
/// it, so it's an estimate, the larger of the cap and the most an earlier
/// refresh wrote. `None` where Arko has no price for the model, or nothing
/// was sent yet.
// shortcut: priced here, before the refresh, from what the status knows,
// until arko_workflows' refresh quotes its own price first.
pub(crate) fn refresh_quote(
    stats: &crate::ui::Stats,
    route: &crate::keep_warm::Route,
) -> Option<f64> {
    let output = if route.plan {
        REFRESH_OUTPUT_TOKENS.max(stats.keep_warm.most_output())
    } else {
        REFRESH_OUTPUT_TOKENS
    };
    let quote = providers::refresh_price(
        &route.model,
        stats.context_tokens?,
        stats.reply_tokens.unwrap_or(0) + PING_TOKENS,
        output,
        route.fast,
        long_cache(stats.cache_lifetime),
    );
    (quote > 0.0).then_some(quote)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gauges with nothing set, reading `feed`.
    fn gauges(feed: &remote::Feed) -> Arc<Gauges> {
        let output = Arc::new(crate::ui::Output::new(None, false));
        Arc::new(Gauges {
            models: Mutex::default(),
            conversations: Mutex::default(),
            workflow: Mutex::new(None),
            crew: Mutex::new(None),
            stats: Arc::default(),
            outside: Arc::new(outside::Outside::new(false, Vec::new(), ".".into())),
            handover: Arc::new(handover::Handover::new(
                Arc::default(),
                None,
                None,
                output,
                feed.clone(),
            )),
            keys: keys::Keys::new(Arc::new(arko_ai::LLMClient::new()), None, None, None),
            text_replies: Default::default(),
            cc_compact: Default::default(),
            extra_rules: Default::default(),
            worker_models: Default::default(),
            billing: Default::default(),
            monitor_wakes: Default::default(),
            keep_warm: Default::default(),
            keep_warm_route: Default::default(),
            worker_depth: Default::default(),
            worker_retry_cap: Default::default(),
            message_copies: Default::default(),
            builtin_skills: Default::default(),
            extensions: Default::default(),
            project: Default::default(),
            background: Arc::default(),
            feed: Some(feed.clone()),
            page: Mutex::default(),
            changed: Default::default(),
            title: Default::default(),
            published: Default::default(),
        })
    }

    /// Reads of the status from several threads at once, as the full
    /// screen's clock, the status's publisher and the web page make them,
    /// all end. A read takes one of the gauges' locks at a time, so no two
    /// reads can each hold a lock the other waits for.
    #[test]
    fn reads_of_the_status_at_once_all_end() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const THREADS: usize = 4;
        const READS: usize = 5_000;
        let feed = remote::Feed::new("main");
        let gauges = gauges(&feed);
        let read = Arc::new(AtomicUsize::new(0));
        for _ in 0..THREADS {
            let (gauges, read) = (gauges.clone(), read.clone());
            std::thread::spawn(move || {
                for _ in 0..READS {
                    let _ = remote::Meter::read(gauges.as_ref());
                    read.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        // Slow reads go on, and stuck ones don't.
        let mut last = 0;
        while last < THREADS * READS {
            std::thread::sleep(Duration::from_secs(3));
            let now = read.load(Ordering::Relaxed);
            assert!(now > last, "the reads stopped after {now} of them");
            last = now;
        }
    }

    /// The status is read once for all the feed's updates that came while it
    /// gathered them, also more than it keeps. After a long streamed reply,
    /// it doesn't go on reading them one at a time.
    #[test]
    fn the_status_takes_the_feeds_updates_at_once() {
        let feed = remote::Feed::new("main");
        let (_, mut updates) = feed.subscribe();
        for n in 0..5_000 {
            feed.notice(remote::Level::Info, &format!("note {n}"));
        }
        assert!(drain(&mut updates));
        assert!(matches!(
            updates.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        drop(feed);
        assert!(!drain(&mut updates), "the feed is gone");
    }

    #[test]
    fn the_main_plan_follows_the_main_models_provider_and_auth() {
        for (name, auth, expected) in [
            ("cc:opus", None, Some(arko_ai::SubscriptionPlan::ClaudeCode)),
            ("opus", None, None),
            (
                "sol",
                Some(providers::OpenAiAuth::ChatGpt.describe()),
                Some(arko_ai::SubscriptionPlan::ChatGpt),
            ),
            ("sol", None, None),
            (
                "opus",
                Some(providers::OpenAiAuth::ChatGpt.describe()),
                None,
            ),
        ] {
            let models = remote::Models {
                main: name.into(),
                openai_auth: auth.map(str::to_string),
                ..remote::Models::default()
            };
            let model = providers::resolve_model(name).unwrap();
            assert_eq!(
                main_plan(Some(&model), &models),
                expected,
                "{name} {auth:?}"
            );
        }
        assert_eq!(main_plan(None, &remote::Models::default()), None);
        let mut reading = remote::Reading::default();
        let before = serde_json::to_value(&reading).unwrap();
        reading.main_plan = Some("Claude plan");
        assert_eq!(serde_json::to_value(&reading).unwrap(), before);
    }

    /// The cache's costs follow the model's prices and its lifetime. A miss
    /// writes the conversation again, keeping it warm reads it. A plan pays
    /// for both.
    /// Keep-warm's quote prices the refresh that goes. The latest request's
    /// input is read from the cache, and what came after it (a long reply,
    /// and the ping) is written at the lifetime's rate, with the output, at
    /// the conversation's own fast mode. On a key the output is its cap, and
    /// on a plan the larger of that and the most an earlier refresh wrote.
    #[test]
    fn the_quote_prices_the_refresh_that_goes() {
        let price = |model: &arko_ai::Model, fast: bool, output: usize| {
            let (write, read) = providers::cache_prices(model, 150_064, fast, true);
            let pricing = model.pricing.at(std::time::SystemTime::now());
            let tier = fast.then_some(arko_ai::ServiceTier::Fast);
            let output_rate =
                pricing.output_cost_per_million * pricing.tier_multiplier(tier.as_ref(), false);
            (100_000.0 * read + 50_064.0 * write + output as f64 * output_rate) / 1e6
        };
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        let mut stats = Stats {
            context_tokens: Some(100_000),
            reply_tokens: Some(50_000),
            cache_lifetime: Some(arko_ai::prompt_cache::ANTHROPIC_EXTENDED_PROMPT_CACHE_LIFETIME),
            ..Stats::default()
        };
        let opus = providers::resolve_model("opus").unwrap();
        let key = crate::keep_warm::Route {
            model: opus.clone(),
            fast: false,
            plan: false,
        };
        let quote = refresh_quote(&stats, &key).unwrap();
        assert!(close(quote, price(&opus, false, 1_024)), "{quote}");
        // More than reading the reply from the cache would cost.
        let (_, read) = providers::cache_prices(&opus, 150_064, false, true);
        assert!(quote > 150_064.0 * read / 1e6);
        // Fast mode as the conversation runs it.
        let sol = providers::resolve_model("sol").unwrap();
        let fast = crate::keep_warm::Route {
            model: sol.clone(),
            fast: true,
            plan: false,
        };
        let quote = refresh_quote(&stats, &fast).unwrap();
        assert!(close(quote, price(&sol, true, 1_024)), "{quote}");
        assert!(quote > price(&sol, false, 1_024));
        // On a plan, an earlier refresh that wrote more sets the estimate.
        let plan = crate::keep_warm::Route {
            plan: true,
            ..key.clone()
        };
        assert!(close(
            refresh_quote(&stats, &plan).unwrap(),
            price(&opus, false, 1_024)
        ));
        stats.keep_warm.sent(&arko_ai::UsageInfo {
            output_tokens: 3_000,
            ..arko_ai::UsageInfo::default()
        });
        assert!(close(
            refresh_quote(&stats, &plan).unwrap(),
            price(&opus, false, 3_000)
        ));
        assert!(close(
            refresh_quote(&stats, &key).unwrap(),
            price(&opus, false, 1_024)
        ));
    }

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
        let cache = cache_status(&stats, &models, None, None, false);
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
        let cache = cache_status(&stats, &models, None, None, false);
        assert_eq!((cache.miss_cost, cache.refresh_cost), (None, None));
        assert_eq!(cache.on_plan, Some("your Claude plan"));
        // Keep-warm shows for a model that refreshes out of band, with its
        // next refresh while no turn runs.
        let used = crate::keep_warm::Moment::now();
        let kept = Stats {
            cache_refreshed_at: Some(used.at),
            cache_seen: Some(used.seen),
            ..stats.clone()
        };
        // (Under the setting, the session's own choice being none.)
        let setting = Some(crate::keep_warm::Limits {
            refreshes: Some(3),
            ..Default::default()
        });
        let route = |name: &str| crate::keep_warm::Route {
            model: providers::resolve_model(name).unwrap(),
            fast: false,
            plan: name.starts_with("cc:"),
        };
        let cc = route("cc:opus");
        assert_eq!(
            cache_status(&kept, &models, Some(&cc), None, false).keep_warm,
            None
        );
        let status = cache_status(&kept, &models, Some(&cc), setting, false)
            .keep_warm
            .unwrap();
        assert_eq!(
            (status.left, status.next_at),
            (Some(3), Some(used.at + 55 * 60_000))
        );
        assert_eq!(
            cache_status(&kept, &models, Some(&cc), setting, true)
                .keep_warm
                .unwrap()
                .next_at,
            None
        );
        let openrouter = route("openrouter:anthropic/claude-opus-5.5");
        assert_eq!(
            cache_status(&kept, &models, Some(&openrouter), setting, false).keep_warm,
            None
        );
        // OpenAI's 12-hour cache on an API key costs 2.5 times the input
        // price to write, and its 30-minute one 1.25 times.
        let sol = providers::resolve_model("sol").unwrap().pricing;
        let models = remote::Models {
            main: "sol".into(),
            openai_auth: Some(providers::OpenAiAuth::ApiKey.describe().to_string()),
            ..remote::Models::default()
        };
        for (lifetime, rate) in [
            (
                arko_ai::prompt_cache::OPENAI_EXTENDED_PROMPT_CACHE_LIFETIME,
                2.5,
            ),
            (
                arko_ai::prompt_cache::OPENAI_STANDARD_PROMPT_CACHE_LIFETIME,
                1.25,
            ),
        ] {
            let stats = Stats {
                cache_lifetime: Some(lifetime),
                ..stats.clone()
            };
            let cache = cache_status(&stats, &models, None, None, false);
            assert!(close(
                cache.miss_cost,
                0.2 * rate * sol.input_cost_per_million
            ));
        }
    }
}

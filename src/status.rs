//! The session's figures for the status (`remote::Meter`), which the full
//! screen and the web page read.

use std::sync::{Arc, Mutex};

use arko_workflows::prelude::*;

use crate::agents::{ExtraRules, WORKER};
use crate::crew::Crew;
use crate::lock;
use crate::ui::Stats;
use crate::{handover, keys, outside, project, providers, remote, shell};

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
}

impl Gauges {
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
        remote::Reading {
            cost: workflow
                .as_ref()
                .map_or(0.0, |workflow| workflow.context().budget.total_spent()),
            api_price: workflow.and_then(|workflow| providers::api_price(&workflow.cost_ledger())),
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
        }
    }
}

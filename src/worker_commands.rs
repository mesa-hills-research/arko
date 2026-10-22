//! `/worker-models`, `/worker-depth` and `/profiles`: who chooses the
//! workers' models, how deep workers go, and the worker profiles the user
//! keeps.

use arko_workflows::notes::{Note, NoteSource};

use crate::Harness;
use crate::agents::{self, ModelChoice};
use crate::crew::NESTED_WORKERS;
use crate::remote::Source;
use crate::workers;

impl Harness {
    /// What the user is told about the workers' models once `choice` is set.
    fn models_sentence(&self, choice: ModelChoice) -> String {
        match choice {
            ModelChoice::Agent => {
                "The main agent chooses a profile, model and effort for each worker, and can save profiles."
                    .to_string()
            }
            ModelChoice::Profiles => {
                "The main agent starts workers with a profile only. It can't choose a model or effort by itself."
                    .to_string()
            }
            ModelChoice::Session => format!(
                "Every worker runs the session's worker model, {}.",
                self.settings.worker.describe()
            ),
        }
    }

    /// `/worker-models`, typed at the prompt, sent from the page or chosen in
    /// the settings: says who chooses the workers' models, and with `agent`,
    /// `profiles` or `session` changes it, at any time. The start hook
    /// enforces it on each start. Before the conversation starts the agents
    /// are built again, since their instructions say what the main agent
    /// may choose. After, the main agent is told in a note.
    pub(crate) fn worker_models_command(&mut self, argument: &str, source: Source) {
        let before = self.settings.worker_models;
        let result = self.worker_models_request(argument);
        let changed = before != self.settings.worker_models;
        self.setting_reply(result, source, changed);
    }

    fn worker_models_request(&mut self, argument: &str) -> Result<String, String> {
        if self.settings.max_workers == 0 || self.crew.is_none() {
            return Err("This session has no workers (it started with --workers 0, or with the old worker tools).".to_string());
        }
        let now = self.settings.worker_models;
        if argument.is_empty() {
            return Ok(format!(
                "{} is set to {}. {} /worker-models agent, profiles or session changes that.",
                workers::MODELS_NAME,
                now.name(),
                self.models_sentence(now)
            ));
        }
        let Some(choice) = ModelChoice::parse(argument) else {
            return Err(
                "/worker-models takes agent, profiles or session (/worker-models profiles)."
                    .to_string(),
            );
        };
        if choice == now {
            return Ok(format!(
                "{} is set to {} already. {}",
                workers::MODELS_NAME,
                choice.name(),
                self.models_sentence(choice)
            ));
        }
        let mut settings = self.settings.clone();
        settings.worker_models = choice;
        let started = self.conversation_started();
        if started {
            self.settings = settings;
            self.save_session();
            self.start_new_workers();
            self.publish_models();
        } else {
            self.rebuild_agents(settings)?;
        }
        let sentence = self.models_sentence(choice);
        if started {
            // The main agent's instructions say what it could choose when the
            // conversation started: a note tells it what it can now.
            let told = match choice {
                ModelChoice::Agent => "The user now lets you choose a profile, model and effort for each worker, and save profiles.".to_string(),
                ModelChoice::Profiles => "The user now lets you start workers with a profile only. Leave out model and effort, and don't save profiles. worker_options lists the profiles.".to_string(),
                ModelChoice::Session => format!(
                    "The user now has every worker run the session's worker model, {}. Start workers without a profile, model or effort.",
                    self.settings.worker.describe()
                ),
            };
            if let Err(error) = self
                .workflow()
                .post_note(Note::new(told).source(NoteSource::Host).to([agents::MAIN]))
            {
                log::warn!(
                    "could not tell the main agent who chooses the workers' models: {error}"
                );
            }
        }
        Ok(sentence)
    }

    /// What `/worker-depth` says of a depth of `depth`.
    fn depth_sentence(depth: u32) -> String {
        if NESTED_WORKERS {
            if depth == 1 {
                "Only the main agent starts workers.".to_string()
            } else {
                format!(
                    "Workers can go {depth} levels deep. A worker the main agent starts can start workers of its own when the main agent allows it."
                )
            }
        } else {
            format!(
                "Worker depth is {depth}. It takes effect when workers can start workers of their own, which they can't yet. For now only the main agent starts workers."
            )
        }
    }

    /// `/worker-depth`, typed at the prompt, sent from the page or chosen in
    /// the settings: says how many levels of workers the session allows, or
    /// sets it, from 1 to 4.
    pub(crate) fn worker_depth_command(&mut self, argument: &str, source: Source) {
        let before = self.settings.worker_depth;
        let result = self.worker_depth_request(argument);
        let changed = before != self.settings.worker_depth;
        self.setting_reply(result, source, changed);
    }

    fn worker_depth_request(&mut self, argument: &str) -> Result<String, String> {
        if self.settings.max_workers == 0 || self.crew.is_none() {
            return Err("This session has no workers (it started with --workers 0, or with the old worker tools).".to_string());
        }
        let now = self.settings.worker_depth;
        if argument.is_empty() {
            return Ok(format!(
                "{} is {now}. {} /worker-depth 1 to {} changes that.",
                workers::DEPTH_NAME,
                Self::depth_sentence(now),
                workers::MAX_DEPTH
            ));
        }
        let depth = workers::parse_depth(argument)?;
        if depth == now {
            return Ok(format!(
                "{} is {depth} already. {}",
                workers::DEPTH_NAME,
                Self::depth_sentence(depth)
            ));
        }
        self.settings.worker_depth = depth;
        self.save_session();
        self.start_new_workers();
        self.publish_models();
        if NESTED_WORKERS
            && self.conversation_started()
            && let Err(error) = self.workflow().post_note(
                Note::new(format!(
                    "The user now allows {depth} levels of workers. Workers you start with nested true can go that deep."
                ))
                .source(NoteSource::Host)
                .to([agents::MAIN]),
            )
        {
            log::warn!("could not tell the main agent how deep workers can go: {error}");
        }
        Ok(Self::depth_sentence(depth))
    }
}

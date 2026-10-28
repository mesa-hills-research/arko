//! `/worker-models`, `/worker-billing`, `/api-budget`, `/worker-depth`,
//! `/worker-retry-cap`, `/message-copies` and `/profiles`: who chooses the
//! workers' models, whether they may run on models billed per token and the
//! day's budget for those, how deep workers go, how many failed requests
//! stop a worker, whether the agent that started a worker gets a copy of the
//! user's messages to it, and the worker profiles the user keeps.

use arko_workflows::notes::{Note, NoteSource};

use crate::agents::{self, ModelChoice};
use crate::billing::WorkerBilling;
use crate::profiles::{self, Author, Place, Profile, Roots};
use crate::remote::Source;
use crate::ui;
use crate::workers;
use crate::{Harness, providers};

/// The most profiles `/profiles` lists, and the most that can't be read or
/// are hidden, since a project's folder can hold any number of files.
const MAX_ROWS: usize = 100;
const MAX_UNREADABLE_ROWS: usize = 20;

/// The width of what a profile is for in `/profiles`, under its row, so a
/// screen of 80 columns wraps nothing of it.
const ABOUT_WIDTH: usize = 68;

/// `text` in lines of at most `width` characters, cut between words.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines
}

/// What a profile made with `/profiles add` and no description says it is for.
const PLACEHOLDER: &str = "Describe what this profile is for";

/// How `/profiles` is used.
const PROFILES_USAGE: &str = "/profiles lists the worker profiles, /profiles NAME shows one, /profiles add NAME MODEL [EFFORT] [DESCRIPTION] makes one for every project, and /profiles remove NAME deletes one.";

/// The retry cap as the commands say it: a number, or `off`.
fn cap_word(cap: Option<u32>) -> String {
    cap.map_or_else(|| "off".to_string(), |cap| cap.to_string())
}

/// What the main agent is told when the user changes the depth to `depth`,
/// if anything. With every worker on the session's model none starts workers,
/// and it was told so.
fn depth_note(depth: u32, choice: ModelChoice) -> Option<String> {
    if choice == ModelChoice::Session {
        return None;
    }
    Some(if depth <= 1 {
        "The user now allows 1 level of workers. A worker can't start workers of its own, so start workers without nested.".to_string()
    } else {
        format!(
            "The user now allows {depth} levels of workers. A worker you start with nested true can start workers of its own, down to level {depth}."
        )
    })
}

impl Harness {
    /// The folders profiles are read from, for this session.
    pub(crate) fn profile_roots(&self) -> Roots {
        Roots::new(&self.root)
            .workspace(&self.settings.workspace)
            .session(&self.session.dir)
    }

    /// What the user is told about the workers' models once `choice` is set.
    fn models_sentence(&self, choice: ModelChoice) -> String {
        match choice {
            ModelChoice::Session => format!(
                "Every worker runs the session's worker model, {}. No worker starts workers.",
                self.settings.worker.describe()
            ),
            other => other.about().to_string(),
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
            return Err(format!(
                "`{}` isn't one of agent, profiles or session, such as /worker-models profiles.",
                argument.trim()
            ));
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
                ModelChoice::Agent => format!(
                    "The user now lets you choose a profile, model and effort for each worker, and save profiles.{}",
                    self.nesting_again()
                ),
                ModelChoice::Profiles => format!(
                    "The user now lets you start workers with a profile only. Leave out model and effort, and don't save profiles. worker_options lists the profiles.{}",
                    self.nesting_again()
                ),
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

    /// `/worker-billing`, typed at the prompt, sent from the page or chosen
    /// in the settings: says whether workers may run on models billed per
    /// token, and with `plans`, `ask` or `api` changes it, at any time. The
    /// start hook holds each start to it. Before the conversation starts the
    /// agents are built again, since the main agent's instructions say it.
    /// After, the main agent is told in a note.
    pub(crate) fn worker_billing_command(&mut self, argument: &str, source: Source) {
        let before = self.settings.worker_billing;
        let result = self.worker_billing_request(argument);
        let changed = before != self.settings.worker_billing;
        self.setting_reply(result, source, changed);
    }

    fn worker_billing_request(&mut self, argument: &str) -> Result<String, String> {
        if self.settings.max_workers == 0 || self.crew.is_none() {
            return Err("This session has no workers (it started with --workers 0, or with the old worker tools).".to_string());
        }
        let now = self.settings.worker_billing;
        let name = workers::BILLING_NAME;
        if argument.is_empty() {
            return Ok(format!(
                "{name} is {} ({}). {} {} plans, ask or api changes it.",
                now.name(),
                now.label(),
                now.about(),
                workers::BILLING_COMMAND
            ));
        }
        let Some(billing) = WorkerBilling::parse(argument) else {
            return Err(format!(
                "`{}` isn't one of plans, ask or api, such as {}.",
                argument.trim(),
                WorkerBilling::Plans.command()
            ));
        };
        if billing == now {
            return Ok(format!(
                "{name} is {} already. {}",
                billing.name(),
                billing.about()
            ));
        }
        let mut settings = self.settings.clone();
        settings.worker_billing = billing;
        if self.conversation_started() {
            self.settings = settings;
            self.save_session();
            self.start_new_workers();
            self.publish_models();
            // The main agent's instructions say how workers were paid for
            // when the conversation started: a note tells it how now.
            let told = format!(
                "The user changed worker billing to {}. {}",
                billing.label(),
                agents::billing_words(billing)
            );
            if let Err(error) = self
                .workflow()
                .post_note(Note::new(told).source(NoteSource::Host).to([agents::MAIN]))
            {
                log::warn!("could not tell the main agent how workers are paid for: {error}");
            }
        } else {
            self.rebuild_agents(settings)?;
        }
        Ok(format!(
            "{name} is now {} ({}). {}",
            billing.name(),
            billing.label(),
            billing.about()
        ))
    }

    /// `/api-budget`, typed at the prompt, sent from the page or chosen in
    /// the settings: says the daily API budget and the day's spend, or with
    /// `off`, `on` or dollars sets it, at any time. The start hook holds each
    /// start to it, and the agents aren't told.
    pub(crate) fn api_budget_command(&mut self, argument: &str, source: Source) {
        let before = self.settings.api_budget;
        let result = self.api_budget_request(argument);
        let changed = before != self.settings.api_budget;
        self.setting_reply(result, source, changed);
    }

    fn api_budget_request(&mut self, argument: &str) -> Result<String, String> {
        let Some(crew) = self.crew.clone().filter(|_| self.settings.max_workers > 0) else {
            return Err("This session has no workers (it started with --workers 0, or with the old worker tools).".to_string());
        };
        let now = self.settings.api_budget;
        let name = workers::BUDGET_NAME;
        let state = |budget: Option<f64>| budget.map_or_else(|| "off".to_string(), workers::money);
        if argument.is_empty() {
            let spent = crew.spent_today().map_or_else(String::new, |spent| {
                format!(
                    " Today’s API spend across your sessions is {}.",
                    workers::money(spent)
                )
            });
            return Ok(format!(
                "{name} is {}. {}{spent} {} off, on or an amount of dollars changes it.",
                state(now),
                workers::budget_about(now),
                workers::BUDGET_COMMAND
            ));
        }
        let budget = workers::parse_budget(argument)?;
        if budget == now {
            return Ok(format!("{name} is {} already.", state(budget)));
        }
        self.settings.api_budget = budget;
        self.save_session();
        self.start_new_workers();
        self.publish_models();
        Ok(format!(
            "{name} is now {}. {}",
            state(budget),
            workers::budget_about(budget)
        ))
    }

    /// What the main agent is told again once workers are not all on the
    /// session's model, when the depth lets them start workers: its
    /// instructions may not have said it.
    fn nesting_again(&self) -> String {
        if self.settings.worker_depth > 1 {
            format!(" {}", agents::NESTING)
        } else {
            String::new()
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
                "{} is {now}. {} /worker-depth 1 to {} changes it.",
                workers::DEPTH_NAME,
                workers::depth_effect(now, self.settings.worker_models),
                workers::MAX_DEPTH
            ));
        }
        let depth = workers::parse_depth(argument)?;
        if depth == now {
            return Ok(format!(
                "{} is {depth} already. {}",
                workers::DEPTH_NAME,
                workers::depth_effect(depth, self.settings.worker_models)
            ));
        }
        let mut settings = self.settings.clone();
        settings.worker_depth = depth;
        if self.conversation_started() {
            self.settings = settings;
            self.save_session();
            self.start_new_workers();
            self.publish_models();
            // The main agent's instructions say what the depth was when the
            // conversation started: a note tells it what it is now.
            if let Some(told) = depth_note(depth, self.settings.worker_models)
                && let Err(error) = self
                    .workflow()
                    .post_note(Note::new(told).source(NoteSource::Host).to([agents::MAIN]))
            {
                log::warn!("could not tell the main agent how deep workers can go: {error}");
            }
        } else {
            // Before it starts, the agents are built again, since their
            // instructions mention workers that start workers or not.
            self.rebuild_agents(settings)?;
        }
        Ok(format!(
            "{} is now {depth}. {}",
            workers::DEPTH_NAME,
            workers::depth_effect(depth, self.settings.worker_models)
        ))
    }

    /// `/worker-retry-cap`, typed at the prompt, sent from the page or chosen
    /// in the settings: says how many failed model requests in a row stop a
    /// worker, or with `off`, `on` or a number sets it. Before the
    /// conversation starts the agents are built again. After it the running
    /// workflow gets the cap, and its running workers have it at their next
    /// failed request.
    pub(crate) fn worker_retry_cap_command(&mut self, argument: &str, source: Source) {
        let before = self.settings.worker_retry_cap;
        let result = self.worker_retry_cap_request(argument);
        let changed = before != self.settings.worker_retry_cap;
        self.setting_reply(result, source, changed);
    }

    fn worker_retry_cap_request(&mut self, argument: &str) -> Result<String, String> {
        if self.settings.max_workers == 0 || self.crew.is_none() {
            return Err("This session has no workers (it started with --workers 0, or with the old worker tools).".to_string());
        }
        let now = self.settings.worker_retry_cap;
        let name = workers::RETRY_CAP_NAME;
        if argument.is_empty() {
            return Ok(format!(
                "{name} is {}. {} {} off, on or a number changes it.",
                cap_word(now),
                workers::retry_cap_about(now),
                workers::RETRY_CAP_COMMAND
            ));
        }
        let cap = workers::parse_retry_cap(argument)?;
        if cap == now {
            return Ok(format!("{name} is {} already.", cap_word(cap)));
        }
        let mut settings = self.settings.clone();
        settings.worker_retry_cap = cap;
        let text = format!(
            "{name} is now {}. {}",
            cap_word(cap),
            workers::retry_cap_about(cap)
        );
        if self.conversation_started() {
            self.settings = settings;
            self.save_session();
            self.publish_models();
            if let Some(crew) = &self.crew {
                crew.set_retry_cap(cap);
            }
            self.workflow().set_max_failed_requests(cap);
        } else {
            self.rebuild_agents(settings)?;
        }
        Ok(text)
    }

    /// `/message-copies`, typed at the prompt, sent from the page or chosen
    /// in the settings: says whether the agent that started a worker gets a
    /// copy of the user's messages to it, or with `on` or `off` sets that.
    /// It applies from the next message, and the agents aren't told.
    pub(crate) fn message_copies_command(&mut self, argument: &str, source: Source) {
        let before = self.settings.message_copies;
        let result = self.message_copies_request(argument);
        let changed = before != self.settings.message_copies;
        self.setting_reply(result, source, changed);
    }

    fn message_copies_request(&mut self, argument: &str) -> Result<String, String> {
        if self.settings.max_workers == 0 || self.crew.is_none() {
            return Err("This session has no workers (it started with --workers 0, or with the old worker tools).".to_string());
        }
        let now = self.settings.message_copies;
        let on = match argument {
            "" => {
                return Ok(format!(
                    "{} are {}. {} {} changes that.",
                    workers::COPIES_NAME,
                    workers::copies_state(now),
                    workers::copies_about(now),
                    workers::copies_command(!now)
                ));
            }
            "on" => true,
            "off" => false,
            _ => {
                return Err(format!(
                    "{} takes on or off, such as {}.",
                    workers::COPIES_COMMAND,
                    workers::copies_command(false)
                ));
            }
        };
        if on == now {
            return Ok(format!(
                "{} are {} already.",
                workers::COPIES_NAME,
                workers::copies_state(on)
            ));
        }
        self.settings.message_copies = on;
        self.save_session();
        self.publish_models();
        Ok(format!(
            "{} are now {}. {}",
            workers::COPIES_NAME,
            workers::copies_state(on),
            workers::copies_about(on)
        ))
    }

    /// `/profiles`, typed at the prompt or sent from the page: the worker
    /// profiles, one profile whole, and making and deleting one.
    pub(crate) fn profiles_command(&mut self, argument: &str) {
        let words: Vec<&str> = argument.split_whitespace().collect();
        let result = match words.as_slice() {
            [] => Ok(self.profiles_list()),
            ["add"] | ["remove"] | ["delete"] => Err(PROFILES_USAGE.to_string()),
            ["add", name, rest @ ..] => self.profiles_add(name, rest),
            ["remove" | "delete", name] => self.profiles_remove(name),
            [name] => self.profiles_show(name),
            _ => Err(PROFILES_USAGE.to_string()),
        };
        self.mirrored(|harness| match result {
            Ok(text) => {
                for line in text.lines() {
                    harness.output.line(line);
                }
            }
            Err(error) => harness.output.line(&harness.output.error(&error)),
        });
    }

    fn profiles_list(&self) -> String {
        let roots = self.profile_roots();
        let listed = profiles::list(&roots);
        if listed.active.is_empty() && listed.unreadable.is_empty() {
            return "No worker profiles yet. /profiles add NAME MODEL creates one, or ask the agent to save one."
                .to_string();
        }
        let worker = &self.settings.worker;
        let reasons = providers::resolve_model(&worker.model)
            .map_or(true, |model| !providers::efforts(&model).is_empty());
        // Each profile is a line with its name, what it runs and where it
        // is, and under it what it is for, so a narrow screen wraps only
        // the words.
        struct Row {
            name: String,
            runs: String,
            place: String,
            about: String,
        }
        let mut rows = vec![Row {
            name: "default".into(),
            runs: crate::worker_tools::runs_text(
                Some(&providers::short_model_name(&worker.model)),
                reasons.then_some(worker.effort),
            ),
            place: String::new(),
            about: "the session's worker model".into(),
        }];
        rows.extend(listed.active.iter().take(MAX_ROWS).map(|found| {
            let profile = &found.profile;
            let mut about = profile.description.clone();
            if profile.made_by_agent() {
                about.push_str(" (made by the main agent)");
            }
            if let Some(note) = &found.note {
                about.push_str(&format!(" (can't start, {note})"));
            }
            Row {
                name: profile.name.clone(),
                runs: crate::worker_tools::runs_text(profile.model.as_deref(), profile.effort),
                place: found.place.reach().to_string(),
                about,
            }
        }));
        let problems: Vec<Row> = listed
            .unreadable
            .iter()
            .take(MAX_UNREADABLE_ROWS)
            .map(|problem| Row {
                name: ui::cut(&problem.name, profiles::MAX_NAME),
                runs: String::new(),
                place: problem.place.reach().to_string(),
                about: problem.reason.clone(),
            })
            .collect();
        let hidden: Vec<Row> = listed
            .hidden
            .iter()
            .take(MAX_UNREADABLE_ROWS)
            .map(|found| Row {
                name: found.profile.name.clone(),
                runs: String::new(),
                place: found.place.reach().to_string(),
                about: String::new(),
            })
            .collect();
        // (A file with a name that isn't a profile's can be any length, so
        // it doesn't make the columns wider.)
        let width = |pick: fn(&Row) -> &str| {
            rows.iter()
                .chain(&hidden)
                .map(|row| pick(row).chars().count())
                .max()
                .unwrap_or(0)
        };
        let (name_width, runs_width) = (width(|row| &row.name), width(|row| &row.runs));
        let write = |text: &mut String, row: &Row| {
            let line = if row.name.chars().count() > name_width {
                format!("  {}  {}", row.name, row.place)
            } else {
                format!(
                    "  {:name_width$}  {:runs_width$}  {}",
                    row.name, row.runs, row.place
                )
            };
            text.push('\n');
            text.push_str(line.trim_end());
            for line in wrap_words(&row.about, ABOUT_WIDTH) {
                text.push_str(&format!("\n      {line}"));
            }
        };
        let mut text = String::from("Worker profiles");
        for row in &rows {
            write(&mut text, row);
        }
        if listed.active.len() > MAX_ROWS {
            text.push_str(&format!(
                "\n  and {} more, which /profiles NAME shows",
                listed.active.len() - MAX_ROWS
            ));
        }
        if !hidden.is_empty() {
            text.push_str("\nHidden by a nearer profile of the same name");
            for row in &hidden {
                write(&mut text, row);
            }
        }
        if !problems.is_empty() {
            text.push_str("\nCan't be read");
            for row in &problems {
                write(&mut text, row);
            }
            if listed.unreadable.len() > MAX_UNREADABLE_ROWS {
                text.push_str(&format!(
                    "\n  and {} more",
                    listed.unreadable.len() - MAX_UNREADABLE_ROWS
                ));
            }
        }
        // (A path on its own line, since it is longer than a screen can be.)
        text.push_str("\n/profiles NAME shows one. The files are in");
        for (place, folder) in profiles::folders(&roots) {
            text.push_str(&format!("\n  {}\n    {}", place.reach(), folder.display()));
        }
        text
    }

    fn profiles_show(&self, name: &str) -> Result<String, String> {
        if name == "default" {
            let worker = &self.settings.worker;
            return Ok(format!(
                "default · the session's worker model\nIt runs {}. /model worker changes it. It has no file.",
                self.settings.describe(worker)
            ));
        }
        let found = profiles::read(&self.profile_roots(), name).map_err(|error| match error {
            profiles::ReadError::Missing => {
                format!("There is no worker profile `{name}`. /profiles lists them.")
            }
            other => other.to_string(),
        })?;
        let profile = &found.profile;
        let mut text = format!(
            "{} · for {}\n{}\n{}\nRuns on {}",
            profile.name,
            found.place.reach(),
            found.file.display(),
            profile.description,
            profile.runs_on()
        );
        if profile.fast {
            text.push_str(" in fast mode");
        }
        if let Some(cache) = profile.cache {
            text.push_str(&format!(" with a {} prompt cache", cache.as_str()));
        }
        text.push('.');
        if profile.nested {
            text.push_str(" A worker started with it can start workers of its own.");
        }
        if profile.made_by_agent() {
            text.push_str(" The main agent made it.");
        }
        if let Some(note) = &found.note {
            text.push_str(&format!("\nIt can't start workers yet, since {note}"));
        }
        if profile.instructions.is_empty() {
            text.push_str("\nIt has no instructions.");
        } else {
            text.push_str(&format!("\nInstructions\n{}", profile.instructions));
        }
        Ok(text)
    }

    /// `/profiles add NAME MODEL [EFFORT] [DESCRIPTION]`: writes the file in
    /// Arko's folder, with an empty body, for the user to edit.
    fn profiles_add(&self, name: &str, rest: &[&str]) -> Result<String, String> {
        let Some((model, rest)) = rest.split_first() else {
            return Err(PROFILES_USAGE.to_string());
        };
        let (effort, description) = match rest.split_first() {
            Some((word, after)) if word.parse::<agents::Effort>().is_ok() => {
                (word.parse::<agents::Effort>().ok(), after)
            }
            _ => (None, rest),
        };
        let description = description.join(" ");
        let placeholder = description.is_empty();
        let mut profile = Profile::new(
            name,
            if placeholder {
                PLACEHOLDER.to_string()
            } else {
                description
            },
        );
        profile.model = Some(model.to_string());
        profile.effort = effort;
        profile.check()?;
        let roots = self.profile_roots();
        let user_folder = roots.folder(Place::User);
        if user_folder
            .as_ref()
            .is_some_and(|folder| folder.join(Profile::file_name(name)).is_file())
        {
            return Err(format!(
                "There is a profile `{name}` for every project already. Edit its file, or /profiles remove {name} first."
            ));
        }
        let saved = profiles::save(&roots, Place::User, &profile, Author::User)?;
        let mut text = format!(
            "Saved the worker profile {name} for every project in {}. Edit the file to write its description and instructions.",
            saved.file.display()
        );
        if placeholder {
            text.push_str(&format!(
                " Until you do, the main agent reads \"{PLACEHOLDER}\" as its description."
            ));
        }
        if let Some(nearer) = saved.hidden_by {
            text.push_str(&format!(
                " A profile of the same name for {} comes first, so it hides this one.",
                nearer.reach()
            ));
        }
        Ok(text)
    }

    /// `/profiles remove NAME`: deletes the file of the profile `/profiles`
    /// shows under that name.
    fn profiles_remove(&self, name: &str) -> Result<String, String> {
        let roots = self.profile_roots();
        // A file that can't be read is removed too, since the user has no
        // other way to clear it from the list.
        let (place, unreadable) = match profiles::read(&roots, name) {
            Ok(found) => (found.place, false),
            Err(profiles::ReadError::Unreadable(problem)) => (problem.place, true),
            Err(profiles::ReadError::Missing) => {
                return Err(format!(
                    "There is no worker profile `{name}`. /profiles lists them."
                ));
            }
            Err(other) => return Err(other.to_string()),
        };
        let file = profiles::delete(&roots, place, name)?;
        Ok(if unreadable {
            format!(
                "Removed {} for {}, which couldn't be read.",
                file.display(),
                place.reach()
            )
        } else {
            format!(
                "Removed the worker profile {name} for {} ({}). The workers started with it keep their setup.",
                place.reach(),
                file.display()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The main agent is told of a new depth, unless every worker runs the
    /// session's model, which leaves workers that start workers out.
    #[test]
    fn a_new_depth_is_told_to_the_main_agent_unless_no_worker_nests() {
        assert_eq!(depth_note(2, ModelChoice::Session), None);
        assert_eq!(depth_note(1, ModelChoice::Session), None);
        assert_eq!(
            depth_note(1, ModelChoice::Agent).unwrap(),
            "The user now allows 1 level of workers. A worker can't start workers of its own, so start workers without nested."
        );
        for choice in [ModelChoice::Agent, ModelChoice::Profiles] {
            assert_eq!(
                depth_note(3, choice).unwrap(),
                "The user now allows 3 levels of workers. A worker you start with nested true can start workers of its own, down to level 3."
            );
        }
    }
}

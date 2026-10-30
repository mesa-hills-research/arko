//! A new conversation that carries the current one over: `/model MODEL`
//! once the conversation has started, and `/new continue` on the same model.
//!
//! The main agent's runs keep their configuration, so a model can't change
//! inside a conversation. A switch starts a new conversation on the new
//! model instead, as `/new` does, with the running workers moving across.
//! Its first message opens with a brief Arko writes of the old conversation
//! without any model (see `earlier`), and it is offered `earlier_conversation`
//! to read the old one as it needs. When the old model can still write, its
//! own report goes in the brief first: Arko asks for it in one more turn of
//! the old conversation, which Ctrl-C skips. A plain `/new` stays a clean
//! start, and so does `/model MODEL` after it (it only sets the model of the
//! conversation to come). The old conversation stays as it was, on its own
//! model, for `/resume`.

use std::sync::{Arc, Mutex, Weak};

use arko_ai::Model;
use arko_workflows::prelude::*;
use uuid::Uuid;

use crate::agents::{Effort, ModelSettings};
use crate::conversations::Conversations;
use crate::earlier;
use crate::inputs::{Event, Inputs};
use crate::remote::{Action, Level, Source};
use crate::session::Settings;
use crate::{Conversation, Harness, Input, lock, providers};

/// What carries the old conversation into the next one: the brief its
/// first message opens with, and which conversation it continues.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Carry {
    /// The brief, under "# Earlier conversation".
    pub brief: String,
    /// The conversation it continues, by its place in the record.
    pub from: usize,
}

/// A new conversation that carries the current one over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Switch {
    /// `/model MODEL [EFFORT]` once the conversation has started.
    Model {
        model: String,
        effort: Option<Effort>,
    },
    /// `/new continue`.
    Continue,
}

/// What `/new` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum New {
    /// A clean start.
    Clean,
    /// A new conversation that carries the current one over.
    Continue,
}

/// What `/new` in `text` asks for, when `text` is that command: plain, or
/// with `continue`. Another word is an error that says what it takes.
pub(crate) fn new_argument(text: &str) -> Option<Result<New, String>> {
    let rest = text.strip_prefix("/new")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(match rest.trim() {
        "" => Ok(New::Clean),
        "continue" => Ok(New::Continue),
        _ => Err(
            "/new takes continue, which carries this conversation over (/new continue)."
                .to_string(),
        ),
    })
}

/// The request for the old model's report, which says how to give it:
/// `with_tool` with `handover_report`, else as a reply. `to` is the new
/// conversation's model when the switch changes it.
pub(crate) fn report_request(with_tool: bool, to: Option<&str>) -> String {
    let why = match to {
        Some(model) => format!(
            "The user is moving this work to a new conversation on {model}, which goes on from a report you write now."
        ),
        None => "The user is starting a new conversation that goes on from a report you write now."
            .to_string(),
    };
    let how = if with_tool {
        "Call `handover_report` with it. Your other tool calls are not run until you do."
    } else {
        "Reply with it as plain text, without calling a tool."
    };
    format!(
        "{why} {how} Cover what the task is, what you have done and the evidence for it (files, commands, results), decisions you made and why, what you were doing and what you meant to do next, and what you are unsure of. List anything you are still waiting for, such as a reply to a question you asked or a worker's report, and every message you received that you haven't fully answered yet, with who sent it and what you meant to reply. The new conversation can read this one, so point to the turns that matter rather than copying them."
    )
}

/// Why the old model couldn't write its report, from the error its request
/// ended in, as the switch's line says it: "plan limit reached".
pub(crate) fn report_failure(error: &WorkflowError) -> String {
    use arko_agents::ErrorRecoveryCause;
    match error {
        WorkflowError::AgentPaused {
            pause:
                arko_workflows::policy::AgentPause::ErrorRecovery {
                    cause, error_kind, ..
                },
            ..
        } => match (cause, error_kind) {
            (ErrorRecoveryCause::UsageLimit { .. }, _) => "plan limit reached".to_string(),
            (_, Some(arko_ai::ErrorKind::Authentication)) => {
                "its key or login was refused".to_string()
            }
            (ErrorRecoveryCause::RetriesExhausted { .. }, _) => {
                "its requests kept failing".to_string()
            }
            (ErrorRecoveryCause::Refusal, _) => "the model refused".to_string(),
            _ => "its request failed".to_string(),
        },
        WorkflowError::AgentStopped { .. } | WorkflowError::Cancelled => "skipped".to_string(),
        other => crate::ui::cut(&other.to_string(), 160),
    }
}

/// How a run that can't write a report on request ended, for the line that
/// says why there is none. `None` for a run that can.
fn unable(result: &AgentResult) -> Option<&'static str> {
    match result.status {
        AgentRunStatus::Completed | AgentRunStatus::WaitingForInput | AgentRunStatus::Cancelled => {
            None
        }
        AgentRunStatus::Paused => Some("its last turn is paused"),
        AgentRunStatus::Interrupted => Some("its last turn was cut off when Arko closed"),
        _ => Some("its last turn failed"),
    }
}

/// A worker's state as the brief names it.
fn state_word(worker: &arko_workflows::subagents::SubagentRecord) -> &'static str {
    use arko_workflows::subagents::SubagentState;
    match worker.state {
        _ if worker.asking => "waiting for an answer",
        SubagentState::Queued => "queued",
        SubagentState::Running => "running",
        SubagentState::Finished => "done",
        SubagentState::Closed if worker.report.is_some() => "done",
        SubagentState::Failed => "failed",
        SubagentState::Stopped | SubagentState::Interrupted | SubagentState::Closed => "stopped",
    }
}

/// Whether the conversation `task` holds changed since its latest handover
/// report: the agent wrote something after it.
fn worked_since_report(task: &arko_agents::Task) -> bool {
    task.conversation
        .messages
        .iter()
        .any(|message| message.role == arko_ai::Role::Assistant)
}

/// The session's conversations as `earlier_conversation` reads them: the
/// record's runs, which the harness keeps current here, read from the
/// workflow.
#[derive(Debug, Default)]
pub(crate) struct Earlier {
    workflow: Mutex<Weak<Workflow>>,
    record: Mutex<Shared>,
}

#[derive(Debug, Default)]
struct Shared {
    /// Each conversation's runs, and the one it continues, in the record's
    /// order.
    conversations: Vec<(Vec<Uuid>, Option<usize>)>,
    /// The conversation the next new one continues (a switch was made and
    /// its first message hasn't been recorded yet).
    pending: Option<usize>,
}

impl Earlier {
    /// Reads the session's conversations from `workflow` from now on.
    pub(crate) fn attach(&self, workflow: &Arc<Workflow>) {
        *lock(&self.workflow) = Arc::downgrade(workflow);
    }

    /// The record as it is now.
    pub(crate) fn update(&self, conversations: &Conversations) {
        *lock(&self.record) = Shared {
            conversations: conversations
                .list
                .iter()
                .map(|record| (record.runs.clone(), record.continues))
                .collect(),
            pending: conversations
                .continuing
                .or(conversations.carry.as_ref().map(|carry| carry.from)),
        };
    }
}

impl earlier::Saved for Earlier {
    fn count(&self) -> usize {
        lock(&self.record).conversations.len()
    }

    fn runs(&self, number: usize) -> Result<Vec<AgentResult>, String> {
        let runs = number
            .checked_sub(1)
            .and_then(|index| lock(&self.record).conversations.get(index).cloned())
            .map(|(runs, _)| runs)
            .ok_or_else(|| {
                format!(
                    "There is no conversation {number}. This session has {}.",
                    self.count()
                )
            })?;
        let workflow = lock(&self.workflow)
            .upgrade()
            .ok_or_else(|| "The session is closing.".to_string())?;
        Ok(runs
            .iter()
            .filter_map(|run| workflow.agent_result(*run))
            .collect())
    }

    fn continued(&self, run: Uuid) -> Option<usize> {
        let record = lock(&self.record);
        match record
            .conversations
            .iter()
            .find(|(runs, _)| runs.contains(&run))
        {
            Some((_, continues)) => continues.map(|index| index + 1),
            // Its first turn, before the record has it.
            None => record.pending.map(|index| index + 1),
        }
    }
}

/// What a switch changes the session to: its settings, and the agents'
/// configurations built from them.
struct Target {
    settings: Settings,
    setup: providers::Setup,
    main_config: arko_agents::AgentConfig,
    worker_config: arko_agents::AgentConfig,
    /// New workers follow the main agent's model, which changed.
    workers_follow: bool,
}

impl Harness {
    /// The switch `command` asks for, at the prompt between turns:
    /// `/new continue`, and `/model MODEL` once the conversation has started
    /// (before, `/model` builds the agents again on the model).
    pub(crate) fn switch_of(&self, command: &str) -> Option<Switch> {
        if let Some(Ok(New::Continue)) = new_argument(command) {
            return Some(Switch::Continue);
        }
        let argument = command.strip_prefix("/model")?;
        if !argument.is_empty() && !argument.starts_with(char::is_whitespace) {
            return None;
        }
        match crate::model_command::parse(argument) {
            Ok(crate::model_command::Request::Set {
                target: crate::model_command::Target::Main,
                model,
                effort,
            }) if self.conversation_started() => Some(Switch::Model { model, effort }),
            _ => None,
        }
    }

    /// The model of the conversation the next message goes to, as `/model`
    /// takes it: the current conversation's, else the one a new conversation
    /// starts on.
    pub(crate) fn conversation_model(&self) -> String {
        self.conversations
            .current_run()
            .and_then(|run| self.workflow.as_ref()?.agent_result(run))
            .map(|result| providers::model_name(&result.info().agent_config.llm_config.model))
            .unwrap_or_else(|| self.settings.main.model.clone())
    }

    /// Makes `switch`: a new conversation that carries the current one
    /// over, or (`/model` with none current, after `/new`) sets the model
    /// of the conversation to come. Returns whether the user quit while
    /// the old model wrote its report.
    pub(crate) async fn switch(
        &mut self,
        switch: Switch,
        source: Source,
        input: &mut Inputs,
    ) -> bool {
        let current = self.conversations.current;
        let latest = self
            .conversations
            .current_run()
            .and_then(|run| self.workflow().agent_result(run));
        let (model, effort) = match &switch {
            Switch::Model { model, effort } => (model.clone(), *effort),
            Switch::Continue => {
                let Some(latest) = &latest else {
                    self.mirrored(|harness| {
                        harness.output.line(&harness.output.error(
                            "There is no conversation to continue, since the next message starts a new one. /resume goes back to one first.",
                        ))
                    });
                    return false;
                };
                let model = &latest.info().agent_config.llm_config.model;
                (providers::model_name(model), None)
            }
        };
        let target = match self.switch_target(&model, effort) {
            Ok(target) => target,
            Err(error) => {
                self.mirrored(|harness| harness.output.line(&harness.output.error(&error)));
                return false;
            }
        };
        let (Some(index), Some(latest)) = (current, latest) else {
            // After `/new`: the next conversation starts clean on the model.
            let text = self.apply_target(target);
            self.setting_reply(Ok(text), source, true);
            return false;
        };
        let old_model = providers::model_name(&latest.info().agent_config.llm_config.model);
        let changes = matches!(switch, Switch::Model { .. });
        // The old model's report, when it can write one.
        let (report, quit) = self
            .old_report(
                &latest,
                &old_model,
                changes.then_some(target.settings.main.model.as_str()),
                input,
            )
            .await;
        let report = match report {
            Ok(report) => report,
            Err(why) => {
                if let Some(why) = why {
                    self.say(
                        Level::Warn,
                        &format!("{old_model} couldn't write a report ({why})."),
                    );
                }
                None
            }
        };
        // (Of the conversation as it was before the report, which it holds.)
        let brief = self.brief_of(index, &latest, &old_model, report.as_deref(), &target);
        // The conversation the new one continues is left, as `/new` leaves it.
        let follow = target.workers_follow;
        let new_model = target.settings.main.clone();
        self.apply_target(target);
        self.start_new_conversation();
        self.conversations.carry = Some(Carry { brief, from: index });
        self.save_conversations();
        self.update_earlier();
        let line = self.switch_line(
            changes.then_some(&new_model),
            report.is_some(),
            index,
            follow,
        );
        self.feed.conversation(&match changes {
            true => format!("New conversation on {}", new_model.model),
            false => "New conversation".to_string(),
        });
        self.say(Level::Info, &line);
        quit
    }

    /// The settings and configurations `/model` with `model` at `effort`
    /// changes the session to, or what it needs that the session doesn't
    /// have.
    fn switch_target(&self, model: &str, effort: Option<Effort>) -> Result<Target, String> {
        let resolved = providers::resolve_model(model)?;
        self.can_reach_model(&resolved)?;
        let resolved = match resolved.provider {
            arko_ai::Provider::OpenRouter => {
                crate::openrouter::Settings::read_to_use(&self.root)?;
                crate::openrouter::known(&self.root, &resolved.id).unwrap_or(resolved)
            }
            _ => resolved,
        };
        let effort = match effort {
            Some(effort) if providers::efforts(&resolved).is_empty() => effort,
            Some(effort) if !providers::efforts(&resolved).contains(&effort) => {
                return Err(format!(
                    "{} takes {} effort, not {effort}.",
                    providers::model_name(&resolved),
                    providers::efforts_text(&resolved)
                ));
            }
            Some(effort) => effort,
            None => providers::fit_effort(&resolved, self.settings.main.effort),
        };
        let (settings, workers_follow) = self.main_switched(&resolved, effort);
        let setup = providers::Setup {
            openai_auth: settings.openai_auth,
            claude_code: self.providers.claude_code.clone(),
        };
        let (main_config, worker_config) = self.agent_configs(&settings, &setup)?;
        Ok(Target {
            settings,
            setup,
            main_config,
            worker_config,
            workers_follow,
        })
    }

    /// The session takes `target`'s settings and configurations without
    /// building the workflow again: new conversations start on its main
    /// configuration, and new workers that follow the main agent on its
    /// model. Returns what to say.
    fn apply_target(&mut self, target: Target) -> String {
        let model_changed = target.settings.main != self.settings.main;
        self.settings = target.settings;
        self.providers = target.setup;
        self.main_config = target.main_config;
        self.worker_config = target.worker_config;
        self.main_reconfigured = true;
        if target.workers_follow
            && let Some(crew) = self.crew.clone()
        {
            if let Err(error) = crew.keep_started_models() {
                log::warn!("could not keep the started workers' models: {error}");
            }
            self.start_new_workers();
        }
        // The next conversation's handover size, compaction and window.
        self.handover
            .configure_for(&self.main_config, self.settings.openai_auth);
        self.save_session();
        if model_changed
            && !self.one_shot
            && let Err(error) = providers::LastModels::remember(&self.root, &self.settings)
        {
            log::warn!("could not keep the session's models for the next one: {error}");
        }
        self.publish_models();
        let shown = format!(
            "{} ({} effort)",
            self.settings.main.model, self.settings.main.effort
        );
        if target.workers_follow {
            format!("The next conversation runs {shown}, and new workers follow it.")
        } else {
            format!("The next conversation runs {shown}.")
        }
    }

    /// The old conversation's latest run `latest` (on `old_model`) writes a
    /// report for the new conversation (on `to`, when the switch changes
    /// the model), unless it can't or has nothing new to say. The line says
    /// it asks, and Ctrl-C (or the page's stop) skips it. Returns the
    /// report, or why there is none (`None`: nothing to say), and whether
    /// the user quit meanwhile.
    async fn old_report(
        &mut self,
        latest: &AgentResult,
        old_model: &str,
        to: Option<&str>,
        input: &mut Inputs,
    ) -> (Result<Option<String>, Option<String>>, bool) {
        let Ok(task) = latest.task() else {
            return (Err(Some("its messages can't be read".into())), false);
        };
        if !worked_since_report(task) {
            // Its latest handover report is the brief's.
            return (Ok(None), false);
        }
        let config = &latest.info().agent_config;
        if let Some(needs) = self.needs(&config.llm_config.model) {
            return (Err(Some(crate::ui::cut(&needs, 160))), false);
        }
        if let Some(why) = unable(latest) {
            return (Err(Some(why.to_string())), false);
        }
        let with_tool = config.handover.is_some() && !config.runs_in_claude_code();
        let request = report_request(with_tool, to);
        let workflow = self.workflow().clone();
        let run = latest.agent_run_id;
        let asking = {
            let workflow = workflow.clone();
            let latest = latest.clone();
            tokio::spawn(async move { workflow.request_report(&latest, Some(&request)).await })
        };
        tokio::pin!(asking);
        self.say(
            Level::Info,
            &format!("Asking {old_model} for a report (Ctrl-C skips it)."),
        );
        self.feed.reporting(true);
        let mut skipped = false;
        let mut quit = false;
        // What the user sends meanwhile goes to the new conversation, read
        // once the switch is made, in the order it came.
        let mut held: Vec<Event> = Vec::new();
        let outcome = loop {
            tokio::select! {
                outcome = &mut asking => break outcome,
                event = input.next_event(false) => {
                    let skip = match event {
                        Event::Input(Some(Input::Interrupt)) | Event::Remote(Action::Stop) => true,
                        Event::Input(Some(Input::Eof) | None) => {
                            quit = true;
                            true
                        }
                        Event::Input(Some(Input::Quit(agreed))) => {
                            quit = self.mirrored(|harness| harness.may_exit(agreed));
                            quit
                        }
                        Event::Request(request) => {
                            Box::pin(self.request(request, input.waiting())).await;
                            false
                        }
                        Event::Question(question) => {
                            if !question.answer.as_ref().is_some_and(|answer| answer.is_closed()) {
                                self.question_asked(question);
                            }
                            false
                        }
                        Event::Input(Some(Input::Background(agent))) => {
                            self.move_to_background(&agent);
                            false
                        }
                        Event::Input(Some(Input::AnswerApprovals(answers))) => {
                            self.answer_approvals(answers, Some(self.input_source));
                            false
                        }
                        Event::Remote(Action::AnswerApprovals(answers)) => {
                            self.answer_approvals(answers, Some(Source::Web));
                            false
                        }
                        Event::Remote(
                            action @ (Action::Background { .. } | Action::StopJob { .. }),
                        ) => {
                            self.page_job_action(action);
                            false
                        }
                        Event::Woken | Event::Finished(_) | Event::Alert(_) => false,
                        // Messages, answers and lines: for the new conversation.
                        event @ (Event::Input(Some(
                            Input::Line(_)
                            | Input::Message(_)
                            | Input::Answer { .. }
                            | Input::AnswerQuestions(_)
                            | Input::DismissQuestions(_),
                        ))
                        | Event::Remote(
                            Action::Message(_)
                            | Action::Reply { .. }
                            | Action::Answer { .. }
                            | Action::AnswerQuestions(_)
                            | Action::DismissQuestions(_),
                        )) => {
                            if held.is_empty() {
                                self.say(
                                    Level::Info,
                                    "  (what you send goes to the new conversation, once the report is in or skipped)",
                                );
                            }
                            held.push(event);
                            false
                        }
                    };
                    // A stop that doesn't take (the report's run isn't under
                    // way yet) ends the request before it starts.
                    if skip && !skipped {
                        skipped = true;
                        let reason = if quit { "The user quit." } else { "The user skipped the report." };
                        if !workflow.stop_run(
                            run,
                            StopRequest::new(StopKind::Operator, reason),
                            RunStopIntent::Relaunch,
                        ) {
                            asking.abort();
                        }
                    }
                }
            }
        };
        input.held.extend(held);
        self.feed.reporting(false);
        self.flush_printer().await;
        let outcome = outcome.unwrap_or_else(|error| {
            Err(WorkflowError::Channel(format!(
                "the report's task failed ({error})"
            )))
        });
        let report = match outcome {
            Ok(result) => {
                let report = result.summary.trim().to_string();
                // The old conversation goes on from it, for `/resume`.
                if self.conversations.current_run() == Some(result.agent_run_id) {
                    self.conversation = Conversation::Answered(result);
                }
                Ok(Some(report).filter(|report| !report.is_empty()))
            }
            Err(_) if skipped => {
                if !quit {
                    self.say(Level::Info, &format!("Skipped {old_model}'s report."));
                }
                Err(None)
            }
            Err(error) => Err(Some(report_failure(&error))),
        };
        (report, quit)
    }

    /// The brief of the conversation at `index` (on `old_model`), with
    /// `report`, sized to `target`'s main model. Its latest run is read as
    /// `latest` has it: before the request for the report and the report,
    /// which the brief holds in a section of its own.
    fn brief_of(
        &self,
        index: usize,
        latest: &AgentResult,
        old_model: &str,
        report: Option<&str>,
        target: &Target,
    ) -> String {
        let workflow = self.workflow().clone();
        let results: Vec<AgentResult> = self.conversations.list[index]
            .runs
            .iter()
            .filter_map(|run| match *run == latest.agent_run_id {
                true => Some(latest.clone()),
                false => workflow.agent_result(*run),
            })
            .collect();
        let tasks: Vec<&arko_agents::Task> = results
            .iter()
            .filter_map(|result| result.task().ok())
            .collect();
        let runs: Vec<Uuid> = results.iter().map(|result| result.agent_run_id).collect();
        let workers: Vec<earlier::Worker> = workflow
            .subagents()
            .into_iter()
            .filter(|worker| runs.contains(&worker.caller_run_id))
            .map(|worker| earlier::Worker {
                handle: worker.handle.clone(),
                state: state_word(&worker).to_string(),
                task: crate::ui::cut(
                    worker
                        .task
                        .lines()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or(""),
                    160,
                ),
            })
            .collect();
        let mut open = earlier::Open {
            questions: tasks
                .last()
                .map(|task| {
                    task.questions
                        .open
                        .iter()
                        .map(|asked| asked.question.question.clone())
                        .collect()
                })
                .unwrap_or_default(),
            unread: self.unread.clone(),
            ended: None,
        };
        if let Some(latest) = results.last() {
            open.ended = match latest.status {
                AgentRunStatus::Cancelled => Some("stopped by the user".to_string()),
                AgentRunStatus::Paused => Some("paused".to_string()),
                AgentRunStatus::Interrupted => Some("cut off when Arko closed".to_string()),
                AgentRunStatus::Failed => Some("failed".to_string()),
                _ => None,
            };
        }
        let model = &target.main_config.llm_config.model;
        let chatgpt = model.provider == arko_ai::Provider::OpenAI
            && target.settings.openai_auth == Some(providers::OpenAiAuth::ChatGpt);
        let limit = arko_ai::input_limit::effective_max_input_tokens(model, chatgpt);
        earlier::brief(&earlier::BriefParts {
            number: index + 1,
            model: old_model,
            runs: &tasks,
            report,
            workers: &workers,
            open: &open,
            budget: earlier::budget(limit),
        })
    }

    /// The line that says what the switch did: the new conversation, on
    /// `model` when it changed, from the old model's report (`reported`)
    /// and a brief of conversation `index`, the workers that move across,
    /// and (`follow`) that new workers follow the model.
    fn switch_line(
        &self,
        model: Option<&ModelSettings>,
        reported: bool,
        index: usize,
        follow: bool,
    ) -> String {
        let on = model.map_or(String::new(), |model| {
            format!(" on {} ({} effort)", model.model, model.effort)
        });
        let from = if reported {
            let old = self
                .conversations
                .list
                .get(index)
                .and_then(|record| record.runs.last())
                .and_then(|run| self.workflow().agent_result(*run))
                .map(|result| providers::model_name(&result.info().agent_config.llm_config.model))
                .unwrap_or_default();
            format!(
                "from {old}'s report and a brief of conversation {}",
                index + 1
            )
        } else {
            format!("from a brief of conversation {}", index + 1)
        };
        let mut line = format!("New conversation{on}, {from}.");
        let running = self
            .crew
            .as_ref()
            .map(|crew| crew.running())
            .unwrap_or_default();
        match running.as_slice() {
            [] => {}
            [one] => line.push_str(&format!(" Worker {one} keeps running and reports here.")),
            [rest @ .., last] => line.push_str(&format!(
                " Workers {} and {last} keep running and report here.",
                rest.join(", ")
            )),
        }
        if follow && model.is_some() {
            line.push_str(" New workers without a model of their own follow it.");
        }
        line
    }

    /// The conversations `earlier_conversation` reads, as the record has
    /// them now.
    pub(crate) fn update_earlier(&self) {
        self.earlier.update(&self.conversations);
    }

    /// The configuration the next new conversation starts on, when it isn't
    /// the one the workflow was built with: after a switch, and with
    /// `earlier_conversation` when it carries an earlier one over. That one
    /// takes the carry, and is recorded as continuing it.
    pub(crate) fn new_conversation_config(&mut self) -> Option<arko_agents::AgentConfig> {
        let carry = self.conversations.carry.take();
        if carry.is_none() && !self.main_reconfigured {
            return None;
        }
        let mut config = self.main_config.clone();
        if let Some(carry) = carry {
            config
                .tool_config
                .available_tool_names
                .push(earlier::TOOL_NAME.to_string());
            self.conversations.continuing = Some(carry.from);
            self.save_conversations();
        }
        Some(config)
    }

    /// Whether the session can reach `model`, or what it needs.
    fn can_reach_model(&self, model: &Model) -> Result<(), String> {
        match self.needs(model) {
            Some(needs) => Err(providers::needs_text(&providers::model_name(model), &needs)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_takes_continue_or_nothing() {
        assert_eq!(new_argument("/new"), Some(Ok(New::Clean)));
        assert_eq!(new_argument("/new  continue "), Some(Ok(New::Continue)));
        assert!(new_argument("/new again").unwrap().is_err());
        assert_eq!(new_argument("/news"), None);
        assert_eq!(new_argument("/resume"), None);
    }

    #[test]
    fn the_report_request_says_how_to_give_it_and_why() {
        let tool = report_request(true, Some("gpt-6.1-sol"));
        assert!(tool.contains("a new conversation on gpt-6.1-sol"), "{tool}");
        assert!(tool.contains("Call `handover_report` with it."));
        let text = report_request(false, None);
        assert!(text.starts_with("The user is starting a new conversation"));
        assert!(text.contains("Reply with it as plain text"));
        for request in [tool, text] {
            assert!(!request.contains(';'), "{request}");
        }
    }
}

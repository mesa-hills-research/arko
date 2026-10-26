//! What the session waits for between and during turns: the user's input,
//! the main agent's `ask_user` questions, finished jobs, monitors' alerts
//! and the crew's wake-ups.

use std::time::Duration;

use arko_workflows::prelude::*;
use arko_workflows::user_input::{UserInputHandler, UserInputRequest};
use tokio::sync::{mpsc, oneshot};

use crate::agents::MAIN;
use crate::engine::Request;
use crate::remote::Action;
use crate::ui::Input;

/// The user's input, read on its own thread; the main agent's questions;
/// and the crew's wake-ups.
pub(crate) struct Inputs {
    pub(crate) receiver: mpsc::UnboundedReceiver<Input>,
    /// An input read ahead while gathering a pasted block.
    pub(crate) pending: Option<Input>,
    pub(crate) questions: mpsc::UnboundedReceiver<Question>,
    /// The crew says the main agent has news, or a worker stopped running.
    pub(crate) woken: Option<mpsc::UnboundedReceiver<()>>,
    /// What the remote page sends.
    pub(crate) remote: mpsc::UnboundedReceiver<Action>,
    /// What the front end asks of the session.
    pub(crate) requests: mpsc::UnboundedReceiver<Request>,
    /// The terminal's jobs that finish in the background; `None` with -p,
    /// where they wake no one.
    pub(crate) finished:
        Option<tokio::sync::broadcast::Receiver<arko_agents::terminal::JobFinished>>,
    /// A job that finished during the agent's last turn, too late for it
    /// to read its update (see `waits_with`).
    pub(crate) late: Option<arko_agents::terminal::JobFinished>,
    /// The monitors' news that should wake an idle agent. `None` with -p,
    /// where it wakes no one.
    pub(crate) alerts: Option<tokio::sync::broadcast::Receiver<arko_agents::terminal::JobAlert>>,
    /// A monitor's news that came during the agent's last request, too late
    /// for it to read (see `alert_waits`).
    pub(crate) late_alert: Option<Alert>,
}

/// A monitor's news for an idle agent.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Alert {
    /// Whose monitor it is.
    pub(crate) owner: uuid::Uuid,
    /// The monitor, when known.
    pub(crate) job: Option<uuid::Uuid>,
    /// The monitor ends soon (else it alerted).
    pub(crate) ends_soon: bool,
    /// It came during the agent's last request, and its task holds it.
    pub(crate) late: bool,
}

impl Alert {
    fn of(alert: &arko_agents::terminal::JobAlert) -> Self {
        Self {
            owner: alert.owner,
            job: Some(alert.job_id),
            ends_soon: matches!(alert.kind, arko_agents::terminal::AlertKind::EndsSoon),
            late: false,
        }
    }
}

/// What [`Inputs::next_event`] returns.
pub(crate) enum Event {
    /// An input; `None` once the input thread is gone.
    Input(Option<Input>),
    /// The main agent asks the user something.
    Question(Question),
    /// The crew woke Arko up.
    Woken,
    /// A job finished in the background.
    Finished(arko_agents::terminal::JobFinished),
    /// A monitor has news for an idle agent.
    Alert(Alert),
    /// The remote page sent something.
    Remote(Action),
    /// The front end asks something of the session.
    Request(Request),
}

impl Inputs {
    /// The next input, question, action from the remote page, or (when
    /// `woken`) wake-up, job finished or monitor's alert. It's cancel-safe.
    pub(crate) async fn next_event(&mut self, woken: bool) -> Event {
        if let Some(input) = self.pending.take() {
            return Event::Input(Some(input));
        }
        if woken && let Some(job) = self.late.take() {
            return Event::Finished(job);
        }
        if woken && let Some(alert) = self.late_alert.take() {
            return Event::Alert(alert);
        }
        tokio::select! {
            input = self.receiver.recv() => Event::Input(input),
            Some(question) = self.questions.recv() => Event::Question(question),
            Some(action) = self.remote.recv() => Event::Remote(action),
            Some(request) = self.requests.recv() => Event::Request(request),
            () = wake(&mut self.woken), if woken => Event::Woken,
            finished = job_finished(&mut self.finished), if woken => Event::Finished(finished),
            alert = job_alert(&mut self.alerts), if woken => Event::Alert(alert),
        }
    }

    /// Whether input waits to be read: what the user typed or the web page
    /// sent, or a finished job's update or a monitor's news the agent
    /// hasn't had.
    pub(crate) fn waiting(&self) -> bool {
        self.pending.is_some()
            || self.late.is_some()
            || self.late_alert.is_some()
            || self
                .alerts
                .as_ref()
                .is_some_and(|alerts| !alerts.is_empty())
            || !self.receiver.is_empty()
            || !self.remote.is_empty()
            || self
                .finished
                .as_ref()
                .is_some_and(|finished| !finished.is_empty())
    }

    /// Forgets the wake-ups so far: what they were for is being seen to.
    pub(crate) fn drain_woken(&mut self) {
        if let Some(woken) = &mut self.woken {
            while woken.try_recv().is_ok() {}
        }
    }

    /// Forgets the jobs that finished so far, and returns them: their
    /// updates reached the agent, or go with the turn starting.
    pub(crate) fn drain_finished(&mut self) -> Vec<arko_agents::terminal::JobFinished> {
        let mut jobs: Vec<_> = self.late.take().into_iter().collect();
        if let Some(finished) = &mut self.finished {
            loop {
                match finished.try_recv() {
                    Ok(job) => jobs.push(job),
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
        jobs
    }

    /// Forgets the monitors' alerts so far: their news reaches the agent
    /// with the turn starting.
    pub(crate) fn drain_alerts(&mut self) {
        self.late_alert = None;
        if let Some(alerts) = &mut self.alerts {
            while let Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) =
                alerts.try_recv()
            {}
        }
    }

    /// A pasted block arrives as several lines in quick succession (where
    /// the terminal doesn't mark pastes): they are one message.
    pub(crate) async fn coalesce(&mut self, first: String) -> String {
        let mut text = first;
        while self.pending.is_none() {
            match tokio::time::timeout(Duration::from_millis(40), self.receiver.recv()).await {
                Ok(Some(Input::Line(line))) => {
                    text.push('\n');
                    text.push_str(&line);
                }
                Ok(Some(other)) => self.pending = Some(other),
                Ok(None) => self.pending = Some(Input::Eof),
                Err(_) => break,
            }
        }
        text
    }
}

/// The next job that finishes in the background; never, without a
/// terminal to hear from. (Missed ones are told by the next: a turn they
/// start gets all their updates.)
pub(crate) async fn job_finished(
    finished: &mut Option<tokio::sync::broadcast::Receiver<arko_agents::terminal::JobFinished>>,
) -> arko_agents::terminal::JobFinished {
    if let Some(receiver) = finished {
        loop {
            match receiver.recv().await {
                Ok(job) => return job,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        *finished = None;
    }
    std::future::pending().await
}

/// The next monitor's alert for an idle agent, or never, without a
/// terminal to hear from.
pub(crate) async fn job_alert(
    alerts: &mut Option<tokio::sync::broadcast::Receiver<arko_agents::terminal::JobAlert>>,
) -> Alert {
    if let Some(receiver) = alerts {
        loop {
            match receiver.recv().await {
                Ok(alert) => return Alert::of(&alert),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        *alerts = None;
    }
    std::future::pending().await
}

/// The notes of the turn (`notes`, with their sequence numbers) that the
/// main agent's runs never read.
pub(crate) fn unread_notes(notes: &[(u64, String)], workflow: &Workflow) -> Vec<String> {
    let runs: Vec<_> = workflow
        .context()
        .agent_results
        .read()
        .iter()
        .filter(|run| run.agent_name == MAIN)
        .map(|run| run.agent_run_id)
        .collect();
    let given = runs
        .into_iter()
        .map(|run| workflow.notes_given(run))
        .max()
        .unwrap_or(0);
    notes
        .iter()
        .filter(|(seq, _)| *seq > given)
        .map(|(_, text)| text.clone())
        .collect()
}

/// Whether the update of `job` (a command's terminal update, a moved
/// call's note) came too late for the agent to read in its last turn. It
/// came while the agent made its last request, and its task keeps it for
/// the next turn. Or the turn's runtime ended before it was sent, and the
/// manager (`jobs`) keeps it for the next runtime, with the owner's other
/// news that should wake it (`has_alerts`).
pub(crate) fn waits_with(
    task: &arko_agents::Task,
    job: &arko_agents::terminal::JobFinished,
    jobs: Option<&arko_agents::terminal::TerminalManager>,
) -> bool {
    let id = job.job_id.to_string();
    task.pending_context_injections
        .iter()
        .any(|content| content.contains(&id))
        || jobs.is_some_and(|jobs| jobs.has_alerts(job.owner))
}

/// A monitor's news that came too late for the agent to read in its last
/// turn. The runtime got it while the agent made its last request, and the
/// agent's task (`task`, of `owner`'s run) keeps it for the next turn. The
/// framework says which notes should wake the agent (`note_wakes`). An
/// alert does, also of a monitor that ended since, and a warning before a
/// monitor ends does while `warnings_wake` (the user wrote lately). The
/// monitors' other news (one ended) goes with the next turn. A warning
/// names its monitor among those `jobs` runs, to say how long it has left.
pub(crate) fn alert_waits(
    task: &arko_agents::Task,
    owner: uuid::Uuid,
    jobs: Option<&arko_agents::terminal::TerminalManager>,
    warnings_wake: bool,
) -> Option<Alert> {
    use arko_agents::terminal::{AlertKind, JobKind, note_wakes};
    let mut warning = None;
    for note in &task.pending_context_injections {
        match note_wakes(note) {
            Some(AlertKind::EndsSoon) => warning = warning.or(Some(note.as_str())),
            Some(_) => {
                return Some(Alert {
                    owner,
                    job: None,
                    ends_soon: false,
                    late: true,
                });
            }
            None => {}
        }
    }
    let warning = warning.filter(|_| warnings_wake)?;
    let job = jobs.and_then(|jobs| {
        jobs.jobs()
            .into_iter()
            .find(|job| {
                job.owner == owner
                    && matches!(job.kind, JobKind::Monitor(_))
                    && warning.contains(&job.job_id.to_string())
            })
            .map(|job| job.job_id)
    });
    Some(Alert {
        owner,
        job,
        ends_soon: true,
        late: true,
    })
}

/// The crew's next wake-up; never, without a crew.
pub(crate) async fn wake(woken: &mut Option<mpsc::UnboundedReceiver<()>>) {
    if let Some(receiver) = woken {
        if receiver.recv().await.is_some() {
            return;
        }
        // The crew is gone.
        *woken = None;
    }
    std::future::pending::<()>().await;
}

/// Questions the main agent asks with `ask_user` (or a question Arko asks
/// for it, such as approving a worker profile), and, when it waits for the
/// answer, where the answer goes.
pub(crate) struct Question {
    /// The run that asks: the main agent's conversation.
    pub(crate) run: uuid::Uuid,
    /// The questions with their ids. A question Arko asks has an id that
    /// isn't the agent's (`q1`), and its answer goes back as the user's
    /// words alone.
    pub(crate) asked: Vec<arko_agents::questions::AskedQuestion>,
    /// Where the answer goes, when the agent waits for it.
    pub(crate) answer: Option<oneshot::Sender<String>>,
}

impl Question {
    /// The questions' text, one per line.
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        self.asked
            .iter()
            .map(|question| question.question.question.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether Arko asks it, not the agent: its answer is the user's words.
    pub(crate) fn plain(&self) -> bool {
        !self
            .asked
            .iter()
            .all(|question| crate::questions::is_agent_id(&question.id))
    }
}

/// Puts the main agent's `ask_user` questions to the user. The answers to
/// those it waits for go back as the answer, and the others reach it later
/// as notes. Without a terminal, the agent is told that no one can answer.
/// It always answers: an error would fail the turn.
pub(crate) struct Asker {
    pub(crate) questions: Option<mpsc::UnboundedSender<Question>>,
    /// The feed, which says which questions are still open.
    pub(crate) feed: Option<crate::remote::Feed>,
}

impl Asker {
    /// Whether anyone can answer a question (there is a terminal, or an
    /// app).
    pub(crate) fn can_ask(&self) -> bool {
        self.questions.is_some()
    }
}

/// The answer when no one can answer.
pub(crate) const CANT_ASK: &str = "No one can answer questions in this session (Arko runs without a terminal, from a script or with -p). Decide yourself, go on, and say in your reply what you assumed.";

#[async_trait::async_trait]
impl UserInputHandler for Asker {
    async fn respond(&self, request: UserInputRequest) -> Result<String, String> {
        let Some(questions) = &self.questions else {
            return Ok(CANT_ASK.to_string());
        };
        let (answer, answered) = oneshot::channel();
        let question = Question {
            run: request.agent_run_id,
            asked: asked_of(&request),
            answer: Some(answer),
        };
        if questions.send(question).is_err() {
            return Ok(CANT_ASK.to_string());
        }
        Ok(answered
            .await
            .unwrap_or_else(|_| "(The user didn't answer.)".to_string()))
    }

    /// The questions go to the user, and their answers reach the agent
    /// later, as notes.
    async fn asked(&self, request: UserInputRequest) -> Result<Option<String>, String> {
        let Some(questions) = &self.questions else {
            return Ok(Some(CANT_ASK.to_string()));
        };
        let question = Question {
            run: request.agent_run_id,
            asked: asked_of(&request),
            answer: None,
        };
        if questions.send(question).is_err() {
            return Ok(Some(CANT_ASK.to_string()));
        }
        Ok(None)
    }

    fn open_questions(&self, agent_run_id: uuid::Uuid) -> Option<Vec<String>> {
        self.feed
            .as_ref()
            .map(|feed| feed.open_question_ids(agent_run_id))
    }
}

/// The questions of `request`, or its text as one question.
fn asked_of(request: &UserInputRequest) -> Vec<arko_agents::questions::AskedQuestion> {
    if !request.asked.is_empty() {
        return request.asked.clone();
    }
    vec![arko_agents::questions::AskedQuestion {
        id: crate::questions::PLAIN_ID.to_string(),
        question: arko_agents::questions::Question {
            question: request.questions.trim().to_string(),
            ..Default::default()
        },
    }]
}

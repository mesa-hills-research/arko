//! What the session waits for between and during turns: the user's input,
//! the main agent's `ask_user` questions, finished jobs and the crew's
//! wake-ups.

use std::time::Duration;

use arko_workflows::prelude::*;
use arko_workflows::user_input::{UserInputHandler, UserInputRequest};
use tokio::sync::{mpsc, oneshot};

use crate::agents::MAIN;
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
    /// The terminal's jobs that finish in the background; `None` with -p,
    /// where they wake no one.
    pub(crate) finished:
        Option<tokio::sync::broadcast::Receiver<arko_agents::terminal::JobFinished>>,
    /// A job that finished during the agent's last turn, too late for it
    /// to read the update (see `waits_with`).
    pub(crate) late: Option<arko_agents::terminal::JobFinished>,
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
    /// The remote page sent something.
    Remote(Action),
}

impl Inputs {
    /// The next input, question, action from the remote page, or (when
    /// `woken`) wake-up or job finished; cancel-safe.
    pub(crate) async fn next_event(&mut self, woken: bool) -> Event {
        if let Some(input) = self.pending.take() {
            return Event::Input(Some(input));
        }
        if woken && let Some(job) = self.late.take() {
            return Event::Finished(job);
        }
        tokio::select! {
            input = self.receiver.recv() => Event::Input(input),
            Some(question) = self.questions.recv() => Event::Question(question),
            Some(action) = self.remote.recv() => Event::Remote(action),
            () = wake(&mut self.woken), if woken => Event::Woken,
            finished = job_finished(&mut self.finished), if woken => Event::Finished(finished),
        }
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

/// The notes of the turn (`notes`, with their sequence numbers) that the
/// main agent's runs never read.
pub(crate) fn unread_notes(notes: &[(u64, String)], workflow: &Workflow) -> Vec<String> {
    let given = workflow
        .result()
        .agent_results
        .iter()
        .filter(|run| run.agent_name == MAIN)
        .map(|run| workflow.notes_given(run.agent_run_id))
        .max()
        .unwrap_or(0);
    notes
        .iter()
        .filter(|(seq, _)| *seq > given)
        .map(|(_, text)| text.clone())
        .collect()
}

/// Whether the terminal update of `job` came while the agent made its last
/// request, too late for it to read: its task keeps it for the next turn.
/// shortcut: misses an update the turn's runtime ended before it was sent
/// (it goes with the next message), upgrade with a TerminalManager query
/// if that shows up.
pub(crate) fn waits_with(
    task: &arko_agents::Task,
    job: &arko_agents::terminal::JobFinished,
) -> bool {
    let id = job.job_id.to_string();
    task.pending_context_injections
        .iter()
        .any(|content| content.starts_with("Terminal update:") && content.contains(&id))
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

/// A question the main agent asks with `ask_user`, and where its answer
/// goes.
pub(crate) struct Question {
    pub(crate) text: String,
    pub(crate) answer: oneshot::Sender<String>,
}

/// Answers the main agent's `ask_user` questions: the user types the
/// answer at the prompt. Without a terminal, the agent is told that no one
/// can answer. It always answers: an error would fail the turn.
pub(crate) struct Asker {
    pub(crate) questions: Option<mpsc::UnboundedSender<Question>>,
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
            text: request.questions,
            answer,
        };
        if questions.send(question).is_err() {
            return Ok(CANT_ASK.to_string());
        }
        Ok(answered
            .await
            .unwrap_or_else(|_| "(The user didn't answer.)".to_string()))
    }
}

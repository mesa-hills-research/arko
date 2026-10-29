//! Approvals: what an agent's tool asks the user to allow before it acts,
//! such as saving a worker profile or a skill for every project, adding an
//! MCP server, or starting a worker on a model billed per token.
//!
//! A tool asks with [`Approvals::request`] and waits for the [`Answer`]. The
//! request waits in the session's approval tray, which every front end shows
//! above its message box ([`Reading::approvals`](crate::remote::Reading::approvals)).
//! Requests that come while others wait join them, and the user answers
//! them all at once ([`Input::AnswerApprovals`](crate::Input::AnswerApprovals)):
//! each is approved or rejected, and the words the message box held go to
//! the agent with each answer. A request whose tool stops waiting (its turn
//! stopped) leaves the tray. Nothing of the tray is kept with the session,
//! since a tool that runs again after a resume asks again.
//!
//! When no one can answer (`-p`, no terminal), [`Approvals::can_answer`] is
//! false, and a tool refuses with words of its own before it asks.

use std::sync::Arc;

use arko_agents::TaskContext;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::remote::Feed;
pub use crate::remote::protocol::{
    Approval, ApprovalAnswers, ApprovalChoice, ApprovalRequest, BillingApproval, Detail, EnvName,
    Look, MAX_APPROVAL_WORDS, ProfileApproval, Scope, ServerApproval, SkillApproval, SkillFile,
    SkillSource, approval_words_refusal, approvals_heading, approve_label, reject_label,
};
use crate::ui::Output;

/// What the user decided about one request, as the tool that asked reads
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Approved. `words` are what the message box held, for the agent, and
    /// `option` says the request's option was on ([`Approval::option`]).
    Approved { words: Option<String>, option: bool },
    /// Rejected, with what the message box held.
    Rejected { words: Option<String> },
    /// No answer came: the turn stopped, or no one can answer in this
    /// session.
    Unanswered,
}

/// Puts the agents' requests to the user and gives each tool its answer.
#[derive(Clone)]
pub struct Approvals {
    feed: Option<Feed>,
    /// Someone can answer: a terminal or an app.
    answerable: bool,
    /// The web page is served, where the user answers too (shared by the
    /// clones).
    page: Arc<std::sync::atomic::AtomicBool>,
    /// Where the line prompt shows a request.
    output: Option<Arc<Output>>,
}

impl std::fmt::Debug for Approvals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Approvals")
            .field("answerable", &self.answerable)
            .finish_non_exhaustive()
    }
}

impl Approvals {
    /// The tray of the session whose feed is `feed`. `answerable` says that
    /// someone can answer (there is a terminal, or an app).
    pub(crate) fn new(feed: Option<Feed>, answerable: bool, output: Option<Arc<Output>>) -> Self {
        Self {
            feed,
            answerable,
            page: Arc::default(),
            output,
        }
    }

    /// Says whether the session's web page is served, where the user can
    /// answer requests also without a terminal or an app.
    pub(crate) fn set_page_served(&self, served: bool) {
        self.page.store(served, std::sync::atomic::Ordering::SeqCst);
    }

    /// Approvals no one can answer.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self::new(None, false, None)
    }

    /// Whether anyone can answer a request: at a terminal, in an app, or on
    /// the web page while it is served. A tool that can't go on without an
    /// answer refuses when no one can, in words the agent can act on.
    pub fn can_answer(&self) -> bool {
        (self.answerable || self.page.load(std::sync::atomic::Ordering::SeqCst))
            && self.feed.is_some()
    }

    /// Asks the user to approve `approval` for the tool call of `context`,
    /// and waits for the answer. The wait ends with [`Answer::Unanswered`]
    /// when no one can answer, and the request leaves the tray when the call
    /// stops waiting (its turn stopped).
    pub async fn request(&self, context: &TaskContext<'_>, approval: Approval) -> Answer {
        self.request_for(context.task_id, context.call_id.clone(), approval)
            .await
    }

    /// Asks the user to approve `approval` for run `run` (its call `call`,
    /// when known), and waits for the answer.
    pub(crate) async fn request_for(
        &self,
        run: Uuid,
        call: Option<String>,
        approval: Approval,
    ) -> Answer {
        match self.ask(run, call, approval) {
            Some(waiter) => waiter.answer().await,
            None => Answer::Unanswered,
        }
    }

    /// Puts `approval` in the tray for run `run`, or `None` when no one can
    /// answer. The request leaves the tray when the [`Waiter`] goes.
    pub(crate) fn ask(
        &self,
        run: Uuid,
        call: Option<String>,
        approval: Approval,
    ) -> Option<Waiter> {
        if !self.can_answer() {
            return None;
        }
        let feed = self.feed.clone()?;
        let shown = approval.clone();
        let (id, answer) = feed.request_approval(run, call, approval);
        if let Some(output) = &self.output {
            let count = feed.approvals().len();
            for line in line_prompt_lines(&shown, count) {
                output.echo(&line);
            }
        }
        Some(Waiter { id, feed, answer })
    }
}

/// A request in the tray, until its answer comes or its asker stops
/// waiting.
pub(crate) struct Waiter {
    id: String,
    feed: Feed,
    answer: oneshot::Receiver<Answer>,
}

impl Waiter {
    /// Waits for the answer.
    pub(crate) async fn answer(mut self) -> Answer {
        (&mut self.answer).await.unwrap_or(Answer::Unanswered)
    }

    /// Where the answer comes, for a wait of the caller's own.
    pub(crate) fn receiver(&mut self) -> &mut oneshot::Receiver<Answer> {
        &mut self.answer
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.feed.withdraw_approval(&self.id);
    }
}

/// The requests waiting in a session's tray, with where each answer goes.
#[derive(Default)]
pub(crate) struct Queue {
    /// The number of the last request.
    last: u64,
    /// The number of the latest batch.
    batch: u64,
    waiting: Vec<Waiting>,
}

struct Waiting {
    request: ApprovalRequest,
    answer: oneshot::Sender<Answer>,
}

impl Queue {
    /// Adds a request for `approval`: its id, and where its answer comes.
    /// It joins the requests waiting, or starts a batch of its own.
    pub(crate) fn add(&mut self, approval: Approval) -> (String, oneshot::Receiver<Answer>) {
        self.drop_lost();
        if self.waiting.is_empty() {
            self.batch += 1;
        }
        self.last += 1;
        let id = format!("a{}", self.last);
        let (answer, answered) = oneshot::channel();
        self.waiting.push(Waiting {
            request: ApprovalRequest {
                id: id.clone(),
                batch: self.batch,
                approval,
            },
            answer,
        });
        (id, answered)
    }

    /// The request `id` leaves, unanswered. Returns whether it waited.
    pub(crate) fn withdraw(&mut self, id: &str) -> bool {
        let before = self.waiting.len();
        self.waiting.retain(|waiting| waiting.request.id != id);
        self.waiting.len() != before
    }

    /// The requests waiting, in the order they came.
    pub(crate) fn requests(&self) -> Vec<ApprovalRequest> {
        self.waiting
            .iter()
            .filter(|waiting| !waiting.answer.is_closed())
            .map(|waiting| waiting.request.clone())
            .collect()
    }

    /// Gives the requests `answers` names their answers. Returns those
    /// answered, which leave. Those it doesn't name stay.
    pub(crate) fn answer(&mut self, answers: &ApprovalAnswers) -> Vec<ApprovalRequest> {
        self.drop_lost();
        let words = answers
            .words
            .as_deref()
            .map(str::trim)
            .filter(|words| !words.is_empty())
            .map(str::to_string);
        let mut answered = Vec::new();
        let mut kept = Vec::new();
        for waiting in std::mem::take(&mut self.waiting) {
            let Some(choice) = answers
                .choices
                .iter()
                .find(|choice| choice.id == waiting.request.id)
            else {
                kept.push(waiting);
                continue;
            };
            let answer = if choice.approve {
                Answer::Approved {
                    words: words.clone(),
                    option: choice.option && waiting.request.approval.option().is_some(),
                }
            } else {
                Answer::Rejected {
                    words: words.clone(),
                }
            };
            if waiting.answer.send(answer).is_ok() {
                answered.push(waiting.request);
            }
        }
        self.waiting = kept;
        answered
    }

    /// The requests whose asker stopped waiting leave.
    fn drop_lost(&mut self) {
        self.waiting.retain(|waiting| !waiting.answer.is_closed());
    }
}

/// What the line prompt shows of a request that came, the `count`th in the
/// tray: its question, its details and how to answer.
fn line_prompt_lines(approval: &Approval, count: usize) -> Vec<String> {
    let mut lines = vec!["  ? Arko asks for your approval".to_string()];
    let title = approval.title();
    lines.push(if count > 1 {
        format!("    {count}. {title}")
    } else {
        format!("    {title}")
    });
    for detail in approval.details() {
        let indent = if detail.look == Look::Box {
            "      "
        } else {
            "    "
        };
        for line in detail.text.lines() {
            lines.push(format!("{indent}{line}"));
        }
    }
    if let Some(option) = approval.option() {
        lines.push(format!(
            "    /approve {count}+ approves it with “{option}”."
        ));
    }
    lines.push(if count > 1 {
        "  (Type /approve to approve them all, /approve 1 3 to approve those and reject the rest, or /reject to reject them all. Words you add after go to the agent.)".to_string()
    } else {
        "  (Type /approve or /reject. Words you add after go to the agent.)".to_string()
    });
    lines
}

/// The answer that `/approve` or `/reject` (`approve`) with `argument` gives
/// the `requests` waiting: `/approve` approves all of them, `/approve 1 3`
/// those and rejects the rest, and a number followed by `+` turns its option
/// on. The words after the numbers go with it. `Err` says what is wrong.
pub(crate) fn command_answers(
    requests: &[ApprovalRequest],
    approve: bool,
    argument: &str,
) -> Result<ApprovalAnswers, String> {
    if requests.is_empty() {
        return Err("No approvals wait.".to_string());
    }
    let mut words = argument.trim();
    let mut picked: Vec<(usize, bool)> = Vec::new();
    if approve {
        while let Some(word) = words.split_whitespace().next() {
            let (digits, option) = match word.strip_suffix('+') {
                Some(digits) => (digits, true),
                None => (word, false),
            };
            let Ok(number) = digits.parse::<usize>() else {
                break;
            };
            if number == 0 || number > requests.len() {
                return Err(format!(
                    "There's no approval {number}. The tray holds {}.",
                    requests.len()
                ));
            }
            picked.push((number - 1, option));
            words = words[word.len()..].trim_start();
        }
    }
    let all = approve && picked.is_empty();
    Ok(ApprovalAnswers::of(
        requests,
        |id| all || picked.iter().any(|(at, _)| requests[*at].id == id),
        |id| {
            picked
                .iter()
                .any(|(at, option)| *option && requests[*at].id == id)
        },
        words,
    ))
}

#[cfg(test)]
mod tests;

//! The main agent's questions in a session: put to the user, answered in
//! an answer panel or in a message, dismissed, and the notes that tell the
//! agent. See [`crate::questions`].
//!
//! What the user says about the agent's questions reaches the agent one
//! way only, as one note for its conversation's run. Each answer, dismissal
//! or message that answers questions changes the questions' states and
//! keeps that note in the session's `questions.json`, with an id of its
//! own, in one step. The note is then posted as a workflow note for that
//! run alone (`Note::for_run`), whose number the session records, and it
//! leaves once the workflow counts the run as given it (`notes_given`),
//! which the workflow saves itself. A wait for the answer gets only
//! [`WAIT_ANSWERED`], and the note follows. An idle conversation starts a
//! turn for it, a working one takes it at its next step, and a stopped one
//! when it goes on. Arko posts any kept note not posted yet, or lost with a
//! crash, as the session opens.

use arko_agents::questions::{self as notes, Answers, AskedQuestion, QuestionAnswer};
use uuid::Uuid;

use crate::harness::{Conversation, Harness, News, Prompt, TurnEnd};
use crate::inputs::{Answering, Inputs, Question};
use crate::questions::{
    AgentQuestion, ApprovalAnswer, Pending, QuestionState, WAIT_ANSWERED, is_agent_id,
};
use crate::remote::Level;

/// A wait for the user's answer: the agent's `ask_user` with `wait`, or an
/// approval Arko asked for it.
pub(crate) struct Waiting {
    /// The conversation that waits.
    pub(crate) run: Uuid,
    pub(crate) ids: Vec<String>,
    pub(crate) answer: Answering,
}

impl Waiting {
    /// Arko asked it, not the agent (an approval, or a question without
    /// ids): its answer is the user's decision or words alone, not a note.
    fn plain(&self) -> bool {
        matches!(self.answer, Answering::Approval(_)) || !self.ids.iter().any(|id| is_agent_id(id))
    }
}

/// A message of the user's as the agent reads it, and whether it answers
/// questions (it then goes to the agent as their kept note, not as the
/// message itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    pub(crate) text: String,
    pub(crate) kept: bool,
}

/// Those of `ids` the agent's `ask_user` gave (not Arko's own questions).
fn agent_ids(ids: &[String]) -> Vec<String> {
    ids.iter().filter(|id| is_agent_id(id)).cloned().collect()
}

fn ids_of(questions: &[AgentQuestion]) -> Vec<String> {
    questions
        .iter()
        .map(|question| question.id.clone())
        .collect()
}

/// The questions as the user reads them, each numbered in its ask, with its
/// choices and what the agent assumes without an answer. `dim` mutes a
/// hint.
fn shown(questions: &[AgentQuestion], dim: impl Fn(&str) -> String) -> String {
    let mut lines = Vec::new();
    for question in questions {
        if question.multiple && !question.choices.is_empty() && question.approval.is_none() {
            lines.push(format!(
                "{}  {}",
                question.numbered(),
                dim(crate::questions::CHOOSE_ANY)
            ));
        } else {
            lines.push(question.numbered());
        }
        if !question.choices.is_empty() {
            lines.push(format!("   Choices: {}", question.choices.join(" | ")));
        }
        if let Some(default) = &question.default {
            lines.push(format!("   Default: {default}"));
        }
    }
    lines.join("\n")
}

impl Harness {
    /// The agent asks: the questions go in the transcript and the answer
    /// panel, and a wait for the answer is kept. A wait for questions the
    /// user answered already (as the ask was on its way, or before a
    /// resumed task took its wait up again) gets [`WAIT_ANSWERED`] at once,
    /// and their note follows.
    pub(crate) fn question_asked(&mut self, question: Question) {
        let plain = question.plain();
        let mut asked = question.asked;
        if plain {
            for question in &mut asked {
                question.id = crate::questions::plain_id();
            }
        }
        let waits = question.answer.is_some();
        let run = question.run;
        let item = self.feed.ask(
            run,
            &asked,
            waits,
            question.approval.clone(),
            question.call.clone(),
        );
        let out = &self.output;
        out.echo(&out.bold("  ? Arko asks"));
        let questions: Vec<AgentQuestion> = self
            .feed
            .card_questions(item)
            .into_iter()
            .filter(|known| asked.iter().any(|asked| asked.id == known.id))
            .collect();
        for line in shown(&questions, |text| out.dim(text)).trim().lines() {
            out.echo(&format!("    {line}"));
        }
        out.echo(&out.dim(if question.approval.is_some() {
            "  (Type save to save it, or what to change.)"
        } else if waits {
            "  (Type your answer and press Enter.)"
        } else {
            "  (Arko goes on meanwhile. Answer when you're ready.)"
        }));
        let Some(answer) = question.answer else {
            return;
        };
        // (A wait whose answer can no longer come ends.)
        self.waiting.retain(|waiting| !waiting.answer.is_closed());
        let ids: Vec<String> = asked.iter().map(|asked| asked.id.clone()).collect();
        if !plain {
            let open = ids_of(&self.feed.open_questions_of(run));
            if ids.iter().any(|id| !open.contains(id))
                && let Answering::Text(answer) = answer
            {
                self.post_kept_notes(run);
                let _ = answer.send(WAIT_ANSWERED.to_string());
                return;
            }
        }
        self.waiting.push(Waiting { run, ids, answer });
    }

    /// Tells the agent's wait for the questions of `pending` that the user
    /// replied: their note follows. False when it doesn't wait for them (or
    /// its wait is gone).
    fn answer_agent_wait(&mut self, pending: &Pending) -> bool {
        let Some(at) = self.waiting.iter().position(|waiting| {
            !waiting.plain()
                && waiting.run == pending.run
                && waiting.ids.iter().any(|id| pending.ids.contains(id))
        }) else {
            return false;
        };
        let waiting = self.waiting.remove(at);
        match waiting.answer {
            Answering::Text(answer) => answer.send(WAIT_ANSWERED.to_string()).is_ok(),
            Answering::Approval(_) => false,
        }
    }

    /// Posts run `run`'s kept notes not posted yet, each as a workflow note
    /// for that run alone under the kept note's own id, and keeps which note
    /// carries each. A note the workflow has already, posted before Arko
    /// could record it, is found by that id instead of posted again.
    pub(crate) fn post_kept_notes(&self, run: Uuid) {
        let Some(workflow) = self.workflow.as_ref() else {
            return;
        };
        for pending in self.feed.queued_notes(run) {
            if pending.posted.is_some() {
                continue;
            }
            let note = arko_workflows::notes::Note::new(pending.note.clone())
                .to([crate::agents::MAIN])
                .for_run(run);
            match workflow.post_note_once(pending.id, note) {
                Ok(posted) => self
                    .feed
                    .note_posted(pending.id, Some((posted.id, posted.seq))),
                Err(error) => self.say(
                    Level::Error,
                    &format!("Couldn't pass your answers on ({error}). Arko tries again after the next turn."),
                ),
            }
        }
    }

    /// Gives the waits Arko asked among `answered` their answers: an
    /// approval what `decided` says of its id, a question without ids the
    /// words `words` gives.
    fn answer_plain_waits(
        &mut self,
        answered: &[String],
        decided: impl Fn(&str) -> ApprovalAnswer,
        words: impl Fn(&str) -> String,
    ) -> bool {
        let mut any = false;
        let mut kept = Vec::new();
        for waiting in std::mem::take(&mut self.waiting) {
            match waiting.ids.iter().find(|id| answered.contains(id)) {
                Some(id) if waiting.plain() => {
                    match waiting.answer {
                        Answering::Approval(answer) => {
                            let _ = answer.send(decided(id));
                        }
                        Answering::Text(answer) => {
                            let _ = answer.send(words(id));
                        }
                    }
                    any = true;
                }
                _ => kept.push(waiting),
            }
        }
        self.waiting = kept;
        any
    }

    /// The user's answers from an answer panel, to the current
    /// conversation's questions. Those that are open now are answered, and
    /// the others dropped. An approval's answer goes to it. Returns the
    /// note kept for the agent (`None` when nothing was open).
    pub(crate) fn answer_questions(&mut self, answers: Answers) -> Option<String> {
        let run = self.feed.questions_conversation();
        // One answer for each question, the first filled in.
        let mut given: Vec<QuestionAnswer> = Vec::new();
        for answer in answers.answers {
            if !answer.answer.is_empty() && !given.iter().any(|known| known.id == answer.id) {
                given.push(answer);
            }
        }
        let ids: Vec<String> = given.iter().map(|given| given.id.clone()).collect();
        let answer_of = |id: &str| {
            given
                .iter()
                .find(|given| given.id == id)
                .map(|given| &given.answer)
        };
        let (settled, pending) = self.feed.settle_noting(
            run,
            &ids,
            |question| QuestionState::Answered {
                answer: answer_of(&question.id)
                    .map(|answer| answer.words(None))
                    .unwrap_or_default(),
            },
            |settled, open| {
                let agent: Vec<QuestionAnswer> = given
                    .iter()
                    .filter(|given| {
                        is_agent_id(&given.id) && settled.iter().any(|q| q.id == given.id)
                    })
                    .cloned()
                    .collect();
                if agent.is_empty() {
                    return None;
                }
                let asked: Vec<AskedQuestion> = settled.iter().map(AgentQuestion::asked).collect();
                let still = agent_ids(&ids_of(open));
                let ids = agent.iter().map(|given| given.id.clone()).collect();
                let answers = Answers { answers: agent };
                Some((ids, notes::answer_note(&asked, &answers, &still)))
            },
        );
        if settled.is_empty() && pending.is_none() {
            self.feed
                .notice(Level::Warn, "Those questions are no longer open.");
            return None;
        }
        self.user_present();
        self.output.echo(&self.output.dim("  ✎ answered"));
        self.answer_plain_waits(
            &ids_of(&settled),
            |id| answer_of(id).map_or(ApprovalAnswer::DontSave, crate::questions::approval_reply),
            |id| {
                answer_of(id)
                    .map(|answer| answer.words(None))
                    .unwrap_or_default()
            },
        );
        let pending = pending?;
        self.post_kept_notes(pending.run);
        self.answer_agent_wait(&pending);
        Some(pending.note)
    }

    /// The user dismisses the current conversation's questions `ids` ("Not
    /// now"). An approval dismissed isn't saved. Returns the note kept for
    /// the agent.
    pub(crate) fn dismiss_questions(&mut self, ids: Vec<String>) -> Option<String> {
        let run = self.feed.questions_conversation();
        let (settled, pending) = self.feed.settle_noting(
            run,
            &ids,
            |_| QuestionState::Dismissed,
            |settled, _| {
                let told = agent_ids(&ids_of(settled));
                (!told.is_empty()).then(|| (told.clone(), notes::dismissed_note(&told)))
            },
        );
        if settled.is_empty() {
            return None;
        }
        self.user_present();
        self.answer_plain_waits(
            &ids_of(&settled),
            |_| ApprovalAnswer::DontSave,
            |_| String::new(),
        );
        let pending = pending?;
        self.post_kept_notes(pending.run);
        self.answer_agent_wait(&pending);
        Some(pending.note)
    }

    /// A message the user sends (to start a turn, or as a note) with the
    /// questions `attached` (the "Replying to" chip), or, without any, while
    /// the agent waits for an answer, which it is. The questions it answers
    /// are no longer open, and the message as the agent reads it ("(This
    /// message answers q3.)" before it) is kept and posted as their note,
    /// which is how it reaches the agent. Returns the message (`kept` when
    /// it went as that note, else with "(Still open: q5.)" after it), or
    /// `None` when it answered a wait.
    pub(crate) fn reply_text(&mut self, text: &str, attached: &[String]) -> Option<Reply> {
        let run = self.feed.questions_conversation();
        // Only a message without questions attached answers the waits.
        let ids: Vec<String> = if attached.is_empty() {
            self.waiting
                .iter()
                .filter(|waiting| Some(waiting.run) == run)
                .flat_map(|waiting| waiting.ids.clone())
                .collect()
        } else {
            attached.to_vec()
        };
        let (settled, pending) = if ids.is_empty() {
            (Vec::new(), None)
        } else {
            self.feed.settle_noting(
                run,
                &ids,
                |_| QuestionState::AnsweredInMessage,
                |settled, open| {
                    let agent = agent_ids(&ids_of(settled));
                    let still = agent_ids(&ids_of(open));
                    (!agent.is_empty()).then(|| {
                        let labelled =
                            notes::still_open(&notes::message_answers(&agent, text), &still);
                        (agent, labelled)
                    })
                },
            )
        };
        let answered = ids_of(&settled);
        let approved = self.answer_plain_waits(
            &answered,
            |_| crate::questions::approval_words(text),
            |_| text.trim().to_string(),
        );
        let Some(pending) = pending else {
            if approved {
                return None;
            }
            let still = agent_ids(&ids_of(&self.feed.open_questions()));
            return Some(Reply {
                text: notes::still_open(text, &still),
                kept: false,
            });
        };
        self.post_kept_notes(pending.run);
        if self.answer_agent_wait(&pending) {
            return None;
        }
        Some(Reply {
            text: pending.note,
            kept: true,
        })
    }

    /// The open questions of the card `item`, which an answer to it from
    /// the old web page (`/api/answer`) answers, or `None` (the user is
    /// told) when it has none open any more.
    pub(crate) fn card_answered(&self, item: u64) -> Option<Vec<String>> {
        let open: Vec<String> = self
            .feed
            .card_questions(item)
            .into_iter()
            .filter(|question| question.state.is_open())
            .map(|question| question.id)
            .collect();
        if open.is_empty() {
            self.feed
                .notice(Level::Warn, "That question is no longer open.");
            return None;
        }
        Some(open)
    }

    /// After a turn: the waits that can no longer be answered end. A
    /// question Arko asked for one (an approval) is closed. The agent's questions stay
    /// open, and a resumed task asks again.
    pub(crate) fn end_lost_wait(&mut self) {
        let (lost, kept): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|waiting| waiting.answer.is_closed());
        self.waiting = kept;
        for waiting in lost {
            if matches!(waiting.answer, Answering::Approval(_)) || waiting.plain() {
                self.feed
                    .settle_questions(Some(waiting.run), &waiting.ids, |_| QuestionState::Closed {
                        why: None,
                    });
            } else {
                self.feed.questions_unwaited(&waiting.ids);
            }
        }
    }

    /// A note kept for the agent while no turn runs: an idle conversation
    /// starts a turn for it, as a worker's report does. A stopped one gets it
    /// when it goes on.
    pub(crate) async fn deliver_answers(&mut self, input: &mut Inputs) -> News {
        if !matches!(self.conversation, Conversation::Answered(_)) {
            self.say(
                Level::Info,
                "  (your answers reach the agent with your next message)",
            );
            return News::Waiting;
        }
        match self.turn(Prompt::Answers, input).await {
            TurnEnd::Answered => News::Done,
            TurnEnd::Exit => News::Exit,
            TurnEnd::Stopped => News::Stopped,
            TurnEnd::Failed => News::Failed,
        }
    }

    /// Whether run `run` has a kept note posted that it hasn't been given
    /// yet (its last turn ended first): a turn for it delivers it.
    pub(crate) fn notes_waiting(&self, run: Uuid) -> bool {
        let Some(workflow) = self.workflow.as_ref() else {
            return false;
        };
        let given = workflow.notes_given(run);
        self.feed
            .queued_notes(run)
            .iter()
            .any(|pending| pending.posted.is_some_and(|(_, seq)| seq > given))
    }

    /// The kept notes that reached their runs leave: those the workflow
    /// counts as given (by number, in its own saved state). Those not
    /// posted yet are posted, and so are those whose note the workflow no
    /// longer has (Arko stopped before the workflow saved it).
    pub(crate) fn settle_answer_notes(&self) {
        let Some(workflow) = self.workflow.as_ref() else {
            return;
        };
        let kept = self.feed.kept_notes();
        if kept.is_empty() {
            return;
        }
        let notes: Vec<Uuid> = workflow.notes().iter().map(|posted| posted.id).collect();
        let mut reached = Vec::new();
        let mut runs = Vec::new();
        for pending in kept {
            match pending.posted {
                Some((_, seq)) if workflow.notes_given(pending.run) >= seq => {
                    reached.push(pending.id);
                }
                Some((note, _)) if !notes.contains(&note) => {
                    self.feed.note_posted(pending.id, None);
                    runs.push(pending.run);
                }
                Some(_) => {}
                None => runs.push(pending.run),
            }
        }
        if !reached.is_empty() {
            self.feed.ack_notes(&reached);
        }
        runs.dedup();
        for run in runs {
            self.post_kept_notes(run);
        }
    }

    /// The questions of the conversation `run` are the ones shown. Its
    /// task's own record of the questions it asked brings back those the
    /// session lost.
    pub(crate) fn show_questions_of(&self, run: Option<Uuid>) {
        self.feed.questions_run(run);
        let (Some(run), Some(task)) = (run, self.conversation_task()) else {
            return;
        };
        if task.id != run {
            return;
        }
        self.feed.questions_found(run, &task.questions.open);
    }
}

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
//! [`WAIT_ANSWERED`], and the note follows. A wait that comes after the
//! agent read the note (Claude Code waits only once the turn that asked
//! ends) is told the answer reached it already. An idle conversation starts a
//! turn for it, a working one takes it at its next step, and a stopped one
//! when it goes on. Arko posts any kept note not posted yet, or lost with a
//! crash, as the session opens.

use arko_agents::questions::{self as notes, Answers, QuestionAnswer};
use uuid::Uuid;

use crate::harness::{Conversation, Harness, News, Prompt, TurnEnd};
use crate::inputs::{Answering, Inputs, Question};
use crate::questions::{AgentQuestion, Pending, QuestionState, WAIT_ANSWERED, is_agent_id};
use crate::remote::{Delivery, Level, Partial, RefusalKind, Source};
use crate::sending::Reply as SendReply;

/// A wait for the user's answer: the agent's `ask_user` with `wait`, or a
/// question of its runtime's without ids.
pub(crate) struct Waiting {
    /// The conversation that waits.
    pub(crate) run: Uuid,
    pub(crate) ids: Vec<String>,
    pub(crate) answer: Answering,
}

impl Waiting {
    /// Arko asked it, not the agent (a question without ids): its answer is
    /// the user's words alone, not a note.
    fn plain(&self) -> bool {
        !self.ids.iter().any(|id| is_agent_id(id))
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
        if question.multiple && !question.choices.is_empty() {
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
    /// and their note follows. When the agent has read that note already
    /// (a runtime that waits only once the turn that asked ends), the wait
    /// is told so instead, or, with questions still open, waits for those.
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
        let item = self.feed.ask(run, &asked, waits);
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
        out.echo(&out.dim(if waits {
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
            let (open, settled): (Vec<String>, Vec<String>) =
                ids.iter().cloned().partition(|id| open.contains(id));
            if !settled.is_empty()
                && let Answering::Text(answer) = answer
            {
                if self.note_unread(run, &settled) {
                    self.post_kept_notes(run);
                    let _ = answer.send(WAIT_ANSWERED.to_string());
                } else if open.is_empty() {
                    let _ = answer.send(crate::questions::wait_answered_earlier(&settled));
                } else {
                    self.waiting.push(Waiting {
                        run,
                        ids: open,
                        answer: Answering::Text(answer),
                    });
                }
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

    /// Gives the waits Arko asked among `answered` (questions without ids)
    /// the words `words` gives.
    fn answer_plain_waits(&mut self, answered: &[String], words: impl Fn(&str) -> String) -> bool {
        let mut any = false;
        let mut kept = Vec::new();
        for waiting in std::mem::take(&mut self.waiting) {
            match waiting.ids.iter().find(|id| answered.contains(id)) {
                Some(id) if waiting.plain() => {
                    match waiting.answer {
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
    /// the others dropped. Answers too long to send settle nothing, and the
    /// user is told. Returns the note kept for the agent (`None` when
    /// nothing was open). They went once their questions were settled,
    /// which `reply` says.
    pub(crate) fn answer_questions(
        &mut self,
        answers: Answers,
        reply: &mut SendReply,
    ) -> Option<String> {
        let run = self.feed.questions_conversation();
        if let Some(run) = run
            && let Some(refusal) =
                crate::questions::answers_refusal(&self.feed.open_questions_of(run), &answers)
        {
            if !reply.refuse(RefusalKind::TooLong, &refusal) {
                self.say(Level::Warn, &refusal);
            }
            return None;
        }
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
            |settled, open| crate::questions::answers_note(settled, open, &given),
        );
        if settled.is_empty() && pending.is_none() {
            const WORDS: &str = "Those questions are no longer open.";
            if !reply.refuse(RefusalKind::NotOpen, WORDS) {
                self.feed.notice(Level::Warn, WORDS);
            }
            return None;
        }
        reply.accept_partly(Delivery::Answer, None, dropped(&ids, &settled));
        self.user_present();
        self.output.echo(&self.output.dim("  ✎ answered"));
        self.answer_plain_waits(&ids_of(&settled), |id| {
            answer_of(id)
                .map(|answer| answer.words(None))
                .unwrap_or_default()
        });
        let pending = pending?;
        self.post_kept_notes(pending.run);
        self.answer_agent_wait(&pending);
        Some(pending.note)
    }

    /// The user dismisses the current conversation's questions `ids` ("Not
    /// now"). Returns the note kept for the agent. They went once the
    /// questions were settled, which `reply` says.
    pub(crate) fn dismiss_questions(
        &mut self,
        ids: Vec<String>,
        reply: &mut SendReply,
    ) -> Option<String> {
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
            reply.refuse(RefusalKind::NotOpen, "Those questions are no longer open.");
            return None;
        }
        reply.accept_partly(Delivery::Answer, None, dropped(&ids, &settled));
        self.user_present();
        self.answer_plain_waits(&ids_of(&settled), |_| String::new());
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
        let ids = self.replied_ids(attached);
        let (settled, pending) = if ids.is_empty() {
            (Vec::new(), None)
        } else {
            self.feed.settle_noting(
                run,
                &ids,
                |_| QuestionState::AnsweredInMessage,
                |settled, open| crate::questions::message_note(settled, open, text),
            )
        };
        let answered = ids_of(&settled);
        let answered_wait = self.answer_plain_waits(&answered, |_| text.trim().to_string());
        let Some(pending) = pending else {
            if answered_wait {
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

    /// The questions a message of the user's answers: those `attached` to
    /// it, or, without any, those the agent waits for in the current
    /// conversation.
    fn replied_ids(&self, attached: &[String]) -> Vec<String> {
        if !attached.is_empty() {
            return attached.to_vec();
        }
        let run = self.feed.questions_conversation();
        self.waiting
            .iter()
            .filter(|waiting| Some(waiting.run) == run)
            .flat_map(|waiting| waiting.ids.clone())
            .collect()
    }

    /// Why the user's message `text` can't be sent as the answer to the
    /// questions it answers ([`reply_text`](Self::reply_text)), or `None`.
    /// Checked before anything is settled, so they stay open.
    pub(crate) fn reply_refused(&self, text: &str, attached: &[String]) -> Option<String> {
        let ids = self.replied_ids(attached);
        let run = self.feed.questions_conversation()?;
        crate::questions::reply_refusal(&self.feed.open_questions_of(run), &ids, text)
    }

    /// The open questions of the card `item`, which an answer to it from
    /// the old web page (`/api/answer`) answers, or `None` when it has none
    /// open any more.
    pub(crate) fn card_answered(&self, item: u64) -> Option<Vec<String>> {
        let open: Vec<String> = self
            .feed
            .card_questions(item)
            .into_iter()
            .filter(|question| question.state.is_open())
            .map(|question| question.id)
            .collect();
        (!open.is_empty()).then_some(open)
    }

    /// After a turn: the waits that can no longer be answered end. A
    /// question Arko asked (without ids) is closed. The agent's questions
    /// stay open, and a resumed task asks again.
    pub(crate) fn end_lost_wait(&mut self) {
        let (lost, kept): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|waiting| waiting.answer.is_closed());
        self.waiting = kept;
        for waiting in lost {
            if waiting.plain() {
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

    /// Whether a kept note about any of the questions `ids` hasn't reached
    /// run `run` yet, posted or not. A note kept before notes named their
    /// questions counts for every question.
    fn note_unread(&self, run: Uuid, ids: &[String]) -> bool {
        let given = self
            .workflow
            .as_ref()
            .map_or(0, |workflow| workflow.notes_given(run));
        self.feed.queued_notes(run).iter().any(|pending| {
            (pending.ids.is_empty() || pending.ids.iter().any(|id| ids.contains(id)))
                && pending.posted.is_none_or(|(_, seq)| seq > given)
        })
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

    /// The user's answer to the approval tray's requests (see
    /// [`crate::approvals`]). Words too long to send settle nothing, and the
    /// user is told. Words typed in the message box show as the user's
    /// (`source` says where), unless a command gave them.
    pub(crate) fn answer_approvals(
        &mut self,
        answers: crate::approvals::ApprovalAnswers,
        source: Option<Source>,
        reply: &mut SendReply,
    ) {
        if let Some(refusal) = answers
            .words
            .as_deref()
            .and_then(crate::approvals::approval_words_refusal)
        {
            if !reply.refuse(RefusalKind::TooLong, &refusal) {
                self.say(Level::Warn, &refusal);
            }
            return;
        }
        let answered = self.feed.answer_approvals(&answers);
        if answered.is_empty() {
            const WORDS: &str = "Those approvals are no longer open.";
            if !reply.refuse(RefusalKind::NotOpen, WORDS) {
                self.feed.notice(Level::Warn, WORDS);
            }
            return;
        }
        self.user_present();
        let item = match source {
            Some(source) => answers
                .words
                .as_deref()
                .map(str::trim)
                .filter(|words| !words.is_empty())
                .map(|words| self.feed.user(words, source, Delivery::Answer)),
            None => None,
        };
        let named: Vec<String> = answers
            .choices
            .iter()
            .map(|choice| choice.id.clone())
            .filter(|id| !answered.iter().any(|request| &request.id == id))
            .collect();
        reply.accept_partly(
            Delivery::Answer,
            item,
            (!named.is_empty()).then_some(Partial {
                dropped: named,
                words: None,
            }),
        );
        let approved = answered
            .iter()
            .filter(|request| {
                answers
                    .choices
                    .iter()
                    .any(|choice| choice.id == request.id && choice.approve)
            })
            .count();
        let rejected = answered.len() - approved;
        let decided = match (approved, rejected) {
            (_, 0) => "approved".to_string(),
            (0, _) => "rejected".to_string(),
            (approved, rejected) => format!("approved {approved}, rejected {rejected}"),
        };
        self.output
            .echo(&self.output.dim(&format!("  ✎ {decided}")));
    }

    /// `/approve` or `/reject` (`approve`) with `argument`, which answers the
    /// approval tray's requests at the line prompt (see
    /// [`approvals::command_answers`](crate::approvals::command_answers)).
    pub(crate) fn approve_command(&mut self, approve: bool, argument: &str) {
        match crate::approvals::command_answers(&self.feed.approvals(), approve, argument) {
            Ok(answers) => self.answer_approvals(answers, None, &mut SendReply::default()),
            Err(why) => self.say(Level::Warn, &why),
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

/// What of `ids` the user answered isn't among the questions `settled`, as
/// a verdict's part that didn't go.
fn dropped(ids: &[String], settled: &[crate::questions::AgentQuestion]) -> Option<Partial> {
    let dropped: Vec<String> = ids
        .iter()
        .filter(|id| !settled.iter().any(|question| &question.id == *id))
        .cloned()
        .collect();
    (!dropped.is_empty()).then_some(Partial {
        dropped,
        words: None,
    })
}

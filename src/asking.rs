//! The main agent's questions in a session: put to the user, answered in
//! an answer panel or in a message, dismissed, and the notes that tell the
//! agent. See [`crate::questions`].
//!
//! Every answer and dismissal changes the questions' states and keeps the
//! note that tells the agent in one step, in the session's
//! `questions.json`, until the agent has it: read as a note during a turn,
//! given as the answer it waited for, or sent with a turn's message. So an
//! answer given while the conversation is stopped, or as Arko quits,
//! reaches the agent with its next turn.

use arko_agents::questions::{self as notes, Answers, AskedQuestion, QuestionAnswer};
use uuid::Uuid;

use crate::harness::{Conversation, Harness, News, Prompt, TurnEnd};
use crate::inputs::{Answering, Inputs, Question};
use crate::questions::{AgentQuestion, ApprovalAnswer, Pending, QuestionState, is_agent_id};
use crate::remote::{Delivery, Level, Source};

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

/// A note about the agent's questions for it to read (answers, a
/// dismissal), kept as `seq` until it has.
pub(crate) struct AnswerNote {
    pub(crate) seq: u64,
    pub(crate) note: String,
}

/// A message of the user's as the agent reads it, and the number it is
/// kept under until the agent has it, when it answers questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    pub(crate) text: String,
    pub(crate) kept: Option<u64>,
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
/// choices and what the agent assumes without an answer.
fn shown(questions: &[AgentQuestion]) -> String {
    let mut lines = Vec::new();
    for question in questions {
        lines.push(question.numbered());
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
    /// panel, and a wait for the answer is kept. A wait a resumed task
    /// takes up again for questions the user answered meanwhile gets that
    /// answer, or the questions are open again.
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
        for line in shown(&questions).trim().lines() {
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
            let answered: Vec<String> = ids
                .iter()
                .filter(|id| !open.contains(id))
                .cloned()
                .collect();
            if !answered.is_empty() {
                let given = self
                    .feed
                    .queued_notes(run)
                    .into_iter()
                    .find(|pending| pending.ids.iter().any(|id| answered.contains(id)));
                match given {
                    // (It stays kept until the task holds it.)
                    Some(pending) => {
                        if let Answering::Text(answer) = answer {
                            let _ = answer.send(pending.note);
                        }
                        return;
                    }
                    None => self.feed.reopen_questions(run, &answered),
                }
            }
        }
        self.waiting.push(Waiting { run, ids, answer });
    }

    /// Gives the agent's wait for the questions of `pending` its note (kept
    /// until the task holds it). False when it doesn't wait for them (or
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
            Answering::Text(answer) => answer.send(pending.note.clone()).is_ok(),
            Answering::Approval(_) => false,
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
    /// the others dropped. An approval's answer goes to it. What the user
    /// wrote besides ("Anything else") goes with them only when one of the
    /// questions the submission names (answered or not) is the current
    /// conversation's, so it can't reach another conversation. Returns the
    /// note that tells the agent, unless the agent waited for them and has
    /// it already (or nothing was open).
    pub(crate) fn answer_questions(
        &mut self,
        answers: Answers,
        source: Source,
    ) -> Option<AnswerNote> {
        let run = self.feed.questions_conversation();
        let named: Vec<String> = answers
            .answers
            .iter()
            .map(|given| given.id.clone())
            .collect();
        // One answer for each question, the first filled in.
        let mut given: Vec<QuestionAnswer> = Vec::new();
        for answer in answers.answers {
            if !answer.answer.is_empty() && !given.iter().any(|known| known.id == answer.id) {
                given.push(answer);
            }
        }
        let anything_else = answers
            .anything_else
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
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
                let about_open =
                    !settled.is_empty() || open.iter().any(|question| named.contains(&question.id));
                if agent.is_empty() && !(anything_else.is_some() && about_open) {
                    return None;
                }
                let asked: Vec<AskedQuestion> = settled.iter().map(AgentQuestion::asked).collect();
                let still = agent_ids(&ids_of(open));
                let ids = agent.iter().map(|given| given.id.clone()).collect();
                let answers = Answers {
                    answers: agent,
                    anything_else: anything_else.clone(),
                };
                Some((ids, notes::answer_note(&asked, &answers, &still)))
            },
        );
        if settled.is_empty() && pending.is_none() {
            self.feed
                .notice(Level::Warn, "Those questions are no longer open.");
            return None;
        }
        self.user_present();
        if let Some(text) = &anything_else
            && pending.is_some()
        {
            self.feed.user(text, source, Delivery::Answer);
        }
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
        if self.answer_agent_wait(&pending) {
            return None;
        }
        Some(AnswerNote {
            seq: pending.seq,
            note: pending.note,
        })
    }

    /// The user dismisses the current conversation's questions `ids` ("Not
    /// now"). An approval dismissed isn't saved. Returns the note that tells
    /// the agent, unless it waited for them and has it already.
    pub(crate) fn dismiss_questions(&mut self, ids: Vec<String>) -> Option<AnswerNote> {
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
        if self.answer_agent_wait(&pending) {
            return None;
        }
        Some(AnswerNote {
            seq: pending.seq,
            note: pending.note,
        })
    }

    /// A message the user sends (to start a turn, or as a note) with the
    /// questions `attached` (the "Replying to" chip), or, without any, while
    /// the agent waits for an answer, which it is. The questions it answers
    /// are no longer open, and the message as the agent reads it ("(This
    /// message answers q3.)" before it, "(Still open: q5.)" after it) is
    /// kept with them until the agent's task holds it. Returns that message,
    /// or `None` when it went to a wait.
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
                kept: None,
            });
        };
        if self.answer_agent_wait(&pending) {
            return None;
        }
        Some(Reply {
            text: pending.note,
            kept: Some(pending.seq),
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

    /// A note about the questions while no turn runs: it starts a turn of
    /// an idle conversation, as a worker's report does. Otherwise it goes
    /// with the next message.
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

    /// The notes about the conversation `run`'s questions the agent hasn't
    /// read (but `skip`, the message that goes with them), which go with
    /// its next message. They stay kept until its task holds them (see
    /// [`settle_answer_notes`](Self::settle_answer_notes)).
    pub(crate) fn take_answer_notes(
        &mut self,
        run: Option<Uuid>,
        skip: Option<u64>,
    ) -> Vec<String> {
        let Some(run) = run else {
            return Vec::new();
        };
        self.feed
            .queued_notes(run)
            .into_iter()
            .filter(|pending| Some(pending.seq) != skip)
            .map(|pending| pending.note)
            .collect()
    }

    /// After a turn, the notes about the current conversation's questions
    /// that reached the agent leave: those its saved task holds (a message,
    /// a wait's answer, a note it read), and `read`, the notes posted in the
    /// turn the agent read before its task was saved. Those that didn't
    /// (the turn failed or stopped first) stay for its next turn.
    pub(crate) fn settle_answer_notes(&self, read: &[u64]) {
        let Some(run) = self.conversation_run() else {
            return;
        };
        let task = self.conversation_task();
        let reached: Vec<u64> = self
            .feed
            .queued_notes(run)
            .into_iter()
            .filter(|pending| {
                read.contains(&pending.seq)
                    || task.is_some_and(|task| holds_note(task, &pending.note))
            })
            .map(|pending| pending.seq)
            .collect();
        if !reached.is_empty() {
            self.feed.ack_notes(&reached);
        }
    }

    /// The questions of the conversation `run` are the ones shown. Its
    /// task's own record of the questions it asked brings back those the
    /// session lost, and drops the notes the conversation holds already.
    pub(crate) fn show_questions_of(&self, run: Option<Uuid>) {
        self.feed.questions_run(run);
        let (Some(run), Some(task)) = (run, self.conversation_task()) else {
            return;
        };
        if task.id != run {
            return;
        }
        self.feed.questions_found(run, &task.questions.open);
        let held: Vec<u64> = self
            .feed
            .queued_notes(run)
            .into_iter()
            .filter(|pending| holds_note(task, &pending.note))
            .map(|pending| pending.seq)
            .collect();
        if !held.is_empty() {
            self.feed.ack_notes(&held);
        }
    }
}

/// Whether `task`'s conversation holds `note` in a message of the user's
/// (a context update indents its lines).
pub(crate) fn holds_note(task: &arko_agents::Task, note: &str) -> bool {
    let flat = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let note = flat(note);
    task.conversation.messages.iter().any(|message| {
        message.role == arko_ai::Role::User
            && message
                .content
                .iter()
                .filter_map(arko_ai::Content::as_text)
                .any(|text| flat(text).contains(&note))
    })
}

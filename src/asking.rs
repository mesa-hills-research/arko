//! The main agent's questions in a session: put to the user, answered in
//! an answer panel or in a message, dismissed, and the notes that tell the
//! agent. See [`crate::questions`].

use arko_agents::questions::{self as notes, Answers, AskedQuestion, QuestionAnswer};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::harness::{Conversation, Harness, News, Prompt, TurnEnd};
use crate::inputs::{Inputs, Question};
use crate::questions::{AgentQuestion, QuestionState, is_agent_id};
use crate::remote::{Delivery, Level, Source};

/// Questions the main agent waits for: the answer goes back to it at once.
pub(crate) struct Waiting {
    pub(crate) ids: Vec<String>,
    pub(crate) answer: oneshot::Sender<String>,
    /// Arko asked it for the agent (approving a worker profile): the
    /// answer is the user's words alone.
    pub(crate) plain: bool,
}

/// The ids of `questions`.
fn ids(questions: &[AgentQuestion]) -> Vec<String> {
    questions
        .iter()
        .map(|question| question.id.clone())
        .collect()
}

/// Those of `ids` the agent's `ask_user` gave (not Arko's own questions).
fn agent_ids(ids: &[String]) -> Vec<String> {
    ids.iter().filter(|id| is_agent_id(id)).cloned().collect()
}

impl Harness {
    /// The agent asks: the questions go in the transcript and the answer
    /// panel, and a wait for the answer is kept.
    pub(crate) fn question_asked(&mut self, question: Question) {
        let plain = question.plain();
        let mut asked = question.asked;
        if plain {
            for question in &mut asked {
                question.id = crate::questions::plain_id();
            }
        }
        let waits = question.answer.is_some();
        self.feed.ask(question.run, &asked, waits);
        let out = &self.output;
        out.echo(&out.bold("  ? The agent asks"));
        let text = if plain {
            asked
                .iter()
                .map(|question| question.question.question.clone())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            notes::questions_text(&asked)
        };
        for line in text.trim().lines() {
            out.echo(&format!("    {line}"));
        }
        out.echo(&out.dim(if waits {
            "  (Type your answer and press Enter.)"
        } else {
            "  (The agent goes on meanwhile. Answer when you're ready.)"
        }));
        if let Some(answer) = question.answer {
            // (A wait before it, whose answer can no longer come, ends.)
            self.waiting = Some(Waiting {
                ids: asked.iter().map(|question| question.id.clone()).collect(),
                answer,
                plain,
            });
        }
    }

    /// Whether the agent waits for any of `ids`.
    fn waits_for(&self, ids: &[String]) -> bool {
        self.waiting
            .as_ref()
            .is_some_and(|waiting| waiting.ids.iter().any(|id| ids.contains(id)))
    }

    /// Sends `text` (or `plain` to a question Arko asked) to the agent's
    /// wait. False when its wait is gone (the turn stopped).
    fn answer_wait(&mut self, text: String, plain: String) -> bool {
        let Some(waiting) = self.waiting.take() else {
            return false;
        };
        let sent = if waiting.plain { plain } else { text };
        waiting.answer.send(sent).is_ok()
    }

    /// The user's answers from an answer panel. The questions answered are
    /// no longer open. Returns the note that tells the agent, unless the
    /// agent waited for them and has it already (or nothing was open).
    pub(crate) fn answer_questions(&mut self, answers: Answers, source: Source) -> Option<String> {
        let open = self.feed.open_questions();
        let given: Vec<QuestionAnswer> = answers
            .answers
            .into_iter()
            .filter(|given| {
                !given.answer.is_empty() && open.iter().any(|question| question.id == given.id)
            })
            .collect();
        let anything_else = answers
            .anything_else
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        if given.is_empty() && (anything_else.is_none() || open.is_empty()) {
            self.feed
                .notice(Level::Warn, "Those questions are no longer open.");
            return None;
        }
        self.user_present();
        let answered = given
            .iter()
            .map(|given| given.id.clone())
            .collect::<Vec<_>>();
        let asked: Vec<AskedQuestion> = open.iter().map(AgentQuestion::asked).collect();
        self.feed.settle_questions(None, &answered, |question| {
            let answer = given
                .iter()
                .find(|given| given.id == question.id)
                .map(|given| given.answer.words(Some(&question.asked().question)))
                .unwrap_or_default();
            QuestionState::Answered { answer }
        });
        if let Some(text) = &anything_else {
            self.feed.user(text, source, Delivery::Answer);
        }
        self.output.echo(&self.output.dim("  ✎ answered"));
        let still = agent_ids(&ids(&self.feed.open_questions()));
        let words = given
            .first()
            .map(|given| given.answer.words(None))
            .unwrap_or_default();
        let answers = Answers {
            answers: given
                .into_iter()
                .filter(|given| is_agent_id(&given.id))
                .collect(),
            anything_else,
        };
        let note = notes::answer_note(&asked, &answers, &still);
        if self.waits_for(&answered) && self.answer_wait(note.clone(), words) {
            return None;
        }
        // Only Arko's own question was answered, and its wait is gone.
        (!answers.is_empty()).then_some(note)
    }

    /// The user dismisses the questions `ids` ("Not now"), or every open
    /// one when it is empty. Returns the note that tells the agent, unless
    /// it waited for them and has it already.
    pub(crate) fn dismiss_questions(&mut self, ids: Vec<String>) -> Option<String> {
        let open = ids_of_open(&self.feed.open_questions());
        let ids: Vec<String> = if ids.is_empty() {
            open
        } else {
            ids.into_iter().filter(|id| open.contains(id)).collect()
        };
        if ids.is_empty() {
            return None;
        }
        self.user_present();
        self.feed
            .settle_questions(None, &ids, |_| QuestionState::Dismissed);
        let told = agent_ids(&ids);
        let note = notes::dismissed_note(&told);
        if self.waits_for(&ids) && self.answer_wait(note.clone(), String::new()) {
            return None;
        }
        (!told.is_empty()).then_some(note)
    }

    /// A message the user sends (to start a turn, or as a note) with the
    /// questions `attached` (the "Replying to" chip), or, without them,
    /// while the agent waits for an answer, which it is. The questions it
    /// answers are no longer open. Returns the message as the agent reads
    /// it ("(This message answers q3.)" before it, "(Still open: q5.)"
    /// after it), or `None` when it went to the agent's wait.
    pub(crate) fn reply_text(&mut self, text: &str, attached: &[String]) -> Option<String> {
        let open = ids_of_open(&self.feed.open_questions());
        let mut answered: Vec<String> = attached
            .iter()
            .filter(|id| open.contains(id))
            .cloned()
            .collect();
        if answered.is_empty()
            && let Some(waiting) = &self.waiting
        {
            answered = waiting
                .ids
                .iter()
                .filter(|id| open.contains(id))
                .cloned()
                .collect();
        }
        if !answered.is_empty() {
            self.feed
                .settle_questions(None, &answered, |_| QuestionState::AnsweredInMessage);
        }
        let still = agent_ids(&ids(&self.feed.open_questions()));
        let labelled =
            notes::still_open(&notes::message_answers(&agent_ids(&answered), text), &still);
        if self.waits_for(&answered) && self.answer_wait(labelled.clone(), text.to_string()) {
            return None;
        }
        Some(labelled)
    }

    /// The open questions of the card `item`, which an answer to it from
    /// the old web page (`/api/answer`) answers.
    pub(crate) fn card_open_ids(&self, item: u64) -> Vec<String> {
        ids_of_open(&self.feed.card_questions(item))
    }

    /// After a turn: a wait that can no longer be answered ends. A question
    /// Arko asked for it is closed. The agent's questions stay open, and a
    /// resumed task asks again.
    pub(crate) fn end_lost_wait(&mut self) {
        if !self
            .waiting
            .as_ref()
            .is_some_and(|waiting| waiting.answer.is_closed())
        {
            return;
        }
        let waiting = self.waiting.take().expect("checked");
        if waiting.plain {
            self.feed
                .settle_questions(None, &waiting.ids, |_| QuestionState::Closed { why: None });
        }
    }

    /// A note about the questions while no turn runs: it starts a turn of
    /// an idle conversation, as a worker's report does. Otherwise it goes
    /// with the next message.
    pub(crate) async fn deliver_answers(&mut self, note: String, input: &mut Inputs) -> News {
        self.answers.push(note);
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

    /// The questions of the conversation `run` are the ones shown.
    pub(crate) fn show_questions_of(&self, run: Option<Uuid>) {
        self.feed.questions_run(run);
    }
}

fn ids_of_open(questions: &[AgentQuestion]) -> Vec<String> {
    questions
        .iter()
        .filter(|question| question.state.is_open())
        .map(|question| question.id.clone())
        .collect()
}

//! The main agent's questions to the user (`ask_user`), as front ends show
//! them, and what the user does with them.
//!
//! The agent asks without waiting, unless it passes `wait: true`. Each
//! question has an id in its conversation (`q1`, `q2`, ...). The session
//! keeps them with what became of them ([`QuestionState`]) in its folder, so
//! open questions survive a resume. [`Reading::questions`](crate::remote::Reading::questions)
//! has the open ones of the current conversation, and the ask's feed item
//! ([`Kind::Question`](crate::remote::Kind::Question)) has all of its
//! questions in [`Item::questions`](crate::remote::Item::questions), updated
//! as they change.
//!
//! The user answers in the answer panel ([`Input::AnswerQuestions`](crate::Input::AnswerQuestions)),
//! dismisses with "Not now" ([`Input::DismissQuestions`](crate::Input::DismissQuestions)),
//! or answers in a message they send with the questions attached
//! ([`Message::answers`](crate::Message::answers)). Each reaches the agent as
//! a note, which starts a turn when it is idle, as a worker's report does.
//! The agent settles or withdraws a question itself with `close_questions`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use arko_agents::questions::{Answer, Answers, AskedQuestion, QuestionAnswer};

/// What became of a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum QuestionState {
    /// It waits for the user.
    Open,
    /// The user answered it in the answer panel, and the agent read this
    /// (`"Spaces"`, `"You decide (you said you'd assume: not yet)"`).
    Answered { answer: String },
    /// The user answered it in a message they sent with it attached.
    AnsweredInMessage,
    /// The agent settled or withdrew it (`close_questions`), and why, in its
    /// words.
    Closed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        why: Option<String>,
    },
    /// The user dismissed it ("Not now"), and the agent goes on with its
    /// default.
    Dismissed,
}

impl QuestionState {
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }
}

/// One of the main agent's questions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentQuestion {
    /// Its id in its conversation, such as `q3`.
    pub id: String,
    /// The question, with why it matters.
    pub question: String,
    /// Suggested answers (at most six), shown as chips.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// The user may pick several of the choices.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multiple: bool,
    /// What the agent assumes without an answer, which "You decide" means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(flatten)]
    pub state: QuestionState,
    /// The agent waits for the answer (it asked with `wait`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waits: bool,
    /// The feed item of the ask it came in, its question card (0 when the
    /// feed doesn't have it).
    #[serde(default)]
    pub item: u64,
}

impl AgentQuestion {
    fn of(asked: &AskedQuestion, waits: bool, item: u64) -> Self {
        Self {
            id: asked.id.clone(),
            question: asked.question.question.clone(),
            choices: asked.question.choices.clone(),
            multiple: asked.question.multiple,
            default: asked.question.default.clone(),
            state: QuestionState::Open,
            waits,
            item,
        }
    }

    /// A question known only by its text, which a resumed session's card
    /// numbers.
    pub(crate) fn placeholder(question: &str) -> Self {
        Self {
            id: String::new(),
            question: question.trim().to_string(),
            choices: Vec::new(),
            multiple: false,
            default: None,
            state: QuestionState::Open,
            waits: false,
            item: 0,
        }
    }

    /// The question as the agent asked it.
    pub fn asked(&self) -> AskedQuestion {
        AskedQuestion {
            id: self.id.clone(),
            question: arko_agents::questions::Question {
                question: self.question.clone(),
                choices: self.choices.clone(),
                multiple: self.multiple,
                default: self.default.clone(),
            },
        }
    }
}

/// The id a question Arko asks for the agent (approving a worker profile)
/// is asked under. Arko gives each its own id, `x` and eight hex digits,
/// and its answer goes back as the user's words alone.
pub(crate) const PLAIN_ID: &str = "x";

/// Whether `id` is one the agent's `ask_user` gave (`q3`), not one of
/// Arko's own questions.
pub fn is_agent_id(id: &str) -> bool {
    is_id(id)
}

/// A new id for one of Arko's own questions.
pub(crate) fn plain_id() -> String {
    format!("{PLAIN_ID}{}", &Uuid::new_v4().simple().to_string()[..8])
}

/// The file in a session's folder that keeps its questions.
pub(crate) const FILE: &str = "questions.json";

/// A question with the conversation (the main agent's run) it belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    run: Uuid,
    #[serde(flatten)]
    question: AgentQuestion,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    questions: Vec<Entry>,
}

/// The session's questions, the conversation whose are shown, and where
/// they are kept.
#[derive(Debug, Default)]
pub(crate) struct Book {
    entries: Vec<Entry>,
    /// The current conversation's run.
    run: Option<Uuid>,
    path: Option<PathBuf>,
}

/// How many questions of other conversations a session keeps.
const KEPT: usize = 200;

impl Book {
    /// Keeps the questions in `path` from now on, and reads those it has.
    pub(crate) fn keep_in(&mut self, path: PathBuf) {
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(saved) = serde_json::from_str::<Saved>(&text)
        {
            for entry in saved.questions {
                if !self
                    .entries
                    .iter()
                    .any(|known| known.run == entry.run && known.question.id == entry.question.id)
                {
                    let mut entry = entry;
                    // Feed items are numbered anew in each process.
                    entry.question.item = 0;
                    self.entries.push(entry);
                }
            }
        }
        self.path = Some(path);
    }

    fn save(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        // The open questions stay, and the latest of the others.
        let closed = self
            .entries
            .iter()
            .filter(|entry| !entry.question.state.is_open())
            .count();
        if closed > KEPT {
            let mut drop = closed - KEPT;
            self.entries.retain(|entry| {
                if drop > 0 && !entry.question.state.is_open() {
                    drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
        let saved = Saved {
            questions: self.entries.clone(),
        };
        match serde_json::to_string_pretty(&saved) {
            Ok(text) => {
                if let Err(error) = crate::session::write_whole(path, text.as_bytes()) {
                    log::warn!(
                        "could not save the questions in {}: {error}",
                        path.display()
                    );
                }
            }
            Err(error) => log::warn!("could not save the questions: {error}"),
        }
    }

    pub(crate) fn set_run(&mut self, run: Option<Uuid>) {
        self.run = run;
    }

    pub(crate) fn run(&self) -> Option<Uuid> {
        self.run
    }

    /// Run `run` asked `asked` in the card `item`. A question asked again
    /// (a waiting ask, after a resume) keeps its state and gets the card.
    pub(crate) fn ask(&mut self, run: Uuid, asked: &[AskedQuestion], waits: bool, item: u64) {
        for question in asked {
            match self
                .entries
                .iter_mut()
                .find(|entry| entry.run == run && entry.question.id == question.id)
            {
                Some(entry) => {
                    entry.question.item = item;
                    entry.question.waits = waits;
                }
                None => self.entries.push(Entry {
                    run,
                    question: AgentQuestion::of(question, waits, item),
                }),
            }
        }
        self.save();
    }

    /// The questions of the current conversation, in the order asked.
    pub(crate) fn current(&self) -> Vec<AgentQuestion> {
        self.of_run(self.run)
    }

    fn of_run(&self, run: Option<Uuid>) -> Vec<AgentQuestion> {
        self.entries
            .iter()
            .filter(|entry| Some(entry.run) == run)
            .map(|entry| entry.question.clone())
            .collect()
    }

    /// The current conversation's open questions.
    pub(crate) fn open(&self) -> Vec<AgentQuestion> {
        self.current()
            .into_iter()
            .filter(|question| question.state.is_open())
            .collect()
    }

    /// The ids of run `run`'s open questions.
    pub(crate) fn open_ids_of(&self, run: Uuid) -> Vec<String> {
        self.of_run(Some(run))
            .into_iter()
            .filter(|question| question.state.is_open())
            .map(|question| question.id)
            .collect()
    }

    /// The questions of card `item`.
    pub(crate) fn of_item(&self, item: u64) -> Vec<AgentQuestion> {
        self.entries
            .iter()
            .filter(|entry| entry.question.item == item && item != 0)
            .map(|entry| entry.question.clone())
            .collect()
    }

    /// The questions of the current conversation (of run `run`, when
    /// given) among `ids` that are open now become `state` (or what `to`
    /// gives each). Returns them as they were, with the cards that changed.
    pub(crate) fn settle(
        &mut self,
        run: Option<Uuid>,
        ids: &[String],
        mut to: impl FnMut(&AgentQuestion) -> QuestionState,
    ) -> (Vec<AgentQuestion>, Vec<u64>) {
        let run = run.or(self.run);
        let mut settled = Vec::new();
        let mut items = Vec::new();
        for entry in &mut self.entries {
            if Some(entry.run) == run
                && entry.question.state.is_open()
                && ids.contains(&entry.question.id)
            {
                settled.push(entry.question.clone());
                entry.question.state = to(&entry.question);
                entry.question.waits = false;
                if entry.question.item != 0 && !items.contains(&entry.question.item) {
                    items.push(entry.question.item);
                }
            }
        }
        if !settled.is_empty() {
            self.save();
        }
        (settled, items)
    }

    /// The current conversation's questions `ids` are no longer waited on.
    /// Returns their cards.
    pub(crate) fn unwait(&mut self, ids: &[String]) -> Vec<u64> {
        let run = self.run;
        let mut items = Vec::new();
        for entry in &mut self.entries {
            if Some(entry.run) == run && entry.question.waits && ids.contains(&entry.question.id) {
                entry.question.waits = false;
                if !items.contains(&entry.question.item) {
                    items.push(entry.question.item);
                }
            }
        }
        if !items.is_empty() {
            self.save();
        }
        items
    }

    /// Sets the current conversation's question `id` to `state`, whatever
    /// it was. Returns its card.
    pub(crate) fn set_state(&mut self, id: &str, state: QuestionState) -> Option<u64> {
        let run = self.run;
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| Some(entry.run) == run && entry.question.id == id)?;
        entry.question.state = state;
        let item = entry.question.item;
        self.save();
        Some(item)
    }

    /// Gives the current conversation's questions of a restored card their
    /// new item, by their ids.
    pub(crate) fn restored(&mut self, ids: &[String], item: u64) -> Vec<AgentQuestion> {
        let run = self.run;
        let mut found = Vec::new();
        for entry in &mut self.entries {
            if Some(entry.run) == run && ids.contains(&entry.question.id) {
                entry.question.item = item;
                found.push(entry.question.clone());
            }
        }
        found
    }
}

/// The ids an `ask_user` call's result names: "Asked q3 and q4. ..." when
/// it returned at once, or the questions' lines ("q3 Tabs or spaces?") when
/// the agent waited.
pub(crate) fn ids_in_result(result: &str) -> Vec<String> {
    let result = result.trim();
    if let Some(rest) = result.strip_prefix("Asked ") {
        let first = rest.split('.').next().unwrap_or_default();
        return first
            .split([',', ' '])
            .filter(|word| is_id(word))
            .map(str::to_string)
            .collect();
    }
    result
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|word| is_id(word))
        .map(str::to_string)
        .collect()
}

fn is_id(word: &str) -> bool {
    word.strip_prefix('q')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arko_agents::questions::Question;

    fn asked(id: &str, question: &str) -> AskedQuestion {
        AskedQuestion {
            id: id.into(),
            question: Question {
                question: question.into(),
                ..Question::default()
            },
        }
    }

    #[test]
    fn the_book_keeps_states_by_conversation_and_survives_a_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let run = Uuid::new_v4();
        let mut book = Book::default();
        book.keep_in(path.clone());
        book.set_run(Some(run));
        book.ask(run, &[asked("q1", "A?"), asked("q2", "B?")], false, 7);
        let (settled, items) = book.settle(None, &["q1".into(), "q9".into()], |_| {
            QuestionState::Answered {
                answer: "Yes".into(),
            }
        });
        assert_eq!(settled.len(), 1);
        assert_eq!(items, [7]);
        // Settled once only.
        assert!(
            book.settle(None, &["q1".into()], |_| QuestionState::Dismissed)
                .0
                .is_empty()
        );
        assert_eq!(book.open_ids_of(run), ["q2"]);
        // Another conversation's questions aren't shown.
        book.set_run(Some(Uuid::new_v4()));
        assert!(book.open().is_empty());

        let mut again = Book::default();
        again.keep_in(path);
        again.set_run(Some(run));
        let current = again.current();
        assert_eq!(current.len(), 2);
        assert_eq!(
            current[0].state,
            QuestionState::Answered {
                answer: "Yes".into()
            }
        );
        assert!(current[1].state.is_open());
        assert_eq!(current[1].item, 0);
        assert_eq!(again.restored(&["q2".into()], 40)[0].item, 40);
        assert_eq!(again.of_item(40).len(), 1);
    }

    #[test]
    fn the_ids_of_a_result_are_read() {
        assert_eq!(
            ids_in_result("Asked q3 and q4. Their answers reach you as a note."),
            ["q3", "q4"]
        );
        assert_eq!(
            ids_in_result("Asked q1, q2 and q3. Their"),
            ["q1", "q2", "q3"]
        );
        assert_eq!(
            ids_in_result("q5 Push now?\n   Default: not yet\nq6 Why?"),
            ["q5", "q6"]
        );
        assert!(ids_in_result("Which file?").is_empty());
    }

    #[test]
    fn a_question_reads_as_the_page_gets_it() {
        let mut question = AgentQuestion::of(&asked("q3", "Tabs?"), true, 4);
        question.state = QuestionState::Answered {
            answer: "Spaces".into(),
        };
        assert_eq!(
            serde_json::to_value(&question).unwrap(),
            serde_json::json!({
                "id": "q3", "question": "Tabs?", "state": "answered",
                "answer": "Spaces", "waits": true, "item": 4
            })
        );
    }
}

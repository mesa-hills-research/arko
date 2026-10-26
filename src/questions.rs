//! The main agent's questions to the user (`ask_user`), as front ends show
//! them, and what the user does with them.
//!
//! The agent asks without waiting, unless it passes `wait: true`. Each
//! question has an id that is unique in its session (`q1`, `q2`, ...), for
//! the agent and the API. The user never sees it. A front end numbers the
//! questions of each ask from 1 ([`AgentQuestion::number`]), and a lone
//! question gets no number. The session keeps the questions with what
//! became of them ([`QuestionState`]) in its folder, so open questions
//! survive a resume. [`Reading::questions`](crate::remote::Reading::questions)
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
//!
//! Arko asks too, for the agent, before it saves a worker profile for the
//! project or for every project. That question is an approval
//! ([`AgentQuestion::approval`]), with the choices [`SAVE`] and
//! [`DONT_SAVE`].

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
    /// The user answered it in the answer panel, in these words
    /// (`"Spaces"`, `"You decide"`, `"Save"`).
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
    /// Its id in the session, such as `q3`, for the agent and the API. Front
    /// ends show [`number`](Self::number) instead.
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
    /// The agent waits for the answer (it asked with `wait`, or it waits
    /// for an approval).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waits: bool,
    /// The feed item of the ask it came in, its question card (0 when the
    /// feed doesn't have it).
    #[serde(default)]
    pub item: u64,
    /// Its number among its ask's questions (1, 2, 3), which the user sees,
    /// or `None` when the ask has one question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u32>,
    /// What the user is asked to approve, when Arko asks it for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,
}

/// A worker profile the agent wants to save, which the user approves with
/// [`SAVE`] or not with [`DONT_SAVE`]. Words of their own go back to the
/// agent as changes to make, and it asks again with the changed profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Approval {
    /// The profile's name, such as `3d-judge`.
    pub name: String,
    /// The model it runs on, as the profile names it, or "the session's
    /// worker model".
    pub model: String,
    /// Its reasoning effort, such as `high`, unless the model's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Where it is kept: "this project" or "every project".
    pub scope: String,
    /// What it is for, in one line.
    pub description: String,
    /// Its standing instructions (empty without any).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instructions: String,
}

impl Approval {
    /// What it asks: "Save the worker profile 3d-judge for every project?"
    pub fn title(&self) -> String {
        format!("Save the worker profile {} for {}?", self.name, self.scope)
    }

    /// "Runs on sol at high effort."
    pub fn runs_on(&self) -> String {
        match &self.effort {
            Some(effort) => format!("Runs on {} at {effort} effort.", self.model),
            None => format!("Runs on {}.", self.model),
        }
    }
}

/// The choice that saves an approved profile.
pub const SAVE: &str = "Save";
/// The choice that doesn't save it.
pub const DONT_SAVE: &str = "Don't save";

impl AgentQuestion {
    pub(crate) fn of(asked: &AskedQuestion, waits: bool, item: u64) -> Self {
        Self {
            id: asked.id.clone(),
            question: asked.question.question.clone(),
            choices: asked.question.choices.clone(),
            multiple: asked.question.multiple,
            default: asked.question.default.clone(),
            state: QuestionState::Open,
            waits,
            item,
            number: None,
            approval: None,
        }
    }

    /// A question known only by its text, which a resumed session's card
    /// numbers.
    pub(crate) fn placeholder(question: &str) -> Self {
        Self::of(
            &AskedQuestion {
                id: String::new(),
                question: arko_agents::questions::Question {
                    question: question.trim().to_string(),
                    ..Default::default()
                },
            },
            false,
            0,
        )
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

    /// The question as the user reads it in a line, with its number in its
    /// ask ("2. Which branch?") when it has one. An approval says what it
    /// approves.
    pub fn numbered(&self) -> String {
        let question = match &self.approval {
            Some(approval) => approval.title(),
            None => self.question.trim().to_string(),
        };
        match self.number {
            Some(number) => format!("{number}. {question}"),
            None => question,
        }
    }
}

/// The text a question card shows while its questions are open ("Arko
/// asked 2 questions"), which stays as the heading of their record.
pub fn card_line(questions: &[AgentQuestion]) -> String {
    let count = questions.len().max(1);
    if !questions.is_empty() && questions.iter().all(|question| question.approval.is_some()) {
        return if count == 1 {
            "Arko asked to save a worker profile".to_string()
        } else {
            format!("Arko asked to save {count} worker profiles")
        };
    }
    if count == 1 {
        "Arko asked a question".to_string()
    } else {
        format!("Arko asked {count} questions")
    }
}

/// What the tool that asked for an approval reads of the user's answer:
/// [`SAVE`] to save, nothing not to, or the user's words, which ask for
/// changes. Only [`SAVE`] picked alone (or typed alone) saves.
pub(crate) fn approval_reply(answer: &Answer) -> String {
    match answer.text.as_deref().map(str::trim) {
        Some(text) if !text.is_empty() => approval_words(text),
        _ if !answer.you_decide
            && answer.choices.len() == 1
            && answer.choices[0].trim().eq_ignore_ascii_case(SAVE) =>
        {
            SAVE.to_string()
        }
        _ => String::new(),
    }
}

/// [`approval_reply`] of words the user typed: a choice typed alone picks
/// it, and anything else asks for changes.
pub(crate) fn approval_words(text: &str) -> String {
    let choice = text
        .trim()
        .trim_end_matches(['.', '!'])
        .replace('’', "'")
        .to_lowercase();
    match choice.as_str() {
        "save" => SAVE.to_string(),
        "don't save" | "dont save" => String::new(),
        _ => text.trim().to_string(),
    }
}

/// The id a question Arko asks for the agent (an approval) is asked under.
/// Arko gives each its own id, `x` and eight hex digits.
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

/// A question with the conversation (the main agent's run) it belongs to,
/// and the ask (its card) it came in, which outlasts the process's feed
/// items.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    run: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    card: Option<Uuid>,
    #[serde(flatten)]
    question: AgentQuestion,
}

/// A note about the questions `ids` for run `run` that the agent hasn't
/// surely read: an answer, a dismissal. It goes with the next turn of its
/// conversation, and leaves once the conversation holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pending {
    pub(crate) seq: u64,
    pub(crate) run: Uuid,
    #[serde(default)]
    pub(crate) ids: Vec<String>,
    pub(crate) note: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    questions: Vec<Entry>,
    /// The highest question number the session has given out. (A file
    /// written before it was kept has none.)
    #[serde(default)]
    last: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<Pending>,
}

/// The session's questions, the conversation whose are shown, the notes
/// about them the agent hasn't read, and where they are kept.
#[derive(Debug, Default)]
pub(crate) struct Book {
    entries: Vec<Entry>,
    /// The current conversation's run.
    run: Option<Uuid>,
    path: Option<PathBuf>,
    /// The highest question number seen: a conversation's next questions
    /// are numbered after it.
    last: u32,
    pending: Vec<Pending>,
    /// The agent's closings of questions not asked yet here (its ask is on
    /// its way), by run and id.
    closed_early: Vec<(Uuid, String, QuestionState)>,
    /// The file said how many numbers it gave out.
    counted: bool,
}

/// How many settled questions of other conversations a session keeps.
const KEPT: usize = 500;

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
                    self.last = self.last.max(number_of(&entry.question.id));
                    self.entries.push(entry);
                }
            }
            self.counted = saved.last.is_some();
            self.last = self.last.max(saved.last.unwrap_or_default());
            for pending in saved.pending {
                if !self.pending.iter().any(|known| known.seq == pending.seq) {
                    self.pending.push(pending);
                }
            }
        }
        // An approval open in the file waited in a process that is gone:
        // its tool, run again, asks anew.
        for entry in &mut self.entries {
            if entry.question.approval.is_some() && entry.question.state.is_open() {
                entry.question.state = QuestionState::Closed { why: None };
                entry.question.waits = false;
            }
        }
        self.path = Some(path);
    }

    /// Whether the numbers given out are to be counted from the tasks: the
    /// file is missing, or older than the count it keeps.
    pub(crate) fn needs_count(&self) -> bool {
        !self.counted
    }

    /// The tasks gave out numbers up to `last`.
    pub(crate) fn raise(&mut self, last: u32) {
        self.last = self.last.max(last);
        self.counted = true;
    }

    /// Writes what it keeps to its file.
    pub(crate) fn save(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        // The current conversation's questions stay, and the open ones,
        // and the latest of the others.
        let run = self.run;
        let old = |entry: &Entry| Some(entry.run) != run && !entry.question.state.is_open();
        let settled = self.entries.iter().filter(|entry| old(entry)).count();
        if settled > KEPT {
            let mut drop = settled - KEPT;
            self.entries.retain(|entry| {
                if drop > 0 && old(entry) {
                    drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
        let saved = Saved {
            questions: self.entries.clone(),
            last: Some(self.last),
            pending: self.pending.clone(),
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

    /// The number of the first of `count` questions asked now: the next
    /// ones are numbered after them. (Saved by the caller before it is
    /// used.)
    pub(crate) fn reserve(&mut self, count: u32) -> u32 {
        let first = self.last + 1;
        self.last += count;
        first
    }

    /// Run `run` asked `asked` (`wait`: it waits for the answer), which have
    /// no card yet. Those known already stay as they are.
    pub(crate) fn register(&mut self, run: Uuid, asked: &[AskedQuestion], wait: bool) {
        let new: Vec<AgentQuestion> = asked
            .iter()
            .filter(|asked| !self.knows(run, &asked.id))
            .map(|asked| AgentQuestion::of(asked, wait, 0))
            .collect();
        self.ask(run, new, None);
    }

    /// Run `run` asked `questions` (with their card in `item`, which may
    /// hold some already, and is `card` beyond this process). A question
    /// asked again (a waiting ask, after a resume, or one registered before
    /// its card) keeps its state and gets the card. One the agent closed
    /// before it got here is closed.
    pub(crate) fn ask(&mut self, run: Uuid, questions: Vec<AgentQuestion>, card: Option<Uuid>) {
        for question in questions {
            let id = question.id.clone();
            self.last = self.last.max(number_of(&id));
            match self
                .entries
                .iter_mut()
                .find(|entry| entry.run == run && entry.question.id == id)
            {
                Some(entry) => {
                    entry.question.item = question.item;
                    entry.question.waits = question.waits && entry.question.state.is_open();
                    entry.card = card.or(entry.card);
                }
                None => {
                    let mut question = question;
                    if let Some(at) = self
                        .closed_early
                        .iter()
                        .position(|(early, known, _)| *early == run && *known == id)
                    {
                        question.state = self.closed_early.remove(at).2;
                        question.waits = false;
                    }
                    self.entries.push(Entry {
                        run,
                        card,
                        question,
                    });
                }
            }
        }
    }

    /// The approvals of the current conversation's settled card of a
    /// profile named `name` that has no item yet (the session's first
    /// such), which the restored card `item` shows now.
    pub(crate) fn restore_approval(&mut self, name: &str, item: u64) -> Vec<AgentQuestion> {
        let run = self.run;
        let Some(card) = self
            .entries
            .iter()
            .find(|entry| {
                Some(entry.run) == run
                    && entry.question.item == 0
                    && !entry.question.state.is_open()
                    && entry
                        .question
                        .approval
                        .as_ref()
                        .is_some_and(|approval| approval.name == name)
            })
            .and_then(|entry| entry.card)
        else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in &mut self.entries {
            if entry.card == Some(card) {
                entry.question.item = item;
                found.push(entry.question.clone());
            }
        }
        found
    }

    /// Numbers the questions of card `item` (1, 2, 3), or none when it has
    /// one question.
    pub(crate) fn number_card(&mut self, item: u64) {
        let count = self
            .entries
            .iter()
            .filter(|entry| entry.question.item == item)
            .count();
        let mut next = 0;
        for entry in &mut self.entries {
            if entry.question.item == item {
                next += 1;
                entry.question.number = (count > 1).then_some(next);
            }
        }
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
        self.open_of(self.run)
    }

    /// Run `run`'s open questions.
    pub(crate) fn open_of(&self, run: Option<Uuid>) -> Vec<AgentQuestion> {
        self.of_run(run)
            .into_iter()
            .filter(|question| question.state.is_open())
            .collect()
    }

    /// Whether run `run` asked a question with this id.
    pub(crate) fn knows(&self, run: Uuid, id: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.run == run && entry.question.id == id)
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
        (settled, items)
    }

    /// Run `run`'s agent closed the questions `ids`, why it says: those open
    /// are closed, and those not here yet are once they come. Returns the
    /// cards that changed.
    pub(crate) fn close(&mut self, run: Uuid, ids: &[String], why: Option<String>) -> Vec<u64> {
        let state = QuestionState::Closed { why };
        let (_, items) = self.settle(Some(run), ids, |_| state.clone());
        for id in ids {
            if !self.knows(run, id) {
                self.closed_early.push((run, id.clone(), state.clone()));
            }
        }
        items
    }

    /// Run `run`'s questions `ids` are open again (their answer can't be
    /// told to the agent). Returns their cards.
    pub(crate) fn reopen(&mut self, run: Uuid, ids: &[String]) -> Vec<u64> {
        let mut items = Vec::new();
        for entry in &mut self.entries {
            if entry.run == run
                && ids.contains(&entry.question.id)
                && !entry.question.state.is_open()
            {
                entry.question.state = QuestionState::Open;
                if !items.contains(&entry.question.item) {
                    items.push(entry.question.item);
                }
            }
        }
        items
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
        Some(entry.question.item)
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

    // ----- the notes the agent hasn't read -----

    /// Keeps `note`, about run `run`'s questions `ids`, until the agent has
    /// it. Returns its number.
    pub(crate) fn queue(&mut self, run: Uuid, ids: Vec<String>, note: String) -> u64 {
        let seq = self
            .pending
            .iter()
            .map(|pending| pending.seq)
            .max()
            .unwrap_or(0)
            + 1;
        self.pending.push(Pending {
            seq,
            run,
            ids,
            note,
        });
        seq
    }

    /// Run `run`'s notes the agent hasn't read, oldest first.
    pub(crate) fn queued(&self, run: Uuid) -> Vec<Pending> {
        self.pending
            .iter()
            .filter(|pending| pending.run == run)
            .cloned()
            .collect()
    }

    /// The agent has the notes `seqs`. Returns whether any was kept.
    pub(crate) fn ack(&mut self, seqs: &[u64]) -> bool {
        let before = self.pending.len();
        self.pending.retain(|pending| !seqs.contains(&pending.seq));
        self.pending.len() != before
    }
}

/// The number of an id such as `q3`, or 0.
fn number_of(id: &str) -> u32 {
    id.strip_prefix('q')
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

/// The ids an `ask_user` call's result names: "Asked q3 and q4. ..." when
/// it returned at once (or, in Claude Code, when the agent waits), or the
/// questions' lines ("q3 Tabs or spaces?") when the agent waited.
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

/// Whether an `ask_user` call's result says it returned at once, without
/// waiting for the answer.
pub(crate) fn returned_at_once(result: &str) -> bool {
    let result = result.trim();
    result.starts_with("Asked ") && result.contains("reach") && result.contains("as a note")
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

    fn on_card(id: &str, question: &str, item: u64) -> AgentQuestion {
        AgentQuestion::of(&asked(id, question), false, item)
    }

    #[test]
    fn the_book_keeps_states_by_conversation_and_survives_a_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let run = Uuid::new_v4();
        let mut book = Book::default();
        book.keep_in(path.clone());
        book.set_run(Some(run));
        book.ask(
            run,
            vec![on_card("q1", "A?", 7), on_card("q2", "B?", 7)],
            None,
        );
        book.number_card(7);
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
        let seq = book.queue(run, vec!["q1".into()], "The user answered q1.".into());
        book.save();
        assert_eq!(
            book.open_of(Some(run))
                .iter()
                .map(|q| q.numbered())
                .collect::<Vec<_>>(),
            ["2. B?"]
        );
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
        assert_eq!(again.reserve(1), 3, "numbered after the questions it keeps");
        assert_eq!(again.queued(run)[0].note, "The user answered q1.");
        assert!(again.queued(Uuid::new_v4()).is_empty());
        assert!(again.ack(&[seq]));
        assert!(again.queued(run).is_empty());
        assert_eq!(again.restored(&["q2".into()], 40)[0].item, 40);
        assert_eq!(again.of_item(40).len(), 1);
    }

    /// Numbers given out are saved before they are used, so a restart
    /// before their questions reach the book (and another conversation
    /// asking first) can't give them again. A file older than the count it
    /// keeps, or none, asks for the count to come from the tasks.
    #[test]
    fn numbers_given_out_are_kept_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut book = Book::default();
        book.keep_in(path.clone());
        assert!(book.needs_count(), "no file yet");
        book.raise(0);
        assert_eq!(book.reserve(2), 1);
        book.save();
        let mut again = Book::default();
        again.keep_in(path);
        assert!(!again.needs_count());
        assert_eq!(again.reserve(1), 3);
        let older = dir.path().join("older.json");
        std::fs::write(&older, r#"{"questions": []}"#).unwrap();
        let mut old = Book::default();
        old.keep_in(older);
        assert!(old.needs_count());
        old.raise(7);
        assert_eq!(old.reserve(1), 8);
    }

    /// An approval open in the file waited in a process that is gone: it is
    /// closed as the session opens.
    #[test]
    fn an_open_approval_is_closed_on_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let run = Uuid::new_v4();
        let mut book = Book::default();
        book.keep_in(path.clone());
        let mut approval = on_card(&plain_id(), "Save it?", 4);
        approval.approval = Some(Approval {
            name: "reviewer".into(),
            ..Approval::default()
        });
        book.ask(run, vec![approval, on_card("q1", "A?", 5)], None);
        book.save();
        let mut again = Book::default();
        again.keep_in(path);
        again.set_run(Some(run));
        let states: Vec<_> = again.current().into_iter().map(|q| q.state).collect();
        assert_eq!(
            states,
            [QuestionState::Closed { why: None }, QuestionState::Open]
        );
    }

    /// The agent's closing of a question whose ask hasn't reached the book
    /// yet closes it once it comes.
    #[test]
    fn a_question_closed_before_it_is_asked_comes_closed() {
        let run = Uuid::new_v4();
        let mut book = Book::default();
        book.set_run(Some(run));
        assert!(
            book.close(run, &["q1".into()], Some("Found it".into()))
                .is_empty()
        );
        book.ask(
            run,
            vec![on_card("q1", "A?", 3), on_card("q2", "B?", 3)],
            None,
        );
        assert_eq!(
            book.current()[0].state,
            QuestionState::Closed {
                why: Some("Found it".into())
            }
        );
        assert_eq!(book.open().len(), 1);
    }

    /// The current conversation keeps every state, however many, and the
    /// others keep their latest.
    #[test]
    fn the_current_conversation_keeps_all_its_states() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let (run, other) = (Uuid::new_v4(), Uuid::new_v4());
        let mut book = Book::default();
        book.keep_in(path.clone());
        for (run, first) in [(other, 0), (run, KEPT + 10)] {
            book.set_run(Some(run));
            let ids: Vec<String> = (first..first + KEPT + 10)
                .map(|n| format!("q{}", n + 1))
                .collect();
            book.ask(
                run,
                ids.iter().map(|id| on_card(id, "?", 1)).collect::<Vec<_>>(),
                None,
            );
            book.settle(None, &ids, |_| QuestionState::Dismissed);
            book.save();
        }
        let mut again = Book::default();
        again.keep_in(path);
        again.set_run(Some(run));
        assert_eq!(again.current().len(), KEPT + 10);
        again.set_run(Some(other));
        assert_eq!(again.current().len(), KEPT);
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
        assert!(returned_at_once(
            "Asked q3. Its answer reaches you as a note, so go on with your work."
        ));
        assert!(!returned_at_once(
            "Asked q3. End your turn now without calling tools."
        ));
    }

    #[test]
    fn a_question_reads_as_the_page_gets_it() {
        let mut question = AgentQuestion::of(&asked("q3", "Tabs?"), true, 4);
        question.state = QuestionState::Answered {
            answer: "Spaces".into(),
        };
        question.number = Some(2);
        assert_eq!(
            serde_json::to_value(&question).unwrap(),
            serde_json::json!({
                "id": "q3", "question": "Tabs?", "state": "answered",
                "answer": "Spaces", "waits": true, "item": 4, "number": 2
            })
        );
    }

    /// Only "Save", picked or typed alone, saves an approved profile. Other
    /// words ask for changes.
    #[test]
    fn only_save_approves() {
        let picked = |choices: &[&str], text: Option<&str>, you_decide: bool| {
            approval_reply(&Answer {
                choices: choices.iter().map(|choice| choice.to_string()).collect(),
                text: text.map(str::to_string),
                you_decide,
            })
        };
        assert_eq!(picked(&[SAVE], None, false), SAVE);
        assert_eq!(picked(&[DONT_SAVE], None, false), "");
        assert_eq!(picked(&[SAVE], None, true), "");
        assert_eq!(picked(&[], Some("save."), false), SAVE);
        assert_eq!(
            picked(&[SAVE], Some("yes, but high effort"), false),
            "yes, but high effort"
        );
        assert_eq!(approval_words("Don’t save"), "");
        assert_eq!(approval_words("yes"), "yes");
    }
}

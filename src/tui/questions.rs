//! One question at a time, followed by a summary before sending.
//! The panel opens when the user isn't typing. Alt+Q returns to it,
//! Esc answers in chat, and Ctrl+N dismisses the open questions.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear};

use super::app::{App, Effect};
use super::input::{Editor, History};
use super::markdown;
use super::overlay::{Overlay, hints};
use super::select::Docked;
use super::theme::Theme;
use super::transcript;
use super::wrap::{self, Wrapped};
use crate::glyphs;
use crate::questions::{AgentQuestion, Answer, Answers, CHOOSE_ANY, QuestionAnswer};
use crate::ui::Input;

/// The panel opens by itself only after this long without a key.
pub const QUIET: Duration = Duration::from_millis(1500);

/// What the user filled in for one question.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    /// The choices picked, by their place among the question's choices.
    pub chosen: Vec<usize>,
    pub other: String,
    /// Where the cursor is in `other`, in characters.
    pub cursor: usize,
    pub you_decide: bool,
}

impl Draft {
    fn answer(&self, question: &AgentQuestion) -> Answer {
        Answer {
            choices: self
                .chosen
                .iter()
                .filter_map(|at| question.choices.get(*at).cloned())
                .collect(),
            text: other_on(self).then(|| self.other.trim().to_string()),
            you_decide: self.you_decide,
        }
    }
}

#[cfg(test)]
mod tests;

/// The panel's state, and the questions attached to the message box.
#[derive(Debug, Default)]
pub struct Panel {
    /// The panel shows and has the keys.
    pub open: bool,
    /// The question with the keys, by its id, so it keeps them as other
    /// questions come and go.
    pub focus: Option<String>,
    /// What is filled in, by question.
    pub drafts: HashMap<String, Draft>,
    /// The questions attached to the message box: the next message answers
    /// them.
    pub attached: Vec<String>,
    /// The questions the user has seen (in the panel, or open while they
    /// sent a message).
    pub seen: HashSet<String>,
    /// When the user last pressed a key.
    pub last_key: Option<Instant>,
    /// The questions the bell rang for.
    pub rung: HashSet<String>,
    /// New questions came while the window wasn't focused: the bell rings.
    pub ring: bool,
    /// The window Alt+Q opened the panel over, which comes back when the
    /// panel closes.
    pub behind: Option<Overlay>,
    /// The cursor among the focused question's options.
    pub row: usize,
    /// One at a time: the summary shows, with this row highlighted (the
    /// questions, then Send).
    pub review: Option<usize>,
    /// One at a time: an answer changed from the summary goes back to it.
    pub from_review: bool,
    /// The width the panel was drawn at and its height: it keeps that
    /// height while it shows the same questions.
    pub height: (u16, u16),
    /// How far PgUp and PgDn moved from where the focus is in sight.
    pub offset: isize,
}

/// The chip above the message box, when the panel is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chip {
    /// New questions came while the user typed.
    Asks(usize),
    /// The questions attached to the message box.
    Replying(usize),
    /// Open questions, neither new nor attached.
    Open(usize),
}

impl Chip {
    pub fn text(&self) -> String {
        let questions = |count: usize| {
            if count == 1 {
                "1 question".to_string()
            } else {
                format!("{count} questions")
            }
        };
        match self {
            Chip::Asks(count) => format!("Arko asks {}", questions(*count)),
            Chip::Replying(count) => format!("Replying to {} ×", questions(*count)),
            Chip::Open(1) => "1 open question".to_string(),
            Chip::Open(count) => format!("{count} open questions"),
        }
    }
}

impl App {
    /// The main agent's open questions.
    pub fn open_questions(&self) -> &[AgentQuestion] {
        &self.reading.questions
    }

    /// The place of the question with the panel's keys among the open
    /// ones (the first when it has none).
    pub fn focus_index(&self) -> usize {
        self.panel
            .focus
            .as_ref()
            .and_then(|id| {
                self.open_questions()
                    .iter()
                    .position(|question| &question.id == id)
            })
            .unwrap_or(0)
    }

    /// Whether the agent waits for an answer.
    pub fn agent_waits(&self) -> bool {
        self.reading.questions.iter().any(|question| question.waits)
    }

    /// The chips above the message box, when the panel is closed: the
    /// questions attached, then the others.
    pub fn chips(&self) -> Vec<Chip> {
        let open = self.open_questions();
        if self.panel.open || open.is_empty() {
            return Vec::new();
        }
        let mut chips = Vec::new();
        if !self.panel.attached.is_empty() {
            chips.push(Chip::Replying(self.panel.attached.len()));
        }
        let waiting = self.waiting_questions();
        if !waiting.is_empty() {
            let new = waiting.iter().any(|id| !self.panel.seen.contains(id));
            chips.push(if new {
                Chip::Asks(waiting.len())
            } else {
                Chip::Open(waiting.len())
            });
        }
        chips
    }

    /// The open questions not attached to the message box.
    fn waiting_questions(&self) -> Vec<String> {
        self.open_questions()
            .iter()
            .map(|question| question.id.clone())
            .filter(|id| !self.panel.attached.contains(id))
            .collect()
    }

    /// The open questions changed (a new reading): what was filled in for
    /// questions no longer open goes, and new questions open the panel
    /// when the user isn't typing.
    pub(super) fn questions_changed(&mut self) {
        let open: HashSet<String> = self
            .open_questions()
            .iter()
            .map(|question| question.id.clone())
            .collect();
        self.panel.drafts.retain(|id, _| open.contains(id));
        self.panel.attached.retain(|id| open.contains(id));
        self.panel.seen.retain(|id| open.contains(id));
        self.panel.rung.retain(|id| open.contains(id));
        self.panel.height = (0, 0);
        if let Some(review) = &mut self.panel.review {
            *review = (*review).min(open.len());
        }
        if open.iter().any(|id| !self.panel.rung.contains(id)) {
            self.panel.ring |= !self.focused;
            self.panel.rung.extend(open.iter().cloned());
        }
        if open.is_empty() {
            if self.panel.open {
                self.close_panel();
            }
            return;
        }
        // The question being answered was answered elsewhere, or closed:
        // what is typed next doesn't go to another one.
        if let Some(id) = &self.panel.focus
            && !open.contains(id)
        {
            self.focus_on(0);
            if self.panel.open {
                self.close_panel();
                self.say("That question is no longer open");
            }
        }
        let quiet = self.panel.last_key.is_none_or(|at| at.elapsed() >= QUIET);
        let unseen = self
            .open_questions()
            .iter()
            .position(|question| !self.panel.seen.contains(&question.id));
        if let Some(first) = unseen
            && !self.panel.open
            && quiet
            && self.editor.is_empty()
            && self.overlay.is_none()
            && self.list_focus.is_none()
            && !self.tray.focused
            && self.panel.attached.is_empty()
        {
            self.open_panel_at(first);
        }
    }

    /// Whether the bell rings for new questions (once).
    pub fn take_ring(&mut self) -> bool {
        std::mem::take(&mut self.panel.ring)
    }

    /// Alt+Q: the panel opens (or keeps the keys), on the first question
    /// not filled in. Over a window, the window comes back when the panel
    /// closes.
    pub fn open_panel(&mut self) {
        if self.open_questions().is_empty() {
            self.say("No open questions");
            return;
        }
        if let Some(overlay) = self.overlay.take() {
            self.panel.behind = Some(overlay);
        }
        let first = self
            .open_questions()
            .iter()
            .position(|question| {
                self.panel
                    .drafts
                    .get(&question.id)
                    .is_none_or(|draft| *draft == Draft::default())
            })
            .unwrap_or(0);
        self.open_panel_at(first);
    }

    /// The panel opens on the first open question of the card `item` (its
    /// line in the transcript was clicked). False when it has none open.
    pub(super) fn open_card(&mut self, item: u64) -> bool {
        let Some(at) = self
            .open_questions()
            .iter()
            .position(|question| question.item == item)
        else {
            return false;
        };
        self.open_panel_at(at);
        true
    }

    fn open_panel_at(&mut self, focus: usize) {
        self.panel.open = true;
        self.tray.focused = false;
        self.focus_on(focus);
        // Attached questions are answered here now.
        self.panel.attached.clear();
        let ids: Vec<String> = self
            .open_questions()
            .iter()
            .map(|question| question.id.clone())
            .collect();
        self.panel.seen.extend(ids);
    }

    /// The question at `at` gets the keys, in its chips, or in its Other
    /// field when it has no choices.
    pub(super) fn focus_on(&mut self, at: usize) {
        let question = self.open_questions().get(at).cloned();
        self.panel.offset = 0;
        if let Some(question) = &question {
            self.panel.row = picked_row(self.panel.drafts.get(&question.id), question)
                .unwrap_or_else(|| options(question).len() - 1);
            if let Some(draft) = self.panel.drafts.get_mut(&question.id) {
                draft.cursor = draft.other.chars().count();
            }
        }
        self.panel.focus = question.map(|question| question.id);
    }

    /// The panel closes, and the window it opened over comes back.
    pub(super) fn close_panel(&mut self) {
        self.panel.open = false;
        self.panel.offset = 0;
        self.panel.review = None;
        self.panel.from_review = false;
        self.panel.height = (0, 0);
        if let Some(overlay) = self.panel.behind.take()
            && self.overlay.is_none()
        {
            self.overlay = Some(overlay);
        }
    }

    /// Esc and "Answer in chat": the panel closes and the open questions
    /// are attached to the message box.
    pub(super) fn answer_in_chat(&mut self) {
        self.close_panel();
        self.panel.attached = self
            .open_questions()
            .iter()
            .map(|question| question.id.clone())
            .collect();
    }

    /// Takes the questions off the message box (its chip's ×, Backspace in
    /// an empty box): the next message doesn't answer them.
    pub(super) fn detach(&mut self) -> bool {
        if self.panel.attached.is_empty() {
            return false;
        }
        self.panel.attached.clear();
        true
    }

    /// "Not now": the open questions are dismissed, and the agent goes on
    /// with its defaults. Beside questions attached to the message box,
    /// only the others are.
    pub(super) fn not_now(&mut self) {
        let mut ids = self.waiting_questions();
        if ids.is_empty() {
            ids = std::mem::take(&mut self.panel.attached);
        }
        if ids.is_empty() {
            return;
        }
        self.close_panel();
        self.panel.drafts.retain(|id, _| !ids.contains(id));
        self.effects_push(Effect::Send(super::app::Sending {
            input: Input::DismissQuestions(ids),
            restore: super::app::Restore::Nothing,
        }));
        self.say("Dismissed. Arko goes on with its defaults");
    }

    /// Enter in the panel: sends what's filled in, naming every question
    /// it shows. The questions left unanswered stay open.
    pub(super) fn send_answers(&mut self) {
        let answers: Vec<QuestionAnswer> = self
            .open_questions()
            .iter()
            .map(|question| {
                let answer = self
                    .panel
                    .drafts
                    .get(&question.id)
                    .map(|draft| draft.answer(question))
                    .unwrap_or_default();
                QuestionAnswer::new(question.id.clone(), answer)
            })
            .collect();
        if answers.iter().all(|given| given.answer.is_empty()) {
            self.say("Nothing to send yet. Pick a choice, or type an answer");
            return;
        }
        let answers = Answers { answers };
        // The drafts stay until their questions settle (another front end
        // may have answered first), and the panel opens again when the
        // session refuses them.
        self.close_panel();
        self.effects_push(Effect::Send(super::app::Sending {
            input: Input::AnswerQuestions(answers),
            restore: super::app::Restore::Panel,
        }));
        self.transcript.follow();
    }

    /// Paste into the focused question's field, without folding long text.
    pub(super) fn panel_paste(&mut self, text: &str) {
        let Some((_, question)) = self.focused_question() else {
            return;
        };
        self.panel.row = options(&question).len() - 1;
        field_paste(self.draft_mut(&question.id), text);
    }

    /// The panel's own keys, before the message box gets them.
    pub(super) fn panel_key(&mut self, key: KeyEvent) -> bool {
        if !self.panel.open || self.open_questions().is_empty() {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c' | 'l' | 'o') if ctrl => return false,
            KeyCode::Char('q') if alt => return true,
            KeyCode::Char('n') if ctrl => self.not_now(),
            KeyCode::Esc => self.answer_in_chat(),
            KeyCode::PageDown => self.panel.offset += 5,
            KeyCode::PageUp => self.panel.offset -= 5,
            _ => {
                self.panel.offset = 0;
                self.steps_key(key);
            }
        }
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opt {
    Choice(usize),
    YouDecide,
    Other,
}

/// A question's answers to pick, in order: its choices, "You decide", and
/// Other.
fn options(question: &AgentQuestion) -> Vec<Opt> {
    let mut options: Vec<Opt> = (0..question.choices.len()).map(Opt::Choice).collect();
    options.push(Opt::YouDecide);
    options.push(Opt::Other);
    options
}

fn label(question: &AgentQuestion, opt: Opt) -> String {
    match opt {
        Opt::Choice(at) => question.choices.get(at).cloned().unwrap_or_default(),
        Opt::YouDecide => "You decide".to_string(),
        Opt::Other => PLACEHOLDER.to_string(),
    }
}

/// What "You decide" means: "(Spaces)".
fn default_note(question: &AgentQuestion, opt: Opt) -> Option<String> {
    match (opt, &question.default) {
        (Opt::YouDecide, Some(default)) => Some(format!("({})", default.trim())),
        _ => None,
    }
}

/// What an empty Other field says.
const PLACEHOLDER: &str = "Type your own answer";

// ----- what is picked -----

/// Whether Other has words, which go with the answer beside any choice or
/// "You decide".
fn other_on(draft: &Draft) -> bool {
    !draft.other.trim().is_empty()
}

fn is_on(draft: &Draft, opt: Opt) -> bool {
    match opt {
        Opt::Choice(at) => draft.chosen.contains(&at),
        Opt::YouDecide => draft.you_decide,
        Opt::Other => other_on(draft),
    }
}

/// Picks `opt`. With `toggle`, picking what is picked unpicks it (Space).
/// A question that takes one answer has one choice or "You decide". The
/// words in Other stay with either.
fn pick(draft: &mut Draft, question: &AgentQuestion, opt: Opt, toggle: bool) {
    match opt {
        Opt::Choice(at) => {
            let on = draft.chosen.contains(&at);
            draft.you_decide = false;
            if on {
                if toggle {
                    draft.chosen.retain(|chosen| *chosen != at);
                }
            } else if question.multiple {
                draft.chosen.push(at);
                draft.chosen.sort_unstable();
            } else {
                draft.chosen = vec![at];
            }
        }
        Opt::YouDecide => {
            if draft.you_decide {
                draft.you_decide = !toggle;
            } else {
                draft.you_decide = true;
                draft.chosen.clear();
            }
        }
        // Words in Other are on while there are any.
        Opt::Other => {}
    }
}

/// The answer in a few words, for a summary: "Spaces", "You decide
/// (Spaces)", the words typed, or a pick with them ("Spaces, plus “2
/// wide”").
fn answer_text(draft: &Draft, question: &AgentQuestion) -> Option<String> {
    let picked = if draft.you_decide {
        match &question.default {
            Some(default) => format!("You decide ({})", default.trim()),
            None => "You decide".to_string(),
        }
    } else {
        draft
            .chosen
            .iter()
            .filter_map(|at| question.choices.get(*at).cloned())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let words = draft.other.trim();
    match (picked.is_empty(), words.is_empty()) {
        (true, true) => None,
        (false, true) => Some(picked),
        (true, false) => Some(words.to_string()),
        (false, false) => Some(format!("{picked}, plus “{words}”")),
    }
}

/// The option the cursor starts on: what is picked, else the first (Other
/// for a question without choices, which takes words first).
fn picked_row(draft: Option<&Draft>, question: &AgentQuestion) -> Option<usize> {
    let options = options(question);
    let draft = draft?;
    let picked = if let Some(first) = draft.chosen.first() {
        Opt::Choice(*first)
    } else if draft.you_decide {
        Opt::YouDecide
    } else if other_on(draft) {
        Opt::Other
    } else {
        return None;
    };
    options.iter().position(|opt| *opt == picked)
}

// ----- the Other field -----

/// A key in the Other field of `draft`, at its cursor: readline's keys.
/// True when it took it.
fn field_key(draft: &mut Draft, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut chars: Vec<char> = draft.other.chars().collect();
    let mut at = draft.cursor.min(chars.len());
    let word_start = |chars: &[char], at: usize| {
        let mut i = at;
        while i > 0 && chars[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && chars[i - 1] != ' ' {
            i -= 1;
        }
        i
    };
    let word_end = |chars: &[char], at: usize| {
        let mut i = at;
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        while i < chars.len() && chars[i] != ' ' {
            i += 1;
        }
        i
    };
    match key.code {
        KeyCode::Char('w') if ctrl => {
            let from = word_start(&chars, at);
            chars.drain(from..at);
            at = from;
        }
        KeyCode::Backspace if ctrl || alt => {
            let from = word_start(&chars, at);
            chars.drain(from..at);
            at = from;
        }
        KeyCode::Char('u') if ctrl => {
            chars.drain(..at);
            at = 0;
        }
        KeyCode::Char('k') if ctrl => chars.truncate(at),
        KeyCode::Char('a') if ctrl => at = 0,
        KeyCode::Char('e') if ctrl => at = chars.len(),
        KeyCode::Char('b') if ctrl => at = at.saturating_sub(1),
        KeyCode::Char('f') if ctrl => at = (at + 1).min(chars.len()),
        KeyCode::Char('d') if ctrl => {
            if at < chars.len() {
                chars.remove(at);
            }
        }
        KeyCode::Char('b') if alt => at = word_start(&chars, at),
        KeyCode::Char('f') if alt => at = word_end(&chars, at),
        KeyCode::Char(c) if !ctrl && !alt => {
            chars.insert(at, c);
            at += 1;
        }
        KeyCode::Backspace => {
            if at > 0 {
                at -= 1;
                chars.remove(at);
            }
        }
        KeyCode::Delete => {
            if at < chars.len() {
                chars.remove(at);
            }
        }
        KeyCode::Left if ctrl || alt => at = word_start(&chars, at),
        KeyCode::Right if ctrl || alt => at = word_end(&chars, at),
        KeyCode::Left => at = at.saturating_sub(1),
        KeyCode::Right => at = (at + 1).min(chars.len()),
        KeyCode::Home => at = 0,
        KeyCode::End => at = chars.len(),
        _ => return false,
    }
    draft.other = chars.into_iter().collect();
    draft.cursor = at;
    true
}

/// Text pasted into the Other field of `draft`, at its cursor, on one line.
fn field_paste(draft: &mut Draft, text: &str) {
    let mut chars: Vec<char> = draft.other.chars().collect();
    let at = draft.cursor.min(chars.len());
    let pasted: Vec<char> = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    let count = pasted.len();
    chars.splice(at..at, pasted);
    draft.other = chars.into_iter().collect();
    draft.cursor = at + count;
}

/// The field's text wrapped to `width` as the message box wraps its own,
/// and its cursor's row and column.
fn field_rows(
    text: &str,
    cursor: usize,
    width: usize,
    style: Style,
) -> (Vec<Vec<Span<'static>>>, (usize, usize)) {
    let mut editor = Editor::new(History::load(None));
    for c in text.chars() {
        editor.insert(c);
    }
    editor.set_cursor(cursor);
    let mut rows = editor.lines(width, style, style, style);
    let (row, column) = editor.position(width);
    while rows.len() <= row {
        rows.push(Vec::new());
    }
    (rows, (row, column))
}

// ----- lines -----

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| wrap::width(&span.content)).sum()
}

/// A question's own lines at `width`: its text, and "Choose any" after a
/// question that takes several.
fn question_lines(
    question: &AgentQuestion,
    style: Style,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let mut text = markdown::render(&question.question, style, theme, width);
    if text.is_empty() {
        text.push(Wrapped::line(Vec::new()));
    }
    if question.multiple && !question.choices.is_empty() {
        let hint = Span::styled(CHOOSE_ANY.to_string(), theme.dim());
        let last = text.last_mut().expect("a line at least");
        if spans_width(&last.spans) + 2 + wrap::width(CHOOSE_ANY) <= width {
            last.spans.extend([Span::raw("  "), hint]);
        } else {
            text.push(Wrapped::line(vec![hint]));
        }
    }
    text.into_iter()
        .map(|line| Line::from(line.spans))
        .collect()
}

/// A question's options as a list.
struct Rows {
    lines: Vec<Line<'static>>,
    /// Each option's first and last line.
    spans: Vec<(usize, usize)>,
    /// The text cursor, in Other.
    cursor: Option<(usize, usize)>,
}

/// `question`'s options as a list at `width`, the highlight on `row`
/// while it has the keys.
fn list_rows(
    question: &AgentQuestion,
    draft: &Draft,
    row: Option<usize>,
    theme: &Theme,
    width: usize,
) -> Rows {
    let options = options(question);
    let mut rows = Rows {
        lines: Vec::new(),
        spans: Vec::new(),
        cursor: None,
    };
    let pointer = glyphs::render("▸ ").into_owned();
    let digits = options.len().to_string().len();
    for (at, opt) in options.iter().copied().enumerate() {
        let here = row == Some(at);
        let on = is_on(draft, opt);
        let first = rows.lines.len();
        let mut lead: Vec<Span<'static>> = Vec::new();
        if opt == Opt::Other {
            // The field is a prompt of its own, under the numbers.
            lead.push(Span::raw("  "));
            lead.push(Span::styled(
                format!("{:>digits$} ", glyphs::render("›")),
                if here { theme.accent() } else { theme.dim() },
            ));
        } else {
            lead.push(if here {
                Span::styled(pointer.clone(), theme.accent())
            } else {
                Span::raw("  ")
            });
            let mark = Span::styled(
                format!("{:>digits$} ", at + 1),
                if here { theme.accent() } else { theme.dim() },
            );
            lead.push(mark);
        }
        let lead_width = spans_width(&lead);
        let room = width.saturating_sub(lead_width).max(4);
        let label_style = if here {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.text()
        };
        let text_rows: Vec<Vec<Span<'static>>> = if opt == Opt::Other {
            if here {
                if draft.other.is_empty() {
                    rows.cursor = Some((first, lead_width));
                    vec![vec![Span::styled(PLACEHOLDER.to_string(), theme.dim())]]
                } else {
                    let (field, (r, c)) =
                        field_rows(&draft.other, draft.cursor, room, theme.text());
                    rows.cursor = Some((first + r, lead_width + c));
                    field
                }
            } else if !draft.other.is_empty() {
                let style = if on { theme.text() } else { theme.dim() };
                wrap::wrap(&[Span::styled(draft.other.clone(), style)], room, 0)
            } else {
                vec![vec![Span::styled(PLACEHOLDER.to_string(), theme.dim())]]
            }
        } else {
            let mut text = vec![Span::styled(label(question, opt), label_style)];
            if let Some(note) = default_note(question, opt) {
                text.push(Span::styled(format!(" {note}"), theme.dim()));
            }
            if on {
                text.push(Span::styled(
                    format!(" {}", glyphs::render("✓")),
                    theme.accent(),
                ));
            }
            wrap::wrap(&text, room, 0)
        };
        for (n, text) in text_rows.into_iter().enumerate() {
            let mut spans = if n == 0 {
                lead.clone()
            } else {
                vec![Span::raw(" ".repeat(lead_width))]
            };
            spans.extend(text);
            rows.lines.push(Line::from(spans));
        }
        rows.spans.push((first, rows.lines.len() - 1));
    }
    rows
}

/// The key hints that fit in `width`, from the first.
fn fitting(theme: &Theme, pairs: &[(&str, &str)], width: usize) -> Vec<Span<'static>> {
    let mut count = pairs.len();
    loop {
        let spans = hints(theme, &pairs[..count]);
        if count == 0 || spans_width(&spans) <= width {
            return spans;
        }
        count -= 1;
    }
}

/// What the docked panel shows.
struct Card {
    title: String,
    /// Lines that stay at its top while the rest scrolls.
    header: Vec<Line<'static>>,
    body: Vec<Line<'static>>,
    /// The body's lines to keep in sight.
    focus: (usize, usize),
    /// The text cursor in the body.
    cursor: Option<(usize, usize)>,
    keys: Vec<(&'static str, &'static str)>,
    /// The tallest body any question needs here: the panel keeps that
    /// height whichever has the keys.
    tallest: usize,
}

/// Draws the answer panel above the status line and returns its height.
pub fn draw_panel(frame: &mut Frame, app: &mut App, above: Rect, previous: Option<Docked>) -> u16 {
    if app.open_questions().is_empty() || above.height < 5 {
        return 0;
    }
    let width = (above.width as usize).saturating_sub(5).max(8);
    let card = steps_card(app, width);
    draw_card(frame, app, above, previous, card)
}

fn draw_card(
    frame: &mut Frame,
    app: &mut App,
    above: Rect,
    previous: Option<Docked>,
    card: Card,
) -> u16 {
    let theme = app.theme.clone();
    let room = (above.height as usize * 2 / 3)
        .max(5)
        .min(above.height as usize);
    let wanted = card.header.len() + card.tallest.max(card.body.len()).max(1) + 2;
    let mut height = wanted.min(room);
    // It grows, and never shrinks, while it shows the same questions at
    // the same width.
    let (was_width, was_height) = app.panel.height;
    if was_width == above.width {
        height = height.max(was_height as usize).min(room);
    }
    app.panel.height = (above.width, height as u16);
    let rect = Rect::new(
        above.x,
        above.bottom() - height as u16,
        above.width,
        height as u16,
    );
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.borders())
        .border_style(theme.accent())
        .style(theme.base())
        .title(Line::from(Span::styled(
            card.title.clone(),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let header = card.header.len().min(inner.height as usize);
    let view = (inner.height as usize).saturating_sub(header);
    // The focused lines in sight, then as far as PgUp and PgDn moved.
    let last_skip = card.body.len().saturating_sub(view);
    let (first, end) = card.cursor.map_or_else(
        || {
            card.body
                .iter()
                .position(|line| line.spans.iter().any(|span| span.style == theme.chosen()))
                .map_or(card.focus, |row| (row, row))
        },
        |(row, _)| (row, row),
    );
    let base = (end + 1).saturating_sub(view).min(first).min(last_skip);
    let skip = (base as isize + app.panel.offset).clamp(0, last_skip as isize) as usize;
    app.panel.offset = skip as isize - base as isize;
    let mut shown: Vec<Line<'static>> = card.header[..header].to_vec();
    shown.extend(card.body.iter().skip(skip).take(view).cloned());
    let x = inner.x + 1;
    let width = inner.width.saturating_sub(2);
    for (row, line) in shown.iter().enumerate() {
        frame
            .buffer_mut()
            .set_line(x, inner.y + row as u16, line, width);
    }
    // Its text selects as the transcript's does.
    let area = Rect::new(x, inner.y, width, shown.len() as u16);
    let lines: Vec<Wrapped> = shown
        .iter()
        .map(|line| Wrapped::line(line.spans.clone()))
        .collect();
    let selection = previous
        .filter(|docked| docked.area == area && docked.lines == lines)
        .and_then(|docked| docked.selection);
    let docked = Docked {
        id: 0,
        area,
        lines,
        selection,
    };
    docked.draw_selection(frame.buffer_mut(), &theme);
    app.docked = Some(docked);
    if let Some((row, column)) = card.cursor
        && row >= skip
        && row - skip < view
        && app.overlay.is_none()
    {
        frame.set_cursor_position(Position {
            x: (x + column as u16).min(inner.right().saturating_sub(1)),
            y: inner.y + (header + row - skip) as u16,
        });
    }
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    if skip > 0 {
        pairs.push(("PgUp", "more"));
    }
    if skip + view < card.body.len() {
        pairs.push(("PgDn", "more"));
    }
    pairs.extend(card.keys.iter().copied());
    let mut keys = fitting(&theme, &pairs, (rect.width as usize).saturating_sub(6));
    if !keys.is_empty() {
        keys.push(Span::raw(" "));
        let keys = Line::from(keys);
        let w = keys.width() as u16;
        frame
            .buffer_mut()
            .set_line(rect.right() - w - 2, rect.bottom() - 1, &keys, w);
    }
    height as u16
}

/// The open questions, the focused one's place, and its draft.
fn focused(app: &App) -> (Vec<AgentQuestion>, usize) {
    let questions = app.open_questions().to_vec();
    let focus = app.focus_index().min(questions.len().saturating_sub(1));
    (questions, focus)
}

fn draft_of(app: &App, question: &AgentQuestion) -> Draft {
    app.panel
        .drafts
        .get(&question.id)
        .cloned()
        .unwrap_or_default()
}

/// The focused question with numbered options and a field, or the summary.
fn steps_card(app: &App, width: usize) -> Card {
    let theme = &app.theme;
    let (questions, focus) = focused(app);
    let count = questions.len();
    let page = |question: &AgentQuestion, row: Option<usize>| {
        let mut lines = question_lines(
            question,
            theme.text().add_modifier(Modifier::BOLD),
            theme,
            width,
        );
        lines.push(Line::default());
        let offset = lines.len();
        let rows = list_rows(question, &draft_of(app, question), row, theme, width);
        let focus = row
            .and_then(|row| rows.spans.get(row).copied())
            .map_or((0, 0), |(a, b)| (a + offset, b + offset));
        let cursor = rows.cursor.map(|(r, c)| (r + offset, c));
        lines.extend(rows.lines);
        (lines, focus, cursor)
    };
    let (summary, summary_focus) = review_lines(app, &questions, app.panel.review, width);
    let tallest = questions
        .iter()
        .map(|question| page(question, None).0.len())
        .chain([summary.len()])
        .max()
        .unwrap_or(0);
    if app.panel.review.is_some() {
        let on_send = app.panel.review == Some(count);
        return Card {
            title: " Your answers ".to_string(),
            header: Vec::new(),
            body: summary,
            focus: summary_focus,
            cursor: None,
            keys: vec![
                ("Enter", if on_send { "send" } else { "change" }),
                ("↑↓", "choose"),
                ("Esc", "chat"),
                ("Ctrl+N", "not now"),
            ],
            tallest,
        };
    }
    let question = &questions[focus];
    let options = options(question);
    let row = app.panel.row.min(options.len() - 1);
    let (body, focus_rows, cursor) = page(question, Some(row));
    let draft = draft_of(app, question);
    let picks = options.len() - 1;
    let number_keys = match picks {
        1 => "1",
        2 => "1-2",
        3 => "1-3",
        4 => "1-4",
        5 => "1-5",
        6 => "1-6",
        7 => "1-7",
        8 => "1-8",
        _ => "1-9",
    };
    let mut keys: Vec<(&str, &str)> = Vec::new();
    if options[row] == Opt::Other && !draft.other.is_empty() {
        keys.push(("Enter", "next"));
    } else {
        keys.push((number_keys, "pick"));
        keys.push(("↑↓", "choose"));
        keys.push(("Enter", "next"));
    }
    if focus > 0 {
        keys.push(("Shift+Tab", "back"));
    }
    keys.extend([("Esc", "chat"), ("Ctrl+N", "not now")]);
    Card {
        title: if count > 1 {
            format!(" Question {} of {count} ", focus + 1)
        } else {
            title(&questions)
        },
        header: Vec::new(),
        body,
        focus: focus_rows,
        cursor,
        keys,
        tallest,
    }
}

/// The summary of one at a time: each question with its answer, then
/// Send, the highlight on `selected` (the questions, then Send).
fn review_lines(
    app: &App,
    questions: &[AgentQuestion],
    selected: Option<usize>,
    width: usize,
) -> (Vec<Line<'static>>, (usize, usize)) {
    let theme = &app.theme;
    let pointer = glyphs::render("▸ ").into_owned();
    let digits = questions.len().to_string().len();
    let lead = 2 + digits + 2;
    let column = ((width.saturating_sub(lead)) * 2 / 5).clamp(10, 40);
    let answer_width = width.saturating_sub(lead + column + 2).max(8);
    let mut lines = Vec::new();
    let mut focus = (0, 0);
    for (at, question) in questions.iter().enumerate() {
        let here = selected == Some(at);
        let first = lines.len();
        let text = question
            .question
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let text = transcript::cut_end(&text, column);
        let pad = column.saturating_sub(wrap::width(&text));
        let draft = draft_of(app, question);
        let answer = answer_text(&draft, question);
        let answer_style = if answer.is_some() {
            theme.text()
        } else {
            theme.dim()
        };
        let answer = answer.unwrap_or_else(|| "Not answered".to_string());
        let rows = wrap::wrap(&[Span::styled(answer, answer_style)], answer_width, 0);
        for (n, row) in rows.into_iter().enumerate() {
            let mut spans = if n == 0 {
                vec![
                    if here {
                        Span::styled(pointer.clone(), theme.accent())
                    } else {
                        Span::raw("  ")
                    },
                    Span::styled(format!("{:>digits$}  ", at + 1), theme.dim()),
                    Span::styled(
                        text.clone(),
                        if here {
                            theme.accent().add_modifier(Modifier::BOLD)
                        } else {
                            theme.text()
                        },
                    ),
                    Span::raw(" ".repeat(pad + 2)),
                ]
            } else {
                vec![Span::raw(" ".repeat(lead + column + 2))]
            };
            spans.extend(row);
            lines.push(Line::from(spans));
        }
        if here {
            focus = (first, lines.len() - 1);
        }
    }
    lines.push(Line::default());
    let on_send = selected == Some(questions.len());
    if on_send {
        focus = (lines.len(), lines.len());
    }
    lines.push(Line::from(vec![
        if on_send {
            Span::styled(pointer, theme.accent())
        } else {
            Span::raw("  ")
        },
        Span::styled(
            "Send".to_string(),
            if on_send {
                theme.accent().add_modifier(Modifier::BOLD)
            } else {
                theme.text()
            },
        ),
    ]));
    (lines, focus)
}

impl App {
    /// The focused question and its place.
    fn focused_question(&self) -> Option<(usize, AgentQuestion)> {
        let at = self.focus_index();
        self.open_questions()
            .get(at)
            .cloned()
            .map(|question| (at, question))
    }

    fn draft_mut(&mut self, id: &str) -> &mut Draft {
        self.panel.drafts.entry(id.to_string()).or_default()
    }

    /// Types `key` into the focused question's Other field.
    fn type_key(&mut self, question: &AgentQuestion, key: KeyEvent) {
        field_key(self.draft_mut(&question.id), key);
    }

    /// A character typed away from Other: the cursor moves there, and it
    /// is the start of an answer of one's own.
    fn type_into_other(&mut self, question: &AgentQuestion, key: KeyEvent) {
        self.panel.row = options(question).len() - 1;
        let draft = self.draft_mut(&question.id);
        draft.cursor = draft.other.chars().count();
        self.type_key(question, key);
    }

    fn steps_key(&mut self, key: KeyEvent) {
        if let Some(selected) = self.panel.review {
            return self.review_key(key, selected);
        }
        let Some((at, question)) = self.focused_question() else {
            return;
        };
        let options = options(&question);
        let row = self.panel.row.min(options.len() - 1);
        let on_other = options[row] == Opt::Other;
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let text_empty = self
            .panel
            .drafts
            .get(&question.id)
            .is_none_or(|draft| draft.other.is_empty());
        match key.code {
            KeyCode::Up => self.panel.row = (row + options.len() - 1) % options.len(),
            KeyCode::Down => self.panel.row = (row + 1) % options.len(),
            KeyCode::Tab => self.step_on(at),
            KeyCode::BackTab => {
                if at > 0 {
                    self.panel.from_review = false;
                    self.focus_on(at - 1);
                }
            }
            KeyCode::Enter if plain => {
                let draft = self.draft_mut(&question.id);
                if !on_other && !(question.multiple && !draft.chosen.is_empty()) {
                    pick(draft, &question, options[row], false);
                }
                self.step_on(at);
            }
            KeyCode::Char(' ') if plain && !on_other => {
                pick(self.draft_mut(&question.id), &question, options[row], true);
            }
            KeyCode::Char(c @ '1'..='9')
                if plain
                    && (!on_other || (!question.choices.is_empty() && text_empty))
                    && (c as usize - '1' as usize) < options.len() - 1 =>
            {
                let pick_at = c as usize - '1' as usize;
                let multiple = question.multiple;
                pick(
                    self.draft_mut(&question.id),
                    &question,
                    options[pick_at],
                    multiple,
                );
                if multiple {
                    self.panel.row = pick_at;
                } else {
                    self.step_on(at);
                }
            }
            KeyCode::Char(' ') if plain && text_empty => {}
            _ if on_other => {
                self.type_key(&question, key);
            }
            KeyCode::Char(_) if plain => self.type_into_other(&question, key),
            _ => {}
        }
    }

    /// One at a time moves on from question `at`: to the next, or to the
    /// summary after the last (or back to it, when it came from there).
    fn step_on(&mut self, at: usize) {
        let count = self.open_questions().len();
        if self.panel.from_review || at + 1 >= count {
            self.panel.from_review = false;
            self.panel.review = Some(count);
        } else {
            self.focus_on(at + 1);
        }
    }

    fn review_key(&mut self, key: KeyEvent, selected: usize) {
        let count = self.open_questions().len();
        let selected = selected.min(count);
        let change = |app: &mut App, at: usize| {
            app.panel.review = None;
            app.panel.from_review = true;
            app.focus_on(at);
        };
        match key.code {
            KeyCode::Up => self.panel.review = Some(selected.saturating_sub(1)),
            KeyCode::Down => self.panel.review = Some((selected + 1).min(count)),
            KeyCode::Enter if selected == count => self.send_answers(),
            KeyCode::Enter => change(self, selected),
            KeyCode::BackTab => change(self, count - 1),
            KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < count => {
                change(self, c as usize - '1' as usize);
            }
            _ => {}
        }
    }
}

/// The panel's title: "Arko asks 2 questions".
pub(super) fn title(questions: &[AgentQuestion]) -> String {
    match questions.len() {
        1 => " Arko asks ".to_string(),
        count => format!(" Arko asks {count} questions "),
    }
}

/// The question chips while the panel is closed, a line each, the
/// questions attached nearest the message box.
pub fn draw_chip(frame: &mut Frame, app: &App, above: Rect) -> u16 {
    let chips = app.chips();
    let rows = (chips.len() as u16).min(above.height.saturating_sub(1));
    let theme = &app.theme;
    for (at, chip) in chips.iter().take(rows as usize).enumerate() {
        let style = match chip {
            Chip::Asks(_) => theme.accent().add_modifier(Modifier::BOLD),
            Chip::Replying(_) => theme.accent(),
            Chip::Open(_) => theme.dim(),
        };
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(
                crate::glyphs::render(&format!(" {} ", chip.text())).into_owned(),
                style,
            ),
        ];
        let pairs: &[(&str, &str)] = match chip {
            Chip::Replying(_) => &[("Backspace", "take off")],
            _ => &[("Alt+Q", "answer"), ("Ctrl+N", "not now")],
        };
        spans.push(Span::raw("  "));
        spans.extend(hints(theme, pairs));
        let rect = Rect::new(above.x, above.bottom() - 1 - at as u16, above.width, 1);
        frame.render_widget(Clear, rect);
        frame
            .buffer_mut()
            .set_line(rect.x, rect.y, &Line::from(spans), rect.width);
    }
    rows
}

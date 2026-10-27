//! The answer panel: the main agent's open questions, in a box above the
//! message box. It opens by itself when questions come while the user isn't
//! typing, else a chip says so, and Alt+Q opens it. Each question shows its
//! choices as chips that number keys pick, an Other field and "You decide".
//! An approval shows what it approves, with Save and Don't save. Enter sends what's filled in. Esc
//! answers in chat instead: the questions are attached to the message box
//! ("Replying to 2 questions"), and Backspace in an empty box takes them
//! off. Ctrl+N dismisses them ("Not now").
//!
//! While a question is open, only the panel shows it. Its card in the
//! transcript is one quiet line until then (see `transcript`).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear};

use super::app::{App, Effect};
use super::markdown;
use super::overlay::{Overlay, hints};
use super::select::Docked;
use super::wrap;
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
            text: Some(self.other.trim().to_string()).filter(|text| !text.is_empty()),
            you_decide: self.you_decide,
        }
    }
}

/// The panel's state, and the questions attached to the message box.
#[derive(Debug, Default)]
pub struct Panel {
    /// The panel shows and has the keys.
    pub open: bool,
    /// The question with the keys, by its id, so it keeps them as other
    /// questions come and go.
    pub focus: Option<String>,
    /// The focused question's Other field has the keys, not its chips:
    /// digits are text there.
    pub typing: bool,
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
    /// How many lines the panel is scrolled down from the focused
    /// question's start (PgDn), for a long question.
    pub scroll: usize,
    /// The window Alt+Q opened the panel over, which comes back when the
    /// panel closes.
    pub behind: Option<Overlay>,
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

/// Whether the Other field of `question` takes the keys when it gets the
/// focus: it has no choices to pick.
fn types_first(question: &AgentQuestion) -> bool {
    question.choices.is_empty() && question.approval.is_none()
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

    /// The chip above the message box, when the panel is closed.
    pub fn chip(&self) -> Option<Chip> {
        let open = self.open_questions();
        if self.panel.open || open.is_empty() {
            return None;
        }
        if !self.panel.attached.is_empty() {
            return Some(Chip::Replying(self.panel.attached.len()));
        }
        let new = open
            .iter()
            .filter(|question| !self.panel.seen.contains(&question.id))
            .count();
        Some(if new > 0 {
            Chip::Asks(open.len())
        } else {
            Chip::Open(open.len())
        })
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
        self.panel.typing = question.as_ref().is_some_and(types_first);
        self.panel.focus = question.map(|question| question.id);
        self.panel.scroll = 0;
    }

    /// The panel closes, and the window it opened over comes back.
    pub(super) fn close_panel(&mut self) {
        self.panel.open = false;
        self.panel.scroll = 0;
        if let Some(overlay) = self.panel.behind.take()
            && self.overlay.is_none()
        {
            self.overlay = Some(overlay);
        }
    }

    /// Esc and "Answer in chat": the panel closes and the open questions
    /// are attached to the message box.
    fn answer_in_chat(&mut self) {
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
    /// with its defaults.
    pub(super) fn not_now(&mut self) {
        let ids: Vec<String> = self
            .open_questions()
            .iter()
            .map(|question| question.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        self.close_panel();
        self.panel.attached.clear();
        self.panel.drafts.clear();
        self.effects_push(Effect::Input(Input::DismissQuestions(ids)));
        self.say("Dismissed. Arko goes on with its defaults");
    }

    /// Enter in the panel: sends what's filled in, naming every question
    /// it shows. The questions left unanswered stay open.
    fn send_answers(&mut self) {
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
        // Answers too long to send stay in the panel, with how short to
        // make them.
        if let Some(refusal) = crate::questions::answers_refusal(self.open_questions(), &answers) {
            self.say(refusal);
            return;
        }
        // The drafts stay until their questions settle (another front end
        // may have answered first).
        self.close_panel();
        self.effects_push(Effect::Input(Input::AnswerQuestions(answers)));
        self.transcript.follow();
    }

    /// The text field with the keys: the focused question's Other.
    fn field(&mut self) -> Option<&mut String> {
        let id = self.open_questions().get(self.focus_index())?.id.clone();
        Some(&mut self.panel.drafts.entry(id).or_default().other)
    }

    /// A paste while the panel has the keys: into its field, on one line.
    pub(super) fn panel_paste(&mut self, text: &str) {
        let text = text.replace(['\r', '\n'], " ");
        self.panel.typing = true;
        if let Some(field) = self.field() {
            field.push_str(&text);
        }
        self.typed_in_panel();
    }

    /// Typing an answer of its own leaves "You decide".
    fn typed_in_panel(&mut self) {
        if let Some(question) = self.reading.questions.get(self.focus_index()) {
            let id = question.id.clone();
            if let Some(draft) = self.panel.drafts.get_mut(&id) {
                draft.you_decide = false;
            }
        }
    }

    /// Picks chip `at` of the focused question: a choice, or "You decide"
    /// after them.
    fn pick(&mut self, at: usize) {
        let Some(question) = self.reading.questions.get(self.focus_index()).cloned() else {
            return;
        };
        let draft = self.panel.drafts.entry(question.id.clone()).or_default();
        if at == question.choices.len() {
            draft.you_decide = !draft.you_decide;
            if draft.you_decide {
                draft.chosen.clear();
                draft.other.clear();
            }
        } else if at < question.choices.len() {
            draft.you_decide = false;
            if draft.chosen.contains(&at) {
                draft.chosen.retain(|chosen| *chosen != at);
            } else if question.multiple {
                draft.chosen.push(at);
                draft.chosen.sort_unstable();
            } else {
                draft.chosen = vec![at];
            }
        }
    }

    /// ↓ and Tab: from a question's chips to its Other field, and from
    /// there to the next question (after the last, the first).
    fn focus_down(&mut self) {
        let (count, at) = (self.open_questions().len().max(1), self.focus_index());
        if self.panel.typing {
            self.focus_on((at + 1) % count);
        } else {
            self.panel.typing = true;
        }
    }

    /// ↑ and Shift+Tab: the other way.
    fn focus_up(&mut self) {
        let (count, at) = (self.open_questions().len().max(1), self.focus_index());
        if self.panel.typing {
            self.panel.typing = false;
        } else {
            self.focus_on((at + count - 1) % count);
            self.panel.typing = true;
        }
    }

    /// A key while the panel has the keys. True when it took it.
    pub(super) fn panel_key(&mut self, key: KeyEvent) -> bool {
        if !self.panel.open || self.open_questions().is_empty() {
            self.close_panel();
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let chip_count = self
            .open_questions()
            .get(self.focus_index())
            .map(|question| chips(question).len());
        match key.code {
            KeyCode::Esc => self.answer_in_chat(),
            KeyCode::Char('q') if alt => {}
            KeyCode::Char('n') if ctrl => self.not_now(),
            KeyCode::Enter if !alt && !ctrl => self.send_answers(),
            KeyCode::Down | KeyCode::Tab => self.focus_down(),
            KeyCode::Up | KeyCode::BackTab => self.focus_up(),
            KeyCode::PageDown => self.panel.scroll += 5,
            KeyCode::PageUp => self.panel.scroll = self.panel.scroll.saturating_sub(5),
            KeyCode::Backspace if self.panel.typing => {
                if let Some(field) = self.field() {
                    field.pop();
                }
            }
            // In the chips, a number picks one.
            KeyCode::Char(c @ '1'..='9')
                if !ctrl
                    && !alt
                    && !self.panel.typing
                    && chip_count.is_some_and(|count| (c as usize - '1' as usize) < count) =>
            {
                self.pick(c as usize - '1' as usize);
            }
            // Any other character typed goes in the Other field.
            KeyCode::Char(c) if !ctrl && !alt => {
                self.panel.typing = true;
                if let Some(field) = self.field() {
                    field.push(c);
                }
                self.typed_in_panel();
            }
            // Keys that work whatever the panel shows.
            KeyCode::Char('c' | 'l' | 'o') if ctrl => return false,
            _ => {}
        }
        true
    }
}

/// The chips a question shows, each with its number key: its choices, then
/// "You decide" with its default. An approval has Save and Don't save.
fn chips(question: &AgentQuestion) -> Vec<String> {
    let mut chips = question.choices.clone();
    if question.approval.is_none() {
        chips.push(match &question.default {
            Some(default) => format!("You decide ({default})"),
            None => "You decide".to_string(),
        });
    }
    chips
}

/// The panel's title: "Arko asks 2 questions".
fn title(questions: &[AgentQuestion]) -> String {
    let approvals = questions.iter().all(|question| question.approval.is_some());
    match (questions.len(), approvals) {
        (1, true) => " Arko asks to save a worker profile ".to_string(),
        (count, true) => format!(" Arko asks to save {count} worker profiles "),
        (1, false) => " Arko asks ".to_string(),
        (count, false) => format!(" Arko asks {count} questions "),
    }
}

/// The answer panel, above the status line. Returns the rows it takes.
pub fn draw_panel(frame: &mut Frame, app: &mut App, above: Rect, previous: Option<Docked>) -> u16 {
    let theme = app.theme.clone();
    let questions = app.open_questions().to_vec();
    if questions.is_empty() || above.height < 4 {
        return 0;
    }
    let focus = app.focus_index();
    let inner_width = (above.width as usize).saturating_sub(4).max(8);
    // At most two thirds of the screen: a window that keeps the focused
    // row in sight.
    let room = (above.height as usize * 2 / 3).saturating_sub(2).max(3);
    let mut lines: Vec<Line<'static>> = Vec::new();
    // The rows of each question, to keep the focused one in sight.
    let mut spans_of: Vec<(usize, usize)> = Vec::new();
    let mut cursor: Option<(usize, u16)> = None;
    let marker = crate::glyphs::render("› ").into_owned();
    for (at, question) in questions.iter().enumerate() {
        let focused = focus == at;
        // The questions of another ask come after a blank line.
        if at > 0 && questions[at - 1].item != question.item {
            lines.push(Line::default());
        }
        let first = lines.len();
        let style = if focused {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.text()
        };
        let number = question
            .number
            .map(|number| format!("{number}. "))
            .unwrap_or_default();
        let indent = 2 + wrap::width(&number);
        let width = inner_width.saturating_sub(indent).max(4);
        // An approval says what it approves up front, and its instructions
        // follow its choices.
        let mut tail: Vec<wrap::Wrapped> = Vec::new();
        let mut text = match &question.approval {
            Some(approval) => {
                let mut text = markdown::render(&approval.title(), style, &theme, width);
                for part in [approval.runs_on(), approval.description.clone()] {
                    text.extend(markdown::render(&part, theme.text(), &theme, width));
                }
                if !approval.instructions.is_empty() {
                    tail.extend(markdown::render("Instructions", theme.dim(), &theme, width));
                    for line in approval.instructions.lines() {
                        tail.extend(
                            wrap::wrap(&[Span::styled(line.to_string(), theme.text())], width, 0)
                                .into_iter()
                                .map(wrap::Wrapped::line),
                        );
                    }
                }
                text
            }
            None => markdown::render(&question.question, style, &theme, width),
        };
        if text.is_empty() {
            text.push(wrap::Wrapped::line(Vec::new()));
        }
        // A question that takes any number of its choices says so after
        // its text, on its line when it fits.
        if question.multiple && !question.choices.is_empty() && question.approval.is_none() {
            let hint = Span::styled(CHOOSE_ANY.to_string(), theme.dim());
            let last = text.last_mut().expect("a line at least");
            let used: usize = last
                .spans
                .iter()
                .map(|span| wrap::width(&span.content))
                .sum();
            if used + 2 + wrap::width(CHOOSE_ANY) <= width {
                last.spans.extend([Span::raw("  "), hint]);
            } else {
                text.push(wrap::Wrapped::line(vec![hint]));
            }
        }
        for (row, wrapped) in text.into_iter().enumerate() {
            let mut spans = if row == 0 {
                vec![Span::raw("  "), Span::styled(number.clone(), style)]
            } else {
                vec![Span::raw(" ".repeat(indent))]
            };
            spans.extend(wrapped.spans);
            lines.push(Line::from(spans));
        }
        let draft = app
            .panel
            .drafts
            .get(&question.id)
            .cloned()
            .unwrap_or_default();
        let on_chips = focused && !app.panel.typing;
        let mut row: Vec<Span<'static>> = vec![Span::styled(
            if on_chips {
                format!("  {marker}")
            } else {
                "    ".to_string()
            },
            theme.accent(),
        )];
        for (number, chip) in chips(question).into_iter().enumerate() {
            let on = if number == question.choices.len() {
                draft.you_decide
            } else {
                draft.chosen.contains(&number)
            };
            let style = if on { theme.chosen() } else { theme.dim() };
            row.push(Span::styled(format!(" {} {chip} ", number + 1), style));
            row.push(Span::raw(" "));
        }
        for wrapped in wrap::wrap(&row, inner_width, 4) {
            lines.push(Line::from(wrapped));
        }
        if focused || !draft.other.is_empty() {
            let typing = focused && app.panel.typing;
            let label = if typing {
                format!("  {marker}Other  ")
            } else {
                "    Other  ".to_string()
            };
            let mut other = vec![Span::styled(
                label.clone(),
                if typing { theme.accent() } else { theme.dim() },
            )];
            if draft.other.is_empty() && focused {
                let hint = if question.approval.is_some() {
                    "tell Arko what to change"
                } else {
                    "type an answer of your own"
                };
                other.push(Span::styled(hint.to_string(), theme.dim()));
            } else {
                other.push(Span::styled(draft.other.clone(), theme.text()));
            }
            if typing {
                cursor = Some((
                    lines.len(),
                    (wrap::width(&label) + wrap::width(&draft.other)) as u16,
                ));
            }
            lines.push(Line::from(other));
        }
        for wrapped in tail {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            spans.extend(wrapped.spans);
            lines.push(Line::from(spans));
        }
        spans_of.push((first, lines.len()));
    }
    // The focused question in sight from its start, and further down the
    // page (PgDn) for what doesn't fit.
    let (focus_first, focus_end) = spans_of[focus.min(spans_of.len() - 1)];
    let last_skip = lines.len().saturating_sub(room);
    let skip = focus_end.saturating_sub(room).min(focus_first);
    app.panel.scroll = app.panel.scroll.min(last_skip - skip.min(last_skip));
    let skip = (skip + app.panel.scroll).min(last_skip);
    let shown: Vec<Line<'static>> = lines.iter().skip(skip).take(room).cloned().collect();
    let mut pages: Vec<(&str, &str)> = Vec::new();
    if skip > 0 {
        pages.push(("PgUp", "more"));
    }
    if skip + room < lines.len() {
        pages.push(("PgDn", "more"));
    }
    let height = (shown.len() as u16 + 2).min(above.height);
    let rect = Rect::new(above.x, above.bottom() - height, above.width, height);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.borders())
        .border_style(theme.accent())
        .style(theme.base())
        .title(Line::from(Span::styled(
            title(&questions),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let rows = shown.len().min(inner.height as usize);
    for (row, line) in shown.iter().enumerate().take(rows) {
        frame.buffer_mut().set_line(
            inner.x + 1,
            inner.y + row as u16,
            line,
            inner.width.saturating_sub(1),
        );
    }
    // Its text is selected as the transcript's is: what was highlighted
    // stays while the panel shows the same.
    let area = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(1),
        rows as u16,
    );
    let lines: Vec<wrap::Wrapped> = shown
        .iter()
        .take(rows)
        .map(|line| wrap::Wrapped::line(line.spans.clone()))
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
    if let Some((row, column)) = cursor
        && row >= skip
        && row - skip < inner.height as usize
        && app.overlay.is_none()
    {
        frame.set_cursor_position(Position {
            x: (inner.x + 1 + column).min(inner.right().saturating_sub(1)),
            y: inner.y + (row - skip) as u16,
        });
    }
    pages.extend([
        ("Enter", "send"),
        ("Esc", "answer in chat"),
        ("Ctrl+N", "not now"),
    ]);
    let keys = Line::from({
        let mut spans = hints(&theme, &pages);
        spans.push(Span::raw(" "));
        spans
    });
    let width = keys.width() as u16;
    if width + 4 < rect.width {
        frame
            .buffer_mut()
            .set_line(rect.right() - width - 2, rect.bottom() - 1, &keys, width);
    }
    height
}

/// The chip above the message box (one row). Returns the rows it takes.
pub fn draw_chip(frame: &mut Frame, app: &App, above: Rect) -> u16 {
    let Some(chip) = app.chip() else {
        return 0;
    };
    if above.height < 2 {
        return 0;
    }
    let theme = &app.theme;
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
    let rect = Rect::new(above.x, above.bottom() - 1, above.width, 1);
    frame.render_widget(Clear, rect);
    frame
        .buffer_mut()
        .set_line(rect.x, rect.y, &Line::from(spans), rect.width);
    1
}

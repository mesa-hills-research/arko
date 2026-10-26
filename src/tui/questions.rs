//! The answer panel: the main agent's open questions, in a box above the
//! message box. It opens by itself when questions come while the user isn't
//! typing, else a chip says so, and Alt+Q opens it. Each question shows its
//! choices as chips that number keys pick, an Other field and "You decide",
//! and one "Anything else" box comes last. Enter sends what's filled in.
//! Esc answers in chat instead: the questions are attached to the message
//! box ("Replying to 2 questions"), and Backspace in an empty box takes them
//! off. Ctrl+N dismisses them ("Not now").

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
use super::overlay::hints;
use super::select::Docked;
use super::wrap;
use crate::questions::{AgentQuestion, Answer, Answers, QuestionAnswer};
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
    /// The row with the keys: a question (its place among the open ones),
    /// or "Anything else" after them.
    pub focus: usize,
    /// What is filled in, by question.
    pub drafts: HashMap<String, Draft>,
    pub anything_else: String,
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
            self.panel.open = false;
            self.panel.anything_else.clear();
            return;
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
        let last = self.open_questions().len();
        self.panel.focus = self.panel.focus.min(last);
    }

    /// Whether the bell rings for new questions (once).
    pub fn take_ring(&mut self) -> bool {
        std::mem::take(&mut self.panel.ring)
    }

    /// Alt+Q: the panel opens (or keeps the keys), on the first question
    /// not filled in.
    pub fn open_panel(&mut self) {
        if self.open_questions().is_empty() {
            self.say("No open questions");
            return;
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

    fn open_panel_at(&mut self, focus: usize) {
        self.panel.open = true;
        self.panel.focus = focus;
        // Attached questions are answered here now.
        self.panel.attached.clear();
        let ids: Vec<String> = self
            .open_questions()
            .iter()
            .map(|question| question.id.clone())
            .collect();
        self.panel.seen.extend(ids);
    }

    /// Esc and "Answer in chat": the panel closes and the open questions
    /// are attached to the message box.
    fn answer_in_chat(&mut self) {
        self.panel.open = false;
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
        if self.open_questions().is_empty() {
            return;
        }
        self.panel.open = false;
        self.panel.attached.clear();
        self.panel.drafts.clear();
        self.panel.anything_else.clear();
        self.effects_push(Effect::Input(Input::DismissQuestions(Vec::new())));
        self.say("Dismissed. The agent goes on with its defaults");
    }

    /// Enter in the panel: sends what's filled in. The questions left
    /// unanswered stay open.
    fn send_answers(&mut self) {
        let answers: Vec<QuestionAnswer> = self
            .open_questions()
            .iter()
            .filter_map(|question| {
                let answer = self.panel.drafts.get(&question.id)?.answer(question);
                (!answer.is_empty()).then(|| QuestionAnswer::new(question.id.clone(), answer))
            })
            .collect();
        let anything_else =
            Some(self.panel.anything_else.trim().to_string()).filter(|text| !text.is_empty());
        if answers.is_empty() && anything_else.is_none() {
            self.say("Nothing to send yet. Pick a choice, or type an answer");
            return;
        }
        for given in &answers {
            self.panel.drafts.remove(&given.id);
        }
        self.panel.anything_else.clear();
        self.panel.open = false;
        self.effects_push(Effect::Input(Input::AnswerQuestions(Answers {
            answers,
            anything_else,
        })));
        self.transcript.follow();
    }

    /// The text field with the keys: the focused question's Other, or
    /// "Anything else".
    fn field(&mut self) -> &mut String {
        let questions = &self.reading.questions;
        match questions.get(self.panel.focus) {
            Some(question) => {
                &mut self
                    .panel
                    .drafts
                    .entry(question.id.clone())
                    .or_default()
                    .other
            }
            None => &mut self.panel.anything_else,
        }
    }

    /// A paste while the panel has the keys: into its field, on one line.
    pub(super) fn panel_paste(&mut self, text: &str) {
        let text = text.replace(['\r', '\n'], " ");
        self.field().push_str(&text);
        self.typed_in_panel();
    }

    /// Typing an answer of its own leaves "You decide".
    fn typed_in_panel(&mut self) {
        if let Some(question) = self.reading.questions.get(self.panel.focus) {
            let id = question.id.clone();
            if let Some(draft) = self.panel.drafts.get_mut(&id) {
                draft.you_decide = false;
            }
        }
    }

    /// Picks chip `at` of the focused question: a choice, or "You decide"
    /// after them.
    fn pick(&mut self, at: usize) {
        let Some(question) = self.reading.questions.get(self.panel.focus).cloned() else {
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

    /// A key while the panel has the keys. True when it took it.
    pub(super) fn panel_key(&mut self, key: KeyEvent) -> bool {
        if !self.panel.open || self.open_questions().is_empty() {
            self.panel.open = false;
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let rows = self.open_questions().len() + 1;
        match key.code {
            KeyCode::Esc => self.answer_in_chat(),
            KeyCode::Char('q') if alt => {}
            KeyCode::Char('n') if ctrl => self.not_now(),
            KeyCode::Enter if !alt && !ctrl => self.send_answers(),
            KeyCode::Down | KeyCode::Tab => self.panel.focus = (self.panel.focus + 1) % rows,
            KeyCode::Up | KeyCode::BackTab => {
                self.panel.focus = (self.panel.focus + rows - 1) % rows;
            }
            KeyCode::Backspace => {
                self.field().pop();
            }
            KeyCode::Char(c @ '1'..='9')
                if !ctrl && !alt && self.panel.focus < rows - 1 && self.field().is_empty() =>
            {
                self.pick(c as usize - '1' as usize);
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.field().push(c);
                self.typed_in_panel();
            }
            // Keys that work whatever the panel shows.
            KeyCode::Char('c' | 'l' | 'o') if ctrl => return false,
            KeyCode::PageUp | KeyCode::PageDown => return false,
            _ => {}
        }
        true
    }
}

/// The chips a question shows: its choices, then "You decide" with its
/// default, each with its number key.
fn chips(question: &AgentQuestion) -> Vec<String> {
    let mut chips = question.choices.clone();
    chips.push(match &question.default {
        Some(default) => format!("You decide ({default})"),
        None => "You decide".to_string(),
    });
    chips
}

/// The answer panel, above the status line. Returns the rows it takes.
pub fn draw_panel(frame: &mut Frame, app: &mut App, above: Rect, previous: Option<Docked>) -> u16 {
    let theme = app.theme.clone();
    let questions = app.open_questions().to_vec();
    if questions.is_empty() || above.height < 4 {
        return 0;
    }
    let inner_width = (above.width as usize).saturating_sub(4).max(8);
    // At most two thirds of the screen: a window that keeps the focused
    // row in sight.
    let room = (above.height as usize * 2 / 3).saturating_sub(2).max(3);
    let mut lines: Vec<Line<'static>> = Vec::new();
    // The rows of each question, to keep the focused one in sight.
    let mut spans_of: Vec<(usize, usize)> = Vec::new();
    let mut cursor: Option<(usize, u16)> = None;
    for (at, question) in questions.iter().enumerate() {
        let focused = app.panel.focus == at;
        let first = lines.len();
        let marker = if focused {
            crate::glyphs::render("› ").into_owned()
        } else {
            "  ".to_string()
        };
        let style = if focused {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.text()
        };
        // Arko's own question shows without its id.
        let id = if crate::questions::is_agent_id(&question.id) {
            format!("{} ", question.id)
        } else {
            String::new()
        };
        let indent = 2 + wrap::width(&id);
        let mut text = markdown::render(
            &question.question,
            style,
            &theme,
            inner_width.saturating_sub(indent).max(4),
        );
        // A long question is cut, leaving room for its chips.
        let most = room.saturating_sub(3).max(1);
        if text.len() > most {
            text.truncate(most.saturating_sub(1));
            text.push(wrap::Wrapped::line(vec![Span::styled(
                crate::glyphs::render("… (the whole question is in the transcript)").into_owned(),
                theme.dim(),
            )]));
        }
        for (row, wrapped) in text.into_iter().enumerate() {
            let mut spans = if row == 0 {
                vec![
                    Span::styled(marker.clone(), theme.accent()),
                    Span::styled(id.clone(), theme.dim()),
                ]
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
        let mut row: Vec<Span<'static>> = vec![Span::raw("    ")];
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
            let label = "    Other  ";
            let mut other = vec![Span::styled(label.to_string(), theme.dim())];
            if draft.other.is_empty() && focused {
                other.push(Span::styled(
                    "type an answer of your own".to_string(),
                    theme.dim(),
                ));
            } else {
                other.push(Span::styled(draft.other.clone(), theme.text()));
            }
            if focused {
                cursor = Some((
                    lines.len(),
                    (wrap::width(label) + wrap::width(&draft.other)) as u16,
                ));
            }
            lines.push(Line::from(other));
        }
        spans_of.push((first, lines.len()));
    }
    let focused_else = app.panel.focus >= questions.len();
    let label = if focused_else {
        crate::glyphs::render("› Anything else  ").into_owned()
    } else {
        "  Anything else  ".to_string()
    };
    if focused_else {
        cursor = Some((
            lines.len(),
            (wrap::width(&label) + wrap::width(&app.panel.anything_else)) as u16,
        ));
    }
    spans_of.push((lines.len(), lines.len() + 1));
    lines.push(Line::from(vec![
        Span::styled(
            label,
            if focused_else {
                theme.accent().add_modifier(Modifier::BOLD)
            } else {
                theme.dim()
            },
        ),
        Span::styled(app.panel.anything_else.clone(), theme.text()),
    ]));
    let (focus_first, focus_end) = spans_of[app.panel.focus.min(spans_of.len() - 1)];
    let mut skip = 0;
    if lines.len() > room {
        skip = focus_end.saturating_sub(room).min(focus_first);
    }
    let shown: Vec<Line<'static>> = lines.iter().skip(skip).take(room).cloned().collect();
    let height = (shown.len() as u16 + 2).min(above.height);
    let rect = Rect::new(above.x, above.bottom() - height, above.width, height);
    frame.render_widget(Clear, rect);
    let count = questions.len();
    let title = if count == 1 {
        " Arko asks ".to_string()
    } else {
        format!(" Arko asks {count} questions ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.borders())
        .border_style(theme.accent())
        .style(theme.base())
        .title(Line::from(Span::styled(
            title,
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
    let keys = Line::from({
        let mut spans = hints(
            &theme,
            &[
                ("Enter", "send"),
                ("Esc", "answer in chat"),
                ("Ctrl+N", "not now"),
            ],
        );
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

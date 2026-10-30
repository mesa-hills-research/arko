//! The approval tray: the requests that wait for the user's approval (see
//! [`crate::approvals`]), docked above the status line, a row each.
//!
//! Alt+A gives the tray the keys and takes them back, and Esc gives them back
//! to the message box. While it has them, ↑↓ move between the rows, Space
//! selects one (all start selected), A selects all or none, V opens a row's
//! details, O turns its option on, Enter approves those selected and rejects
//! the others, and R rejects them all. Whatever the message box holds goes
//! to the agent with the answer, and the box empties.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Clear;

use super::app::{App, Effect};
use super::markdown;
use super::overlay::hints;
use super::theme::Theme;
use super::transcript::cut_end;
use super::wrap;
use crate::approvals::{
    ApprovalAnswers, ApprovalRequest, Look, approval_words_refusal, approvals_heading,
    approve_label, reject_label,
};
use crate::glyphs;
use crate::ui::Input;

/// The rows of a box (a profile's instructions, a skill's SKILL.md) in
/// sight at once. PgUp and PgDn scroll it.
const BOX_ROWS: usize = 8;

/// What the tray remembers between frames.
#[derive(Debug, Default)]
pub struct Tray {
    /// The tray has the keys.
    pub focused: bool,
    /// The request the cursor is on, by its id.
    pub cursor: Option<String>,
    /// The requests not selected. All start selected.
    pub unselected: HashSet<String>,
    /// The requests whose option is on.
    pub options: HashSet<String>,
    /// The request whose details show.
    pub open: Option<String>,
    /// How far its box is scrolled.
    pub scroll: usize,
    /// The requests the bell rang for.
    pub rung: HashSet<String>,
}

impl App {
    /// The requests in the approval tray.
    pub fn approvals(&self) -> &[ApprovalRequest] {
        &self.reading.approvals
    }

    /// Whether the main agent waits for the user: for an answer to its
    /// question, or for an approval.
    pub fn waits_for_user(&self) -> bool {
        self.agent_waits() || !self.approvals().is_empty()
    }

    /// The place of the row the cursor is on.
    fn tray_cursor(&self) -> usize {
        self.tray
            .cursor
            .as_ref()
            .and_then(|id| {
                self.approvals()
                    .iter()
                    .position(|request| &request.id == id)
            })
            .unwrap_or(0)
    }

    /// The tray's requests changed (a new reading): what was set for those
    /// gone goes, the cursor stays on a row, and the bell rings for new ones
    /// while the window isn't focused.
    pub(super) fn approvals_changed(&mut self) {
        let ids: HashSet<String> = self
            .approvals()
            .iter()
            .map(|request| request.id.clone())
            .collect();
        self.tray.unselected.retain(|id| ids.contains(id));
        self.tray.options.retain(|id| ids.contains(id));
        if self.tray.open.as_ref().is_some_and(|id| !ids.contains(id)) {
            self.tray.open = None;
            self.tray.scroll = 0;
        }
        if ids.iter().any(|id| !self.tray.rung.contains(id)) {
            self.panel.ring |= !self.focused;
        }
        self.tray.rung = ids.clone();
        if ids.is_empty() {
            self.tray.focused = false;
            self.tray.cursor = None;
            return;
        }
        if self.tray.cursor.as_ref().is_none_or(|id| !ids.contains(id)) {
            self.tray.cursor = self.approvals().first().map(|request| request.id.clone());
        }
    }

    /// Alt+A: the tray gets the keys, or gives them back.
    pub(super) fn focus_tray(&mut self) {
        if self.tray.focused {
            self.tray.focused = false;
            return;
        }
        if self.approvals().is_empty() {
            self.say("No approvals wait");
            return;
        }
        if self.panel.open {
            self.close_panel();
        }
        self.list_focus = None;
        self.tray.focused = true;
    }

    /// The tray's keys, while it has them. False for the keys that work
    /// anywhere (Ctrl+C, Ctrl+L, Ctrl+O).
    pub(super) fn tray_key(&mut self, key: KeyEvent) -> bool {
        if !self.tray.focused || self.approvals().is_empty() {
            self.tray.focused = false;
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let count = self.approvals().len();
        let at = self.tray_cursor();
        let id = self.approvals()[at].id.clone();
        match key.code {
            KeyCode::Char('c' | 'l' | 'o') if ctrl => return false,
            _ if ctrl || alt => {}
            KeyCode::Esc => self.tray.focused = false,
            KeyCode::Up | KeyCode::Char('k') => self.move_tray_cursor(at, at.checked_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_tray_cursor(at, (at + 1 < count).then_some(at + 1));
            }
            KeyCode::Char(' ') if count > 1 => {
                if !self.tray.unselected.remove(&id) {
                    self.tray.unselected.insert(id);
                }
            }
            KeyCode::Char('a') if count > 1 => {
                if self.tray.unselected.is_empty() {
                    self.tray.unselected = self
                        .approvals()
                        .iter()
                        .map(|request| request.id.clone())
                        .collect();
                } else {
                    self.tray.unselected.clear();
                }
            }
            KeyCode::Char('v') => {
                self.tray.scroll = 0;
                self.tray.open = if self.tray.open.as_ref() == Some(&id) {
                    None
                } else {
                    Some(id)
                };
            }
            KeyCode::Char('o') if self.approvals()[at].approval.option().is_some() => {
                if !self.tray.options.remove(&id) {
                    self.tray.options.insert(id);
                }
            }
            KeyCode::PageDown => self.tray.scroll += BOX_ROWS / 2,
            KeyCode::PageUp => self.tray.scroll = self.tray.scroll.saturating_sub(BOX_ROWS / 2),
            KeyCode::Enter => self.answer_tray(true),
            KeyCode::Char('r') => self.answer_tray(false),
            KeyCode::Char(_) => self.say("Esc goes back to the message box"),
            _ => {}
        }
        true
    }

    fn move_tray_cursor(&mut self, from: usize, to: Option<usize>) {
        let Some(to) = to else {
            return;
        };
        if to != from {
            self.tray.cursor = self.approvals().get(to).map(|request| request.id.clone());
            self.tray.scroll = 0;
        }
    }

    /// Enter (`approve`: those selected are approved, the others rejected)
    /// or R (all rejected). The message box's text goes with the answer,
    /// and the box empties.
    fn answer_tray(&mut self, approve: bool) {
        let words = self.editor.text();
        if let Some(refusal) = approval_words_refusal(&words) {
            self.say(refusal);
            return;
        }
        let requests = self.approvals().to_vec();
        let tray = &self.tray;
        let answers = ApprovalAnswers::of(
            &requests,
            |id| approve && !tray.unselected.contains(id),
            |id| tray.options.contains(id),
            &words,
        );
        if !words.trim().is_empty() {
            self.editor.take();
        }
        self.tray.focused = false;
        self.effects_push(Effect::Input(Input::AnswerApprovals(answers)));
        self.transcript.follow();
    }
}

/// The tray's lines at `width`: its heading with the keys, a row for each
/// request, and the details of the open one. Also the line the cursor's row
/// is on.
fn tray_lines(app: &App, width: usize) -> (Vec<Line<'static>>, usize) {
    let theme = &app.theme;
    let requests = app.approvals();
    let count = requests.len();
    let tray = &app.tray;
    let cursor = app.tray_cursor();
    let selected = requests
        .iter()
        .filter(|request| !tray.unselected.contains(&request.id))
        .count();
    let mut lines = Vec::new();
    // The heading, with the keys at its end.
    let heading = format!(" {}", approvals_heading(count));
    let approve = approve_label(selected, count).to_lowercase();
    let reject = reject_label(count).to_lowercase();
    let open = tray
        .open
        .as_ref()
        .and_then(|id| requests.iter().find(|request| &request.id == id));
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    if tray.focused {
        if count > 1 {
            pairs.extend([("Space", "select"), ("A", "all")]);
        }
        pairs.push(("Enter", approve.as_str()));
        pairs.push(("R", reject.as_str()));
        pairs.push(("V", if open.is_some() { "close" } else { "view" }));
        if requests[cursor].approval.option().is_some() && open.is_some() {
            pairs.push(("O", "option"));
        }
        pairs.push(("Esc", "message"));
    } else {
        pairs.push(("Alt+A", "answer"));
    }
    let heading_width = wrap::width(&heading);
    let mut keys = fitting(theme, &pairs, width.saturating_sub(heading_width + 2));
    let keys_width: usize = keys.iter().map(|span| wrap::width(&span.content)).sum();
    let mut first = vec![Span::styled(
        heading,
        theme.accent().add_modifier(Modifier::BOLD),
    )];
    if !keys.is_empty() {
        first.push(Span::raw(" ".repeat(
            width.saturating_sub(heading_width + keys_width + 1).max(2),
        )));
        first.append(&mut keys);
    }
    lines.push(Line::from(first));
    // The rows.
    let names = requests
        .iter()
        .map(|request| wrap::width(request.approval.name()))
        .max()
        .unwrap_or(0)
        .min(24);
    let scopes = requests
        .iter()
        .map(|request| wrap::width(request.approval.scope()))
        .max()
        .unwrap_or(0)
        .min(16);
    let pointer = glyphs::render("› ").into_owned();
    let mut cursor_line = 1;
    for (at, request) in requests.iter().enumerate() {
        let approval = &request.approval;
        let here = tray.focused && at == cursor;
        if at == cursor {
            cursor_line = lines.len();
        }
        let mut spans = vec![if here {
            Span::styled(format!(" {pointer}"), theme.accent())
        } else {
            Span::raw("   ")
        }];
        if count > 1 {
            let mark = if tray.unselected.contains(&request.id) {
                "[ ] "
            } else {
                "[x] "
            };
            spans.push(Span::styled(mark.to_string(), theme.text()));
        }
        let style = if here {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.text()
        };
        let pad = |text: &str, columns: usize| {
            let text = cut_end(text, columns);
            let room = columns.saturating_sub(wrap::width(&text));
            format!("{text}{}  ", " ".repeat(room))
        };
        spans.push(Span::styled(
            pad(&approval.kind_word().to_lowercase(), 7),
            theme.dim(),
        ));
        spans.push(Span::styled(pad(approval.name(), names), style));
        // Every project stands out.
        let reach = if approval.every_project() {
            theme.warning()
        } else {
            theme.text()
        };
        spans.push(Span::styled(pad(approval.scope(), scopes), reach));
        let used: usize = spans.iter().map(|span| wrap::width(&span.content)).sum();
        let summary = cut_end(&approval.summary(), width.saturating_sub(used));
        spans.push(Span::styled(summary, theme.dim()));
        lines.push(Line::from(spans));
        if open.is_some_and(|open| open.id == request.id) {
            lines.extend(detail_lines(app, request, width));
        }
    }
    (lines, cursor_line)
}

/// The details of `request`, under its row.
fn detail_lines(app: &App, request: &ApprovalRequest, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let indent = 6;
    let room = width.saturating_sub(indent + 1).max(8);
    let pad = || Span::raw(" ".repeat(indent));
    let mut lines = Vec::new();
    let put = |rows: Vec<Vec<Span<'static>>>, lines: &mut Vec<Line<'static>>| {
        for row in rows {
            let mut spans = vec![pad()];
            spans.extend(row);
            lines.push(Line::from(spans));
        }
    };
    let approval = &request.approval;
    put(
        vec![vec![Span::styled(
            approval.title(),
            theme.text().add_modifier(Modifier::BOLD),
        )]],
        &mut lines,
    );
    for detail in approval.details() {
        match detail.look {
            Look::Text => put(
                markdown::render(&detail.text, theme.text(), theme, room)
                    .into_iter()
                    .map(|line| line.spans)
                    .collect(),
                &mut lines,
            ),
            Look::Heading => put(
                vec![vec![Span::styled(detail.text.clone(), theme.dim())]],
                &mut lines,
            ),
            Look::Warning => put(
                wrap::wrap(
                    &[Span::styled(detail.text.clone(), theme.warning())],
                    room,
                    0,
                ),
                &mut lines,
            ),
            Look::File => put(
                wrap::wrap(&[Span::styled(detail.text.clone(), theme.text())], room, 2),
                &mut lines,
            ),
            Look::Box => {
                let rule = glyphs::render("│ ").into_owned();
                let mut rows = Vec::new();
                for line in detail.text.lines() {
                    let line = wrap::clean(line);
                    for row in wrap::wrap(
                        &[Span::styled(line, theme.text())],
                        room.saturating_sub(2).max(4),
                        0,
                    ) {
                        let mut spans = vec![Span::styled(rule.clone(), theme.dim())];
                        spans.extend(row);
                        rows.push(spans);
                    }
                }
                let last_skip = rows.len().saturating_sub(BOX_ROWS);
                let skip = app.tray.scroll.min(last_skip);
                let more = rows.len() > BOX_ROWS;
                let shown: Vec<Vec<Span<'static>>> =
                    rows.into_iter().skip(skip).take(BOX_ROWS).collect();
                put(shown, &mut lines);
                if more {
                    let mut pairs = Vec::new();
                    if skip > 0 {
                        pairs.push(("PgUp", "more"));
                    }
                    if skip < last_skip {
                        pairs.push(("PgDn", "more"));
                    }
                    put(vec![hints(theme, &pairs)], &mut lines);
                }
            }
        }
    }
    if let Some(option) = approval.option() {
        let mark = if app.tray.options.contains(&request.id) {
            "[x] "
        } else {
            "[ ] "
        };
        put(
            vec![vec![
                Span::styled(mark.to_string(), theme.text()),
                Span::styled(option, theme.text()),
            ]],
            &mut lines,
        );
    }
    lines
}

/// As many of the keys `pairs` as fit in `width`, from the first.
fn fitting(theme: &Theme, pairs: &[(&str, &str)], width: usize) -> Vec<Span<'static>> {
    let mut count = pairs.len();
    loop {
        let spans = hints(theme, &pairs[..count]);
        let used: usize = spans.iter().map(|span| wrap::width(&span.content)).sum();
        if count == 0 || used <= width {
            return spans;
        }
        count -= 1;
    }
}

/// Draws the tray at the bottom of `above` and returns its height (0 when
/// no approvals wait).
pub fn draw_tray(frame: &mut Frame, app: &mut App, above: Rect) -> u16 {
    if app.approvals().is_empty() || above.height < 3 {
        return 0;
    }
    let width = above.width as usize;
    let (lines, cursor_line) = tray_lines(app, width.saturating_sub(1));
    let room = (above.height as usize * 2 / 3).max(2);
    let height = lines.len().min(room);
    // The heading stays, and the rest scrolls to keep the cursor's row and
    // what follows it in sight.
    let body = height - 1;
    let skip = cursor_line
        .saturating_sub(1)
        .min(lines.len().saturating_sub(1 + body));
    let rect = Rect::new(
        above.x,
        above.bottom() - height as u16,
        above.width,
        height as u16,
    );
    frame.render_widget(Clear, rect);
    let theme = &app.theme;
    let surface = if theme.is_terminal() {
        Style::new()
    } else {
        Style::new().bg(theme.raised)
    };
    frame.buffer_mut().set_style(rect, surface);
    let shown = std::iter::once(&lines[0]).chain(lines.iter().skip(1 + skip).take(body));
    for (row, line) in shown.enumerate() {
        frame
            .buffer_mut()
            .set_line(rect.x, rect.y + row as u16, line, rect.width);
    }
    height as u16
}

#[cfg(test)]
mod tests;

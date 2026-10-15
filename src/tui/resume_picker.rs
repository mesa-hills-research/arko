//! `/resume`'s picker: the session's conversations, newest first, each by
//! the first line of its first message and when it was last used, with the
//! current one marked. It opens on the one used last before the current
//! one, so going back to it is one key. Enter sends the session the
//! `/resume N` line a user would type, so the line prompt, the page and
//! the picker switch one way.

use chrono::{DateTime, Local};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::overlay::{centred, footer, frame_box, put};
use super::theme::Theme;
use super::transcript::cut_end;
use super::wrap::{self, width as text_width};
use crate::conversations::{WHEN_WIDTH, when};
use crate::glyphs;
use crate::remote::ConversationEntry;

/// The tag after the current conversation's time.
const CURRENT: &str = "  current";

/// The picker, open.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumePicker {
    /// Newest first, as the session lists them.
    pub entries: Vec<ConversationEntry>,
    pub selected: usize,
    /// When it opened. Each row says how long before this it was used.
    pub now: DateTime<Local>,
}

/// What a key did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Stay,
    Close,
    /// Enter on the conversation the user is in.
    Current,
    /// Go back to conversation number N.
    Resume(usize),
}

impl ResumePicker {
    /// The picker for `entries` (newest first), on the conversation used
    /// last other than the current one.
    pub fn open(entries: Vec<ConversationEntry>) -> Self {
        let selected = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.current)
            // The latest use, and between equals the newer conversation.
            .max_by(|(a, x), (b, y)| x.last_used.cmp(&y.last_used).then(b.cmp(a)))
            .map_or(0, |(n, _)| n);
        Self {
            entries,
            selected,
            now: Local::now(),
        }
    }

    pub fn key(&mut self, key: KeyEvent, close: bool) -> Outcome {
        let last = self.entries.len().saturating_sub(1);
        match key.code {
            _ if close => return Outcome::Close,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::PageUp | KeyCode::Home => self.selected = 0,
            KeyCode::PageDown | KeyCode::End => self.selected = last,
            KeyCode::Enter => {
                return match self.entries.get(self.selected) {
                    Some(entry) if entry.current => Outcome::Current,
                    Some(entry) => Outcome::Resume(entry.number),
                    None => Outcome::Close,
                };
            }
            _ => {}
        }
        Outcome::Stay
    }
}

/// Draws the picker over `area`.
pub fn draw(frame: &mut Frame, theme: &Theme, area: Rect, picker: &ResumePicker) {
    let width = 88.min(area.width.saturating_sub(2));
    let inner_width = width.saturating_sub(4) as usize;
    let now = picker.now;
    let tag = if picker.entries.iter().any(|entry| entry.current) {
        CURRENT.len()
    } else {
        0
    };
    // The marker, the title, a gap, the time, and the tag.
    let title_width = inner_width.saturating_sub(2 + 2 + WHEN_WIDTH + tag).max(8);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (n, entry) in picker.entries.iter().enumerate() {
        let chosen = n == picker.selected;
        let marker = if chosen {
            Span::styled(format!("{} ", glyphs::render("▸")), theme.accent())
        } else {
            Span::raw("  ")
        };
        let style = if chosen {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.text()
        };
        let title = cut_end(&wrap::clean(&entry.title), title_width);
        let pad = title_width.saturating_sub(text_width(&title)) + 2;
        let used = entry.last_used.map(|at| when(at, now)).unwrap_or_default();
        let mut spans = vec![
            marker,
            Span::styled(title, style),
            Span::raw(" ".repeat(pad)),
            Span::styled(format!("{used:<WHEN_WIDTH$}"), theme.dim()),
        ];
        if entry.current {
            spans.push(Span::styled(CURRENT, theme.accent()));
        }
        lines.push(Line::from(spans));
    }
    let height = (lines.len() as u16 + 4).min(area.height);
    let rect = centred(area, width, height);
    let inner = frame_box(frame, rect, theme, "Conversations");
    let body = Rect::new(
        inner.x + 1,
        inner.y + 1,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );
    let visible = body.height as usize;
    let scroll = picker
        .selected
        .saturating_sub(visible / 2)
        .min(lines.len().saturating_sub(visible));
    put(frame, body, &lines, scroll);
    footer(
        frame,
        rect,
        theme,
        &[("↑ ↓", "choose"), ("Enter", "switch"), ("Esc", "close")],
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::Utc;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Noon today: the tests open the picker then, so "2 hours ago" is
    /// the same day at any hour they run.
    pub(crate) fn noon() -> DateTime<Local> {
        Local::now()
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .unwrap()
    }

    fn entry(number: usize, title: &str, hours: i64, current: bool) -> ConversationEntry {
        ConversationEntry {
            number,
            title: title.into(),
            last_used: Some(noon().with_timezone(&Utc) - chrono::Duration::hours(hours)),
            current,
        }
    }

    fn entries() -> Vec<ConversationEntry> {
        vec![
            entry(
                3,
                "Make the login page remember the user between visits, with a cookie that lasts a month",
                0,
                true,
            ),
            entry(2, "Add a README with a usage section", 2, false),
            entry(1, "Fix the build on Windows", 3, false),
        ]
    }

    /// The picker as a screen `width` columns wide shows it, row by row.
    pub(crate) fn screen(picker: &ResumePicker, width: u16) -> Vec<String> {
        let height = 12;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, &Theme::terminal(), frame.area(), picker))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn press(picker: &mut ResumePicker, code: KeyCode) -> Outcome {
        picker.key(KeyEvent::new(code, KeyModifiers::NONE), false)
    }

    /// It opens on the conversation used last before the current one, and
    /// Enter goes back to the one chosen. Enter on the current one doesn't
    /// switch.
    #[test]
    fn enter_goes_back_to_the_conversation_chosen() {
        let mut picker = ResumePicker::open(entries());
        assert_eq!(picker.selected, 1);
        assert_eq!(press(&mut picker, KeyCode::Down), Outcome::Stay);
        assert_eq!(press(&mut picker, KeyCode::Down), Outcome::Stay);
        assert_eq!(press(&mut picker, KeyCode::Enter), Outcome::Resume(1));
        assert_eq!(press(&mut picker, KeyCode::Home), Outcome::Stay);
        assert_eq!(press(&mut picker, KeyCode::Enter), Outcome::Current);
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), true),
            Outcome::Close
        );
        // After /new, none is current: the latest is chosen.
        let mut after_new = entries();
        after_new[0].current = false;
        assert_eq!(ResumePicker::open(after_new).selected, 0);
    }

    /// On an 80-column screen each row fits, its parts line up, a long
    /// title is cut, and the keys show along the bottom.
    #[test]
    fn the_rows_line_up_on_an_80_column_screen() {
        let picker = ResumePicker {
            now: noon(),
            ..ResumePicker::open(entries())
        };
        let rows = screen(&picker, 80);
        let row = |text: &str| {
            rows.iter()
                .find(|row| row.contains(text))
                .unwrap_or_else(|| panic!("no {text:?} in {rows:#?}"))
                .clone()
        };
        // The column `text` starts at in its row.
        let column = |text: &str| {
            let row = row(text);
            row[..row.find(text).unwrap()].chars().count()
        };
        let first = row("Make the login page");
        assert!(first.contains('…') && first.contains("just now"), "{first}");
        assert!(row("Add a README").contains("▸ Add a README"));
        assert_eq!(column("Add a README"), column("Make the login page"));
        assert_eq!(column("just now"), column("2 hours ago"));
        assert_eq!(column("2 hours ago"), column("3 hours ago"));
        assert!(column("current") > column("just now"));
        assert!(row("Esc").contains("Enter  switch"));
        assert!(rows.iter().all(|row| row.chars().count() <= 80));
    }
}

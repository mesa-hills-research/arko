//! Text outside the transcript that the mouse selects as it selects the
//! transcript's: the agent's question, docked above the status line. A drag
//! highlights it and copies it when the button is let go, a double click
//! takes a word, and a triple click a line.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};

use super::theme::Theme;
use super::transcript::{Range, copied, highlight, selected_columns, text_line, word_columns};
use super::wrap::Wrapped;

/// A place in the text: a line, and a column of it.
pub type Spot = (usize, usize);

/// Text drawn outside the transcript, as last drawn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Docked {
    /// What it shows (the question's ID).
    pub id: u64,
    /// Where its lines are.
    pub area: Rect,
    pub lines: Vec<Wrapped>,
    /// What is highlighted: from where the drag began to where it is.
    pub selection: Option<(Spot, Spot)>,
}

impl Docked {
    /// Whether the screen's `x`, `y` is on the text's area.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.area.contains(Position { x, y })
    }

    /// The place at the screen's `x`, `y`: above the text is its start,
    /// below it its end. `None` when it has no lines.
    pub fn spot_at(&self, x: u16, y: u16) -> Option<Spot> {
        let last = self.lines.len().checked_sub(1)?;
        if y < self.area.y {
            return Some((0, 0));
        }
        let row = (y - self.area.y) as usize;
        if row > last {
            return Some((last, usize::MAX / 2));
        }
        Some((row, x.saturating_sub(self.area.x) as usize))
    }

    /// The word at `spot`: what a double click selects.
    pub fn word_at(&self, (line, col): Spot) -> Option<(Spot, Spot)> {
        let (start, end) = word_columns(self.lines.get(line)?, col)?;
        Some(((line, start), (line, end)))
    }

    /// The line of text at `spot` (the display lines it was wrapped into):
    /// what a triple click selects.
    pub fn line_at(&self, (line, _): Spot) -> Option<(Spot, Spot)> {
        let (first, last) = text_line(&self.lines, line)?;
        Some(((first, 0), (last, usize::MAX / 2)))
    }

    /// The selection's ends in the order of the text, as the transcript's
    /// helpers take them (one block).
    fn range(&self) -> Option<Range> {
        let (a, b) = self.selection?;
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        Some(((0, a.0, a.1), (0, b.0, b.1)))
    }

    /// The highlighted text, as written: with the lines wrapped only for
    /// the screen joined again.
    pub fn selected_text(&self) -> Option<String> {
        let range = self.range()?;
        let lines = self.lines.iter().enumerate();
        copied(lines.map(|(line, wrapped)| ((0, line), wrapped)), range)
    }

    /// Highlights what is selected, in `buffer`.
    pub fn draw_selection(&self, buffer: &mut Buffer, theme: &Theme) {
        let Some(range) = self.range() else {
            return;
        };
        let rows = self
            .lines
            .iter()
            .enumerate()
            .take(self.area.height as usize);
        for (line, wrapped) in rows {
            if let Some((from, to)) = selected_columns(range, (0, line), wrapped) {
                for x in from..to.min(self.area.width as usize) {
                    let at = (self.area.x + x as u16, self.area.y + line as u16);
                    buffer[at].set_style(highlight(theme));
                }
            }
        }
    }
}

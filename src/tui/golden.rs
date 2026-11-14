//! A drawn screen as text, with its colours and attributes, for the goldens in
//! `tests/screens/`, the frame recordings and dbg-snapshot's `screen.styled`.
//! The lines of a frame are:
//!
//! ```text
//! size 80 24
//! cursor 2 23 visible default
//! text 0 "  ⣷ Working…"
//! style 0 2 3 fg=#7f8ea3
//! style 0 4 13 fg=#d8dee9 bold
//! text 1 ""
//! ```
//!
//! The `cursor` line has the cursor's column and row, then `visible default`
//! or `hidden` (a hidden cursor is at 0 0). A `text` line gives a row's symbols as a JSON string, as
//! serde_json writes one, without the second cell of a wide character and
//! without trailing spaces. Each `style` line after it gives a run of cells
//! with the same style that isn't the default. It has the row, the first
//! column, the column after the last, and then `fg=`, `bg=` and `ul=` (the
//! underline colour, whether or not `underline` is set), and `underline`,
//! `bold`, `dim`, `italic`, `blink`, `rapid-blink`, `reverse`, `hidden` and
//! `strike`, in that order, for those that are set. A colour is `#rrggbb`,
//! `idx:N` or one of the 16 names, and the terminal's own colour (`Reset`) is
//! left out. A wide character's second cell has the style of its first, as a
//! terminal paints it. Every line ends in a newline.

use std::fmt::Write;

use termelon::buffer::{Buffer, CellRef};
use termelon::layout::Position;
use termelon::style::{Color, Modifier};
#[cfg(test)]
use termelon::{Frame, caps::ColorDepth, testing::TestTerminal};

/// A terminal in memory for the view's tests, `width` by `height` cells,
/// which shows 24-bit colour as the view's themes paint.
#[cfg(test)]
pub(super) fn terminal(width: u16, height: u16) -> TestTerminal {
    let terminal = TestTerminal::new(width, height);
    let mut caps = terminal.capabilities().clone();
    caps.colors = ColorDepth::TrueColor;
    terminal.with_caps(caps)
}

/// The rows a `width` by `height` terminal shows once `draw` drew a frame,
/// without their trailing spaces.
#[cfg(test)]
pub(super) fn rows(width: u16, height: u16, draw: impl FnOnce(&mut Frame<'_>)) -> Vec<String> {
    let mut terminal = terminal(width, height);
    terminal.draw(draw).unwrap();
    terminal.screen().rows()
}

/// The rows and the cells a `width` by `height` terminal shows once `draw`
/// drew a frame, the cells as the frame drew them.
#[cfg(test)]
pub(super) fn drawn(
    width: u16,
    height: u16,
    draw: impl FnOnce(&mut Frame<'_>),
) -> (Vec<String>, Buffer) {
    let mut terminal = terminal(width, height);
    let buffer = terminal.draw(draw).unwrap().buffer().clone();
    (terminal.screen().rows(), buffer)
}

/// The frame `draw` draws on `terminal`, in the golden format, with the
/// cursor where the terminal shows it.
#[cfg(test)]
pub(super) fn frame(terminal: &mut TestTerminal, draw: impl FnOnce(&mut Frame<'_>)) -> String {
    let buffer = terminal.draw(draw).unwrap().buffer().clone();
    let cursor = terminal.cursor().map(|cursor| cursor.position);
    styled(&buffer, cursor)
}

/// `buffer` in the golden format, with the cursor at `cursor`, or hidden.
pub(super) fn styled(buffer: &Buffer, cursor: Option<Position>) -> String {
    let (width, height) = (buffer.area().width, buffer.area().height);
    let mut out = format!("size {width} {height}\n");
    match cursor {
        Some(Position { x, y }) => writeln!(out, "cursor {x} {y} visible default").unwrap(),
        None => writeln!(out, "cursor 0 0 hidden").unwrap(),
    }
    for y in 0..height {
        let mut text = String::new();
        let mut row = Vec::with_capacity(width as usize);
        // The look of the character being written. A terminal paints the
        // cells a wide character covers (empty ones) in its look.
        let mut look = Look::default();
        for x in 0..width {
            let Some(cell) = buffer.cell((x, y)) else {
                continue;
            };
            if !cell.symbol().is_empty() {
                text.push_str(cell.symbol());
                look = Look::of(&cell);
            }
            row.push(look);
        }
        let text = serde_json::to_string(text.trim_end_matches(' ')).unwrap();
        writeln!(out, "text {y} {text}").unwrap();
        let mut x = 0;
        while x < width {
            let start = x;
            while x < width && row[x as usize] == row[start as usize] {
                x += 1;
            }
            let attributes = attributes(&row[start as usize]);
            if !attributes.is_empty() {
                writeln!(out, "style {y} {start} {x} {attributes}").unwrap();
            }
        }
    }
    out
}

/// What a cell shows besides its symbol.
#[derive(Clone, Copy, Default, PartialEq)]
struct Look {
    fg: Color,
    bg: Color,
    underline: Color,
    modifier: Modifier,
}

impl Look {
    fn of(cell: &CellRef) -> Self {
        Self {
            fg: cell.fg(),
            bg: cell.bg(),
            underline: cell.underline_color(),
            modifier: cell.modifier(),
        }
    }
}

/// A look as the words of a `style` line, empty for the default.
fn attributes(look: &Look) -> String {
    let mut words = Vec::new();
    for (name, color) in [("fg", look.fg), ("bg", look.bg), ("ul", look.underline)] {
        if let Some(color) = colour(color) {
            words.push(format!("{name}={color}"));
        }
    }
    for (flag, name) in [
        (Modifier::UNDERLINED, "underline"),
        (Modifier::BOLD, "bold"),
        (Modifier::DIM, "dim"),
        (Modifier::ITALIC, "italic"),
        (Modifier::SLOW_BLINK, "blink"),
        (Modifier::RAPID_BLINK, "rapid-blink"),
        (Modifier::REVERSED, "reverse"),
        (Modifier::HIDDEN, "hidden"),
        (Modifier::CROSSED_OUT, "strike"),
    ] {
        if look.modifier.contains(flag) {
            words.push(name.to_string());
        }
    }
    words.join(" ")
}

/// A colour's name in the format, none for the terminal's own.
fn colour(color: Color) -> Option<String> {
    Some(match color {
        Color::Reset => return None,
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "gray".into(),
        Color::DarkGray => "dark-gray".into(),
        Color::LightRed => "light-red".into(),
        Color::LightGreen => "light-green".into(),
        Color::LightYellow => "light-yellow".into(),
        Color::LightBlue => "light-blue".into(),
        Color::LightMagenta => "light-magenta".into(),
        Color::LightCyan => "light-cyan".into(),
        Color::White => "white".into(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(n) => format!("idx:{n}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use termelon::layout::Rect;
    use termelon::style::Style;

    /// Escapes, wide characters, runs, trailing cells with a background, every
    /// attribute in its place and the cursor.
    #[test]
    fn a_frame_is_written_as_text_with_runs_of_style() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 4));
        let blue = Style::new().fg(Color::Rgb(0x7f, 0x8e, 0xa3));
        buffer.set_string(0, 0, "hi \"q\" \\", blue);
        buffer.set_string(3, 0, "\"q\"", blue.add_modifier(Modifier::BOLD));
        buffer.set_string(0, 1, "日本 ok", Style::new().bg(Color::Red));
        buffer.set_string(7, 1, "é", Style::new().fg(Color::Indexed(208)));
        buffer.set_style(Rect::new(4, 2, 4, 1), Style::new().bg(Color::DarkGray));
        buffer.set_string(
            0,
            3,
            "x",
            Style::new()
                .fg(Color::LightCyan)
                .bg(Color::Gray)
                .underline_color(Color::White)
                .add_modifier(Modifier::all()),
        );
        assert_eq!(
            styled(&buffer, Some(Position::new(2, 3))),
            concat!(
                "size 14 4\n",
                "cursor 2 3 visible default\n",
                "text 0 \"hi \\\"q\\\" \\\\\"\n",
                "style 0 0 3 fg=#7f8ea3\n",
                "style 0 3 6 fg=#7f8ea3 bold\n",
                "style 0 6 8 fg=#7f8ea3\n",
                "text 1 \"日本 oké\"\n",
                "style 1 0 7 bg=red\n",
                "style 1 7 8 fg=idx:208\n",
                "text 2 \"\"\n",
                "style 2 4 8 bg=dark-gray\n",
                "text 3 \"x\"\n",
                "style 3 0 1 fg=light-cyan bg=gray ul=white underline bold dim italic blink rapid-blink reverse hidden strike\n",
            )
        );
        assert!(styled(&buffer, None).starts_with("size 14 4\ncursor 0 0 hidden\ntext 0 "));
    }
}

//! Word wrapping of styled text by display width. The view wraps each item
//! itself, once per width, instead of leaving it to ratatui's paragraphs:
//! their wrapping and line counts are marked unstable
//! (ratatui-widgets/src/paragraph.rs:308-331), and the transcript needs
//! every item's height to scroll without drawing it.

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

/// The display width of `c`: 0 for marks that join the character before.
pub fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// The display width of `text`.
pub fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// `text` safe to draw: tabs as spaces, and no control characters (an
/// escape sequence in a command's output would move the cursor).
pub fn clean(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\t' => clean.push_str("    "),
            '\n' => clean.push('\n'),
            // `ESC [ ... letter`: a terminal style.
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() || c == '~' {
                            break;
                        }
                    }
                }
            }
            c if c.is_control() => {}
            c => clean.push(c),
        }
    }
    clean
}

/// How a display line follows the one before it in the text it shows:
/// what a copy puts between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    /// A line of its own: the text has a line break there.
    Break,
    /// The text goes on, wrapped at a space.
    Space,
    /// The text goes on, a word too long for a line broken.
    Direct,
}

/// A display line, and what a copy of it needs: how it follows the line
/// before, and how many columns at its start (a margin, a gutter, a
/// hanging indent) and its end (a tag) aren't its text.
#[derive(Debug, Clone, PartialEq)]
pub struct Wrapped {
    pub spans: Vec<Span<'static>>,
    pub join: Join,
    pub skip: usize,
    pub tail: usize,
    /// It has no text at all (a box's border): a copy leaves it out.
    pub bare: bool,
    /// What a copy of the whole line takes, where the screen shows it
    /// shortened (a long path).
    pub whole: Option<String>,
}

impl Wrapped {
    /// A line of its own, all text.
    pub fn line(spans: Vec<Span<'static>>) -> Self {
        Self {
            spans,
            join: Join::Break,
            skip: 0,
            tail: 0,
            bare: false,
            whole: None,
        }
    }

    /// This line with `span` before it, as decoration (not copied).
    pub fn decorated(mut self, span: Span<'static>) -> Self {
        self.skip += width(&span.content);
        self.spans.insert(0, span);
        self
    }
}

/// One logical line, as styled spans, wrapped to `width` columns: at
/// spaces where it can be, else in the middle of a word too long for a
/// line. Spaces at the break are dropped. `hang` is the indent of the
/// lines after the first (a list item's text lines up under its text).
pub fn wrap(spans: &[Span<'_>], width: usize, hang: usize) -> Vec<Vec<Span<'static>>> {
    wrap_lines(spans, width, hang)
        .into_iter()
        .map(|line| line.spans)
        .collect()
}

/// [`wrap`], with how each line joins the one before it, for copying.
pub fn wrap_lines(spans: &[Span<'_>], width: usize, hang: usize) -> Vec<Wrapped> {
    let width = width.max(1);
    let chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |c| (c, span.style)))
        .collect();
    // Each line, and how it follows the one before.
    let mut lines: Vec<(Vec<(char, Style)>, Join)> = Vec::new();
    let mut line: Vec<(char, Style)> = Vec::new();
    let mut join = Join::Break;
    let mut used = 0;
    // Where the line can break: after its last space, with the width
    // before it.
    let mut last_space: Option<(usize, usize)> = None;
    let room = |lines: &Vec<(Vec<(char, Style)>, Join)>| {
        if lines.is_empty() {
            width
        } else {
            width.saturating_sub(hang).max(1)
        }
    };
    for (c, style) in chars {
        let w = char_width(c);
        if used + w > room(&lines) && !line.is_empty() {
            let (rest, next) = match last_space {
                // Break after the space; the next line starts with the
                // word that didn't fit.
                Some((at, _)) if c != ' ' => (line.split_off(at), Join::Space),
                _ if c == ' ' => (Vec::new(), Join::Space),
                _ => (Vec::new(), Join::Direct),
            };
            // Spaces at the end of a line aren't drawn.
            while line.last().is_some_and(|(c, _)| *c == ' ') {
                line.pop();
            }
            lines.push((std::mem::take(&mut line), join));
            join = next;
            line = rest;
            used = line.iter().map(|(c, _)| char_width(*c)).sum();
            last_space = None;
            if c == ' ' && line.is_empty() {
                continue;
            }
        }
        line.push((c, style));
        used += w;
        if c == ' ' {
            last_space = Some((line.len(), used));
        }
    }
    lines.push((line, join));
    lines
        .into_iter()
        .enumerate()
        .map(|(n, (chars, join))| {
            let line = Wrapped {
                spans: merge(&chars),
                join,
                skip: 0,
                tail: 0,
                bare: false,
                whole: None,
            };
            if n > 0 && hang > 0 {
                line.decorated(Span::raw(" ".repeat(hang)))
            } else {
                line
            }
        })
        .collect()
}

/// Runs of characters of one style, as spans.
fn merge(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut style: Option<Style> = None;
    for (c, s) in chars {
        if style != Some(*s) {
            if let Some(style) = style {
                spans.push(Span::styled(std::mem::take(&mut text), style));
            }
            style = Some(*s);
        }
        text.push(*c);
    }
    if let Some(style) = style {
        spans.push(Span::styled(text, style));
    }
    spans
}

/// `prefix` before the first of `lines`, and `rest` (as wide) before the
/// others: a margin, such as the user's bar or a tool line's indent.
pub fn prefixed(lines: Vec<Wrapped>, prefix: Span<'static>, rest: Span<'static>) -> Vec<Wrapped> {
    lines
        .into_iter()
        .enumerate()
        .map(|(n, line)| line.decorated(if n == 0 { prefix.clone() } else { rest.clone() }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Vec<Span>]) -> Vec<String> {
        lines
            .iter()
            .map(|spans| spans.iter().map(|span| span.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn lines_break_at_spaces_and_long_words_anywhere() {
        let line = [Span::raw("the quick brown fox jumps")];
        assert_eq!(
            text(&wrap(&line, 10, 0)),
            ["the quick", "brown fox", "jumps"]
        );
        assert_eq!(
            text(&wrap(&[Span::raw("abcdefghij")], 4, 0)),
            ["abcd", "efgh", "ij"]
        );
        assert_eq!(text(&wrap(&[Span::raw("")], 4, 0)), [""]);
        // The lines after the first hang under the text.
        assert_eq!(
            text(&wrap(&[Span::raw("one two three")], 8, 2)),
            ["one two", "  three"]
        );
    }

    #[test]
    fn styles_survive_a_break_and_wide_characters_count_twice() {
        let bold = Style::new().add_modifier(ratatui::style::Modifier::BOLD);
        let line = [Span::raw("see "), Span::styled("this word", bold)];
        let lines = wrap(&line, 8, 0);
        assert_eq!(text(&lines), ["see this", "word"]);
        assert_eq!(lines[1][0].style, bold);
        assert_eq!(
            text(&wrap(&[Span::raw("日本語です")], 4, 0)),
            ["日本", "語で", "す"]
        );
    }

    /// A line wrapped at a space joins back with one; a word broken
    /// joins back directly; a hanging indent isn't text.
    #[test]
    fn wrapped_lines_say_how_they_join() {
        let lines = wrap_lines(&[Span::raw("the quick brownfoxjumps")], 10, 2);
        let shown: Vec<(Join, usize)> = lines.iter().map(|line| (line.join, line.skip)).collect();
        assert_eq!(
            shown,
            [(Join::Break, 0), (Join::Space, 2), (Join::Direct, 2)]
        );
    }

    #[test]
    fn control_characters_are_left_out() {
        assert_eq!(clean("a\tb\x1b[31mred\x1b[0m\x07\n"), "a    bred\n");
    }
}

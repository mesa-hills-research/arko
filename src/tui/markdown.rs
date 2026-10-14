//! The agents' text as markdown, drawn with the terminal's styles: bold,
//! italics, code spans and blocks, headings, lists, quotes and rules. What
//! isn't markdown (a table, an unclosed marker in text that is still
//! streaming) is drawn as it is.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::theme::Theme;
use super::wrap::{self, Wrapped, width as text_width};
use crate::glyphs;

/// `text` as markdown, in lines wrapped to `width` columns: with what a
/// copy of each needs (a gutter isn't text, a wrapped line goes on).
pub fn render(text: &str, base: Style, theme: &Theme, width: usize) -> Vec<Wrapped> {
    let text = wrap::clean(text);
    let mut lines: Vec<Wrapped> = Vec::new();
    let mut fence: Option<String> = None;
    let mut blank = false;
    let gutter = glyphs::render("│ ").into_owned();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let marker = trimmed
            .strip_prefix("```")
            .map(|_| "```")
            .or_else(|| trimmed.strip_prefix("~~~").map(|_| "~~~"));
        if let Some(open) = &fence {
            if marker == Some(open.as_str()) && trimmed.trim_end().len() == 3 {
                fence = None;
            } else {
                // Code is drawn as it is, wrapped anywhere, beside a
                // gutter that marks it.
                let spans = [Span::styled(line.to_string(), theme.text())];
                for wrapped in wrap::wrap_lines(&spans, width.saturating_sub(2), 0) {
                    lines.push(wrapped.decorated(Span::styled(gutter.clone(), theme.dim())));
                }
            }
            blank = false;
            continue;
        }
        if let Some(marker) = marker {
            fence = Some(marker.to_string());
            continue;
        }
        if trimmed.is_empty() {
            // One blank line between blocks, however many there were.
            if !blank && !lines.is_empty() {
                lines.push(Wrapped::line(Vec::new()));
            }
            blank = true;
            continue;
        }
        blank = false;
        if let Some(title) = heading(trimmed) {
            let style = base.add_modifier(Modifier::BOLD);
            lines.extend(wrap::wrap_lines(&inline(title, style, theme), width, 0));
            continue;
        }
        if is_rule(trimmed) {
            let rule = glyphs::render("─").repeat(width.min(40));
            lines.push(Wrapped::line(vec![Span::styled(rule, theme.dim())]));
            continue;
        }
        if let Some(quoted) = trimmed.strip_prefix('>') {
            let quoted = quoted.strip_prefix(' ').unwrap_or(quoted);
            let spans = inline(quoted, theme.dim(), theme);
            for wrapped in wrap::wrap_lines(&spans, width.saturating_sub(2), 0) {
                lines.push(wrapped.decorated(Span::styled(gutter.clone(), theme.dim())));
            }
            continue;
        }
        if let Some((indent, bullet, item)) = list_item(line) {
            let bullet = if bullet.ends_with(['.', ')']) {
                bullet.to_string()
            } else {
                glyphs::render("•").into_owned()
            };
            let lead = format!("{}{bullet} ", " ".repeat(indent));
            let hang = text_width(&lead);
            let mut spans = vec![Span::styled(lead, base)];
            spans.extend(inline(item, base, theme));
            lines.extend(wrap::wrap_lines(&spans, width, hang));
            continue;
        }
        if trimmed.starts_with('|') {
            // A table keeps its columns: as it is.
            lines.extend(wrap::wrap_lines(
                &[Span::styled(line.to_string(), base)],
                width,
                0,
            ));
            continue;
        }
        lines.extend(wrap::wrap_lines(&inline(trimmed, base, theme), width, 0));
    }
    while lines.last().is_some_and(|line| line.spans.is_empty()) {
        lines.pop();
    }
    lines
}

/// A heading's text.
fn heading(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) {
        line[hashes..].strip_prefix(' ').map(str::trim)
    } else {
        None
    }
}

/// `---`, `***` or `___` (spaces between allowed).
fn is_rule(line: &str) -> bool {
    let marks: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    marks.len() >= 3
        && ["-", "*", "_"]
            .iter()
            .any(|mark| marks.chars().all(|c| c.to_string() == *mark))
}

/// A list item: its indent, its bullet (`-`, `*`, `+`, `1.`, `1)`) and
/// its text.
fn list_item(line: &str) -> Option<(usize, &str, &str)> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    let bullet_end = if rest.starts_with(['-', '*', '+']) {
        1
    } else {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || digits > 3 || !rest[digits..].starts_with(['.', ')']) {
            return None;
        }
        digits + 1
    };
    let text = rest[bullet_end..].strip_prefix(' ')?;
    Some((indent, &rest[..bullet_end], text.trim_start()))
}

/// A line's inline markdown as spans: `**bold**`, `*italics*`, `` `code` ``
/// and `[links](url)`.
fn inline(text: &str, base: Style, theme: &Theme) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut bold = false;
    let mut italic = false;
    let style = |bold: bool, italic: bool| {
        let mut style = base;
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        style
    };
    let flush = |plain: &mut String, spans: &mut Vec<Span<'static>>, s: Style| {
        if !plain.is_empty() {
            spans.push(Span::styled(std::mem::take(plain), s));
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let before = i.checked_sub(1).map(|at| chars[at]);
        match c {
            '\\' if next.is_some_and(|n| n.is_ascii_punctuation()) => {
                plain.push(next.unwrap_or_default());
                i += 2;
                continue;
            }
            '`' => {
                let ticks = chars[i..].iter().take_while(|c| **c == '`').count();
                let close = (i + ticks..chars.len()).find(|&at| {
                    chars[at..].iter().take_while(|c| **c == '`').count() == ticks
                        && chars.get(at.wrapping_sub(1)) != Some(&'`')
                });
                if let Some(close) = close {
                    flush(&mut plain, &mut spans, style(bold, italic));
                    let code: String = chars[i + ticks..close].iter().collect();
                    let code = code.strip_prefix(' ').unwrap_or(&code);
                    let code = code.strip_suffix(' ').unwrap_or(code);
                    spans.push(Span::styled(code.to_string(), theme.code()));
                    i = close + ticks;
                    continue;
                }
            }
            '*' | '_' if next == Some(c) => {
                // Bold opens before a word and closes after one.
                let opens = chars.get(i + 2).is_some_and(|n| !n.is_whitespace());
                let closes = before.is_some_and(|b| !b.is_whitespace());
                if (bold && closes) || (!bold && opens && has_closing(&chars[i + 2..], c, 2)) {
                    flush(&mut plain, &mut spans, style(bold, italic));
                    bold = !bold;
                    i += 2;
                    continue;
                }
            }
            '*' | '_' => {
                let intraword = c == '_'
                    && before.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric);
                let opens = next.is_some_and(|n| !n.is_whitespace());
                let closes = before.is_some_and(|b| !b.is_whitespace());
                if !intraword
                    && ((italic && closes)
                        || (!italic && opens && has_closing(&chars[i + 1..], c, 1)))
                {
                    flush(&mut plain, &mut spans, style(bold, italic));
                    italic = !italic;
                    i += 1;
                    continue;
                }
            }
            '[' => {
                if let Some((label, url, end)) = link(&chars, i) {
                    flush(&mut plain, &mut spans, style(bold, italic));
                    spans.push(Span::styled(
                        label.clone(),
                        style(bold, italic).add_modifier(Modifier::UNDERLINED),
                    ));
                    if url != label {
                        spans.push(Span::styled(format!(" ({url})"), theme.dim()));
                    }
                    i = end;
                    continue;
                }
            }
            _ => {}
        }
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, &mut spans, style(bold, italic));
    spans
}

/// Whether `rest` closes a marker of `count` `mark`s (after a word).
fn has_closing(rest: &[char], mark: char, count: usize) -> bool {
    (1..rest.len()).any(|at| {
        !rest[at - 1].is_whitespace()
            && rest[at..].iter().take_while(|c| **c == mark).count() >= count
    })
}

/// A link `[label](url)` at `start`: its label, its URL and where it ends.
fn link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close = (start + 1..chars.len()).find(|&at| chars[at] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|&at| chars[at] == ')')?;
    let label: String = chars[start + 1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    if label.is_empty() || url.contains(' ') {
        return None;
    }
    Some((label, url, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str, width: usize) -> Vec<String> {
        let theme = Theme::terminal();
        render(text, theme.text(), &theme, width)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn styled(text: &str) -> Vec<(String, Style)> {
        let theme = Theme::terminal();
        inline(text, Style::new(), &theme)
            .into_iter()
            .map(|span| (span.content.into_owned(), span.style))
            .collect()
    }

    #[test]
    fn blocks_read_as_markdown() {
        let text = "# Done\n\nThe parser:\n\n- reads **bold**\n- and `code`\n\n```rust\nfn main() {}\n```\n> quoted\n\n---";
        assert_eq!(
            lines(text, 40),
            [
                "Done",
                "",
                "The parser:",
                "",
                "• reads bold",
                "• and code",
                "",
                "│ fn main() {}",
                "│ quoted",
                "",
                "────────────────────────────────────────"
            ]
        );
        // A list item's text lines up under its first line.
        assert_eq!(
            lines("1. one two three", 9),
            ["1. one", "   two", "   three"]
        );
    }

    #[test]
    fn inline_markers_style_and_stray_ones_stay() {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let italic = Style::new().add_modifier(Modifier::ITALIC);
        assert_eq!(
            styled("a **b** c"),
            [
                ("a ".into(), Style::new()),
                ("b".into(), bold),
                (" c".into(), Style::new())
            ]
        );
        assert_eq!(styled("*it*")[0], ("it".to_string(), italic));
        // Arithmetic, snake_case and unclosed markers are text.
        assert_eq!(styled("2 * 3 * 4"), [("2 * 3 * 4".into(), Style::new())]);
        assert_eq!(styled("a_b_c"), [("a_b_c".into(), Style::new())]);
        assert_eq!(styled("**open"), [("**open".into(), Style::new())]);
        let link = styled("see [docs](https://x.dev)");
        assert_eq!(link[1].0, "docs");
        assert_eq!(link[2].0, " (https://x.dev)");
    }
}

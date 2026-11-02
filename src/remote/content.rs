//! A call's content, which opening its row shows: the lines it read, its
//! diff, its output, its message, its result. The server makes it from the
//! call and its result (`render`), as a `result` item whose text is the
//! content's first lines, and every client draws those lines as this module
//! reads them, so the terminal, the page and the apps agree.
//!
//! A content of files' lines keeps each line's number before a tab, and a
//! diff's line its mark before the number. A line without them (a note
//! between a diff's parts) is plain.
//!
//! The web page compiles this file beside `protocol.rs` as a module of its
//! own, so it depends on the standard library and that module alone.

use super::protocol::ContentKind;

/// The lines a content shows before its door ("… 97 more lines"), which
/// opens the rest.
pub const PREVIEW_LINES: usize = 16;

/// The most characters a line of a preview shows.
pub const PREVIEW_LINE_CHARS: usize = 400;

/// A line of a content, as a client draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line<'a> {
    /// A file's line, by its number (empty when the tool didn't say it),
    /// with its mark in a diff.
    Numbered {
        number: &'a str,
        mark: Mark,
        text: &'a str,
    },
    /// Any other line, such as a command's output.
    Plain(&'a str),
}

/// How a diff marks a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// A file's line outside a diff.
    None,
    Kept,
    Removed,
    Added,
}

impl Mark {
    /// The mark as a diff's line shows it.
    pub fn symbol(self) -> char {
        match self {
            Mark::Removed => '-',
            Mark::Added => '+',
            Mark::None | Mark::Kept => ' ',
        }
    }
}

/// The line `line` of a content of `kind`, as a client draws it.
pub fn line(kind: ContentKind, line: &str) -> Line<'_> {
    fn numbered(rest: &str, mark: Mark) -> Option<Line<'_>> {
        let (number, text) = rest.split_once('\t')?;
        number
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then_some(Line::Numbered { number, mark, text })
    }
    let parsed = match kind {
        ContentKind::Lines => numbered(line, Mark::None),
        ContentKind::Diff => {
            let mark = match line.as_bytes().first() {
                Some(b' ') => Some(Mark::Kept),
                Some(b'-') => Some(Mark::Removed),
                Some(b'+') => Some(Mark::Added),
                _ => None,
            };
            mark.and_then(|mark| numbered(&line[1..], mark))
        }
        _ => None,
    };
    parsed.unwrap_or(Line::Plain(line))
}

/// A file's line `number`, as a content of [`ContentKind::Lines`] holds it.
pub fn numbered(number: usize, text: &str) -> String {
    format!("{number}\t{text}")
}

/// A diff's line, as a content of [`ContentKind::Diff`] holds it.
pub fn diff_line(mark: Mark, number: Option<usize>, text: &str) -> String {
    let number = number.map(|number| number.to_string()).unwrap_or_default();
    format!("{}{number}\t{text}", mark.symbol())
}

/// The widest number of `text`'s lines, which their numbers are aligned to.
pub fn number_width(kind: ContentKind, text: &str) -> usize {
    text.lines()
        .map(|each| match line(kind, each) {
            Line::Numbered { number, .. } => number.len(),
            Line::Plain(_) => 0,
        })
        .max()
        .unwrap_or(0)
}

/// The code of a content's numbered lines, one a line, which its
/// highlighting is for. The page finds the spans by this code's hash, and
/// they count bytes in it. `None` for a content without numbered lines.
pub fn code(kind: ContentKind, text: &str) -> Option<String> {
    if !matches!(kind, ContentKind::Lines | ContentKind::Diff) {
        return None;
    }
    let lines: Vec<&str> = text
        .lines()
        .filter_map(|each| match line(kind, each) {
            Line::Numbered { text, .. } => Some(text),
            Line::Plain(_) => None,
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// `count` with its thousands apart ("1,240").
pub fn grouped(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// The door of a preview that leaves out `lines` lines, or when it leaves
/// out none, `characters` characters of lines it cut.
pub fn door(lines: usize, characters: usize) -> Option<String> {
    let s = |count: usize| if count == 1 { "" } else { "s" };
    if lines > 0 {
        Some(format!("… {} more line{}", grouped(lines), s(lines)))
    } else if characters > 0 {
        Some(format!(
            "… {} more character{}",
            grouped(characters),
            s(characters)
        ))
    } else {
        None
    }
}

/// The preview of the content `text`, which is its first
/// [`PREVIEW_LINES`] lines, each at most [`PREVIEW_LINE_CHARS`]
/// characters, and the door to the rest when it leaves any out.
pub fn preview(text: &str) -> (String, Option<String>) {
    let lines: Vec<&str> = text.lines().collect();
    let mut cut = 0;
    let shown: Vec<String> = lines
        .iter()
        .take(PREVIEW_LINES)
        .map(|line| match line.char_indices().nth(PREVIEW_LINE_CHARS) {
            Some((end, _)) => {
                cut += line[end..].chars().count();
                format!("{}…", &line[..end])
            }
            None => line.to_string(),
        })
        .collect();
    let more = lines.len().saturating_sub(PREVIEW_LINES);
    (shown.join("\n"), door(more, cut))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_contents_lines_read_back_as_written() {
        assert_eq!(
            line(ContentKind::Lines, &numbered(96, "fn branched() {")),
            Line::Numbered {
                number: "96",
                mark: Mark::None,
                text: "fn branched() {"
            }
        );
        assert_eq!(
            line(
                ContentKind::Diff,
                &diff_line(Mark::Removed, Some(102), "\tx: None,")
            ),
            Line::Numbered {
                number: "102",
                mark: Mark::Removed,
                text: "\tx: None,"
            }
        );
        assert_eq!(
            line(ContentKind::Diff, &diff_line(Mark::Added, None, "y")),
            Line::Numbered {
                number: "",
                mark: Mark::Added,
                text: "y"
            }
        );
        // (A note between a diff's parts, and output that has a tab.)
        assert_eq!(
            line(ContentKind::Diff, "[Lines 40-60 not shown.]"),
            Line::Plain("[Lines 40-60 not shown.]")
        );
        assert_eq!(line(ContentKind::Plain, "1\tx"), Line::Plain("1\tx"));
        assert_eq!(line(ContentKind::Lines, "a\tb"), Line::Plain("a\tb"));
    }

    #[test]
    fn a_preview_shows_sixteen_lines_and_says_how_many_more() {
        let text: Vec<String> = (1..=113).map(|n| format!("line {n}")).collect();
        let (shown, more) = preview(&text.join("\n"));
        assert_eq!(shown.lines().count(), PREVIEW_LINES);
        assert_eq!(more.as_deref(), Some("… 97 more lines"));
        assert_eq!(preview("a\nb"), ("a\nb".to_string(), None));
        let (_, more) = preview(&"x".repeat(PREVIEW_LINE_CHARS + 1240));
        assert_eq!(more.as_deref(), Some("… 1,240 more characters"));
        assert_eq!(door(1, 0).as_deref(), Some("… 1 more line"));
    }

    #[test]
    fn the_code_of_a_diff_is_its_lines_without_marks_or_numbers() {
        let text = [
            diff_line(Mark::Kept, Some(1), "a"),
            "[Lines 2-3 not shown.]".to_string(),
            diff_line(Mark::Removed, Some(4), "b"),
            diff_line(Mark::Added, Some(4), "c"),
        ]
        .join("\n");
        assert_eq!(code(ContentKind::Diff, &text).as_deref(), Some("a\nb\nc"));
        assert_eq!(number_width(ContentKind::Diff, &text), 1);
        assert_eq!(code(ContentKind::Plain, &text), None);
    }
}

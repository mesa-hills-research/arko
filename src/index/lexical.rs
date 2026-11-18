//! Lexical search as an exact search of names and words. arko_index finds the
//! chunks that hold any word of the query (its candidates, by the words it
//! splits text into, identifier parts included), and a match is a candidate
//! whose text, as the file is now, holds the query as written. A result shows
//! the lines where the query starts.

use std::borrow::Cow;

use arko_index::bench_v1::Chunk;

/// Characters of a matched line as a result shows it.
pub(crate) const LINE_WIDTH: usize = 140;
/// Matched lines a result shows of one chunk. It counts the others.
pub(crate) const LINES: usize = 10;

/// The lines of a chunk that hold the query.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lines {
    /// Each line's number and text, trimmed and cut at [`LINE_WIDTH`], at
    /// most [`LINES`] of them.
    pub shown: Vec<(usize, String)>,
    /// The lines that hold it past those shown.
    pub more: usize,
}

/// A query as lexical search matches it.
#[derive(Clone, Debug)]
pub(crate) struct Literal {
    /// As written, trimmed, with its ASCII letters lower-cased when the case
    /// doesn't matter.
    text: String,
    ignore_case: bool,
}

impl Literal {
    pub fn new(query: &str, ignore_case: bool) -> Self {
        let text = query.trim();
        Self {
            text: if ignore_case {
                text.to_ascii_lowercase()
            } else {
                text.to_string()
            },
            ignore_case,
        }
    }

    /// The chunks of `text` (a file, as it is now) that hold the query, each
    /// with the lines where it starts there. `chunks` are the file's, in
    /// order and apart. A query that starts in one chunk and goes on into the
    /// next is the first one's.
    pub fn matches<'c>(&self, text: &str, chunks: &'c [Chunk]) -> Vec<(&'c Chunk, Lines)> {
        if self.text.is_empty() {
            return Vec::new();
        }
        // ASCII case folding keeps every byte where it was.
        let folded: Cow<'_, str> = if self.ignore_case {
            Cow::Owned(text.to_ascii_lowercase())
        } else {
            Cow::Borrowed(text)
        };
        let starts: Vec<usize> = folded
            .match_indices(self.text.as_str())
            .map(|(at, _)| at)
            .collect();
        if starts.is_empty() {
            return Vec::new();
        }
        let lines = lines_of(text, &starts);
        let mut found = Vec::new();
        for chunk in chunks {
            let first = starts.partition_point(|&at| at < chunk.start_byte);
            let last = starts.partition_point(|&at| at < chunk.end_byte);
            if first == last {
                continue;
            }
            let mut held = Lines::default();
            let mut previous = None;
            for (&at, &line) in starts[first..last].iter().zip(&lines.of[first..last]) {
                if previous == Some(line) {
                    continue;
                }
                previous = Some(line);
                if held.shown.len() < LINES {
                    let (number, begin, end) = lines.bounds[line];
                    let shown = shown_line(&text[begin..end], at - begin, self.text.len());
                    held.shown.push((number, shown));
                } else {
                    held.more += 1;
                }
            }
            found.push((chunk, held));
        }
        found
    }
}

/// Characters a line cut around its match keeps before the match.
const LEAD: usize = 20;

/// The lines matches start on.
struct LinesOf {
    /// Each line's number (1-based) and bytes, once.
    bounds: Vec<(usize, usize, usize)>,
    /// For each match, its line in `bounds`.
    of: Vec<usize>,
}

/// The lines each of `starts` (ascending byte offsets into `text`) lies on,
/// each line found once however many matches it holds.
fn lines_of(text: &str, starts: &[usize]) -> LinesOf {
    let bytes = text.as_bytes();
    let mut lines = LinesOf {
        bounds: Vec::new(),
        of: Vec::with_capacity(starts.len()),
    };
    // The lines counted up to `counted`.
    let (mut number, mut counted) = (1, 0);
    for &at in starts {
        if let Some(&(_, _, end)) = lines.bounds.last()
            && at < end
        {
            lines.of.push(lines.bounds.len() - 1);
            continue;
        }
        number += bytes[counted..at]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count();
        counted = at;
        let begin = text[..at].rfind('\n').map_or(0, |newline| newline + 1);
        let end = text[at..]
            .find('\n')
            .map_or(text.len(), |newline| at + newline);
        lines.bounds.push((number, begin, end));
        lines.of.push(lines.bounds.len() - 1);
    }
    lines
}

/// `line` trimmed and cut at [`LINE_WIDTH`] around the match at byte `at`
/// (`len` bytes).
fn shown_line(line: &str, at: usize, len: usize) -> String {
    // (The query is trimmed, so the match lies in the trimmed line.)
    let lead = line.len() - line.trim_start().len();
    let within = at.saturating_sub(lead);
    let line = line.trim();
    let len = len.min(line.len().saturating_sub(within));
    cut_around(line, within, len)
}

/// `line` cut to [`LINE_WIDTH`] characters so that the match at byte `at`
/// (`len` bytes) shows, whole wherever it fits, with `…` where it was cut.
/// It reads only the characters near the match and the line's ends, so a
/// long line split into many chunks costs no more for each than a short one.
fn cut_around(line: &str, at: usize, len: usize) -> String {
    // Characters of `text`, counted up to `most`.
    let count = |text: &str, most: usize| text.chars().take(most).count();
    if count(line, LINE_WIDTH + 1) <= LINE_WIDTH {
        return line.to_string();
    }
    let span = line[at..at + len].chars().count();
    // A window between two ellipses shows this many characters.
    let room = LINE_WIDTH - 2;
    // From the line's start when the match fits there.
    if count(&line[..at], LINE_WIDTH) + span < LINE_WIDTH {
        return format!("{}…", first(line, LINE_WIDTH - 1));
    }
    // Up to LEAD characters before the match, fewer when the match needs the
    // room, and never so late that the window runs past the line's end.
    let lead = LEAD.min(room.saturating_sub(span));
    let from = back(line, at, lead);
    if count(&line[from..], LINE_WIDTH) < LINE_WIDTH {
        return format!("…{}", &line[back(line, line.len(), LINE_WIDTH - 1)..]);
    }
    if from == 0 {
        return format!("{}…", first(line, LINE_WIDTH - 1));
    }
    format!("…{}…", first(&line[from..], room))
}

/// The first `most` characters of `text`.
fn first(text: &str, most: usize) -> &str {
    &text[..text
        .char_indices()
        .nth(most)
        .map_or(text.len(), |(at, _)| at)]
}

/// The byte `chars` characters before byte `at` of `text` (or its start).
fn back(text: &str, at: usize, chars: usize) -> usize {
    text[..at]
        .char_indices()
        .rev()
        .take(chars)
        .last()
        .map_or(at, |(byte, _)| byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(text: &str, start: usize, end: usize) -> Chunk {
        let line = |at: usize| text[..at].matches('\n').count() + 1;
        Chunk {
            id: String::new(),
            path: "src/x.rs".into(),
            start_byte: start,
            end_byte: end,
            start_line: line(start),
            end_line: line(end.saturating_sub(1).max(start)),
            language: "rust".into(),
            symbol_path: String::new(),
            kind: String::new(),
            token_estimate: 0,
            scope: String::new(),
        }
    }

    fn shown(found: &[(&Chunk, Lines)]) -> Vec<Vec<usize>> {
        found
            .iter()
            .map(|(_, lines)| lines.shown.iter().map(|(number, _)| *number).collect())
            .collect()
    }

    #[test]
    fn the_query_is_matched_as_written() {
        let text = "fn a() {\n    search_blocking(x);\n    search(blocking);\n}\nfn b() {\n    Search_Blocking();\n}\n";
        let split = text.find("fn b").unwrap();
        let chunks = [chunk(text, 0, split), chunk(text, split, text.len())];
        let literal = Literal::new(" search_blocking ", false);
        let found = literal.matches(text, &chunks);
        assert_eq!(shown(&found), [vec![2]]);
        assert_eq!(found[0].1.shown[0].1, "search_blocking(x);");
        assert_eq!(found[0].0.start_byte, 0);
        // Any case of the ASCII letters, with ignore_case.
        let found = Literal::new("search_blocking", true).matches(text, &chunks);
        assert_eq!(shown(&found), [vec![2], vec![6]]);
        // A phrase with its spaces and punctuation.
        let found = Literal::new("search(blocking)", false).matches(text, &chunks);
        assert_eq!(shown(&found), [vec![3]]);
        assert!(Literal::new("   ", false).matches(text, &chunks).is_empty());
        assert!(
            Literal::new("nowhere", false)
                .matches(text, &chunks)
                .is_empty()
        );
    }

    #[test]
    fn each_line_shows_once_and_the_rest_are_counted() {
        let mut text = String::from("fn many() {\n");
        for _ in 0..(LINES + 3) {
            text.push_str("    go(); go();\n");
        }
        text.push_str("}\n");
        let chunks = [chunk(&text, 0, text.len())];
        let found = Literal::new("go()", false).matches(&text, &chunks);
        let lines = &found[0].1;
        assert_eq!(lines.shown.len(), LINES);
        assert_eq!(lines.shown[0], (2, "go(); go();".to_string()));
        assert_eq!(lines.more, 3);
    }

    #[test]
    fn a_line_with_many_matches_shows_once() {
        let line = "fetch_widget(); ".repeat(5_000);
        let text =
            format!("fn a() {{\n    {line}\n}}\nfn b() {{ fetch_widget(); fetch_widget(); }}\n");
        let split = text.find("fn b").unwrap();
        let chunks = [chunk(&text, 0, split), chunk(&text, split, text.len())];
        let found = Literal::new("fetch_widget()", false).matches(&text, &chunks);
        assert_eq!(shown(&found), [vec![2], vec![4]]);
        assert_eq!(found[0].1.more, 0);
        let shown_line = &found[0].1.shown[0].1;
        assert!(shown_line.starts_with("fetch_widget(); fetch_widget();"));
        assert_eq!(shown_line.chars().count(), LINE_WIDTH);
    }

    /// A long line that the chunker splits into many chunks: each chunk's
    /// result shows the window around its own first match.
    #[test]
    fn a_long_line_split_into_chunks_shows_each_chunks_match() {
        let text = format!("let calls = [{}];\n", "fetch_widget(1), ".repeat(16_000));
        let chunks = arko_index::bench_v1::chunk_str(
            std::path::Path::new("src/x.rs"),
            &text,
            &arko_index::bench_v1::Options::default(),
        )
        .unwrap();
        assert!(chunks.len() > 20, "{}", chunks.len());
        let found = Literal::new("fetch_widget(1)", false).matches(&text, &chunks);
        let holding = chunks
            .iter()
            .filter(|chunk| text[chunk.start_byte..chunk.end_byte].contains("fetch_widget(1)"))
            .count();
        assert!(
            found.len() >= holding && holding > 20,
            "{} {holding}",
            found.len()
        );
        for (chunk, lines) in &found {
            assert_eq!(lines.shown.len(), 1, "{chunk:?}");
            assert_eq!(lines.more, 0);
            let (number, line) = &lines.shown[0];
            assert_eq!(*number, 1);
            assert!(line.contains("fetch_widget(1)"), "{line}");
            assert_eq!(line.chars().count(), LINE_WIDTH, "{line}");
        }
    }

    #[test]
    fn a_match_belongs_to_the_chunk_it_starts_in() {
        // Two chunks on one line, and a query across their boundary.
        let text = "let a = 1; let b = 2;\n";
        let split = text.find("let b").unwrap();
        let chunks = [chunk(text, 0, split), chunk(text, split, text.len())];
        let found = Literal::new("1; let b", false).matches(text, &chunks);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.start_byte, 0);
        // The whole line shows, also for the second chunk.
        let found = Literal::new("b = 2", false).matches(text, &chunks);
        assert_eq!(found[0].0.start_byte, split);
        assert_eq!(found[0].1.shown[0].1, "let a = 1; let b = 2;");
    }

    #[test]
    fn long_lines_are_cut_around_the_match_on_characters() {
        let width = |line: &str| line.chars().count();
        // The match in the middle, near the start and at the end.
        let text = format!(
            "// {}needle {}\n    needle {}\n{} needle\n",
            "é".repeat(200),
            "ü".repeat(200),
            "a".repeat(300),
            "b".repeat(300)
        );
        let chunks = [chunk(&text, 0, text.len())];
        let found = Literal::new("NEEDLE", true).matches(&text, &chunks);
        let lines = &found[0].1.shown;
        assert_eq!(lines.len(), 3);
        let (middle, start, end) = (&lines[0].1, &lines[1].1, &lines[2].1);
        assert!(middle.starts_with('…') && middle.ends_with('…'), "{middle}");
        assert!(
            middle.contains(&format!("{}needle ü", "é".repeat(LEAD))),
            "{middle}"
        );
        assert!(
            start.starts_with("needle a") && start.ends_with('…'),
            "{start}"
        );
        assert!(end.starts_with('…') && end.ends_with("b needle"), "{end}");
        for line in [middle, start, end] {
            assert_eq!(width(line), LINE_WIDTH, "{line}");
        }
        // A query that just fits shows whole, with less before it.
        for span in [LINE_WIDTH - 10, LINE_WIDTH - 2] {
            let query = "q".repeat(span);
            let text = format!("{} {query} {}\n", "y".repeat(200), "z".repeat(200));
            let chunks = [chunk(&text, 0, text.len())];
            let found = Literal::new(&query, false).matches(&text, &chunks);
            let line = &found[0].1.shown[0].1;
            assert!(line.contains(&query), "{span}: {line}");
            assert!(line.starts_with('…') && line.ends_with('…'), "{line}");
            assert_eq!(width(line), LINE_WIDTH, "{line}");
        }
        // A query longer than the width shows its start.
        let long = "x".repeat(LINE_WIDTH * 2);
        let text = format!("{} {long}\n", "y".repeat(LINE_WIDTH));
        let chunks = [chunk(&text, 0, text.len())];
        let found = Literal::new(&long, false).matches(&text, &chunks);
        let line = &found[0].1.shown[0].1;
        assert_eq!(*line, format!("…{}…", "x".repeat(LINE_WIDTH - 2)));
    }
}

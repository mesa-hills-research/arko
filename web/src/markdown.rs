//! Replies in Markdown, as HTML. Raw HTML in the text shows as text, and
//! links open only to web and mail addresses.
//!
//! A reply that streams is split into blocks. Once a block is finished (a
//! closed code fence, or a paragraph, list, quote or table followed by a
//! blank line and the start of something else), its HTML is kept, and only
//! the open block at the end is read again as text comes. A table's last
//! row shows once it's whole, so a table grows a row at a time.
//!
//! A finished code block is painted with the spans Arko highlighted it with
//! (found by its code's hash), once they've come. Its language and its copy
//! button sit in its corner.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pulldown_cmark::{
    CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd, TextMergeStream,
};

use crate::protocol::{CodeSpans, HIGHLIGHT_NAMES, code_hash};

/// The code blocks' spans kept, until there are this many.
const KEPT: usize = 8192;

thread_local! {
    /// The spans of the code blocks Arko highlighted, by their code's hash.
    static CODE: RefCell<HashMap<String, Rc<Vec<[u32; 3]>>>> = RefCell::new(HashMap::new());
}

/// Keeps the spans Arko sent for code blocks, for the Markdown that shows
/// them.
pub fn remember(code: &[CodeSpans]) {
    CODE.with(|kept| {
        let mut kept = kept.borrow_mut();
        if kept.len() + code.len() > KEPT {
            kept.clear();
        }
        for block in code {
            kept.insert(block.hash.clone(), Rc::new(block.spans.clone()));
        }
    });
}

/// Appends `code` escaped, in the spans Arko highlighted it with, if any.
fn code_html(html: &mut String, code: &str) {
    let Some(spans) = CODE.with(|kept| kept.borrow().get(&code_hash(code)).cloned()) else {
        escape(html, code);
        return;
    };
    let mut at = 0;
    for &[start, end, name] in spans.iter() {
        let (start, end) = (start as usize, end as usize);
        let Some(name) = HIGHLIGHT_NAMES.get(name as usize) else {
            continue;
        };
        if start < at
            || end > code.len()
            || !code.is_char_boundary(start)
            || !code.is_char_boundary(end)
        {
            continue;
        }
        escape(html, &code[at..start]);
        html.push_str("<span class=\"hl-");
        html.push_str(&name.replace('.', "-"));
        html.push_str("\">");
        escape(html, &code[start..end]);
        html.push_str("</span>");
        at = end;
    }
    escape(html, &code[at..]);
}

/// `text` in Markdown, as safe HTML.
pub fn to_html(text: &str) -> String {
    let mut html = String::with_capacity(text.len() + text.len() / 4);
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    // Inside a link, a web address stays text.
    let mut plain = 0u32;
    // A code block's code, gathered to paint it whole, and its language.
    let mut code: Option<String> = None;
    let mut language: Option<String> = None;
    for event in TextMergeStream::new(Parser::new_ext(text, options)) {
        match event {
            Event::Start(tag) => {
                plain += u32::from(matches!(tag, Tag::Link { .. }));
                if let Tag::CodeBlock(kind) = &tag {
                    code = Some(String::new());
                    language = language_of(kind).map(str::to_string);
                }
                open(&mut html, tag)
            }
            Event::End(tag) => {
                plain = plain.saturating_sub(u32::from(matches!(tag, TagEnd::Link)));
                let block = matches!(tag, TagEnd::CodeBlock);
                if block && let Some(code) = code.take() {
                    code_html(&mut html, &code);
                }
                close(&mut html, tag);
                if block {
                    code_actions(&mut html, language.take().as_deref());
                }
            }
            Event::Text(text) if code.is_some() => {
                if let Some(code) = &mut code {
                    code.push_str(&text);
                }
            }
            Event::Text(text) if plain == 0 => link_addresses(&mut html, &text),
            Event::Text(text) => escape(&mut html, &text),
            Event::Code(code) => {
                html.push_str("<code>");
                escape(&mut html, &code);
                html.push_str("</code>");
            }
            // Shown as typed, never as markup.
            Event::Html(raw) | Event::InlineHtml(raw) => escape(&mut html, &raw),
            Event::SoftBreak => html.push('\n'),
            Event::HardBreak => html.push_str("<br>"),
            Event::Rule => html.push_str("<hr>"),
            Event::TaskListMarker(done) => {
                html.push_str(if done { "☑ " } else { "☐ " });
            }
            _ => {}
        }
    }
    html
}

fn open(html: &mut String, tag: Tag) {
    match tag {
        Tag::Paragraph => html.push_str("<p>"),
        Tag::Heading { level, .. } => {
            // Headings stay small in a transcript.
            let level = match level {
                HeadingLevel::H1 | HeadingLevel::H2 => 3,
                HeadingLevel::H3 => 4,
                _ => 5,
            };
            html.push_str(&format!("<h{level}>"));
        }
        Tag::BlockQuote(_) => html.push_str("<blockquote>"),
        Tag::CodeBlock(kind) => {
            html.push_str("<div class=\"code\"><pre><code");
            if let Some(language) = language_of(&kind) {
                html.push_str(" data-lang=\"");
                escape(html, language);
                html.push('"');
            }
            html.push('>');
        }
        Tag::List(Some(start)) => {
            if start == 1 {
                html.push_str("<ol>");
            } else {
                html.push_str(&format!("<ol start=\"{start}\">"));
            }
        }
        Tag::List(None) => html.push_str("<ul>"),
        Tag::Item => html.push_str("<li>"),
        Tag::Emphasis => html.push_str("<em>"),
        Tag::Strong => html.push_str("<strong>"),
        Tag::Strikethrough => html.push_str("<del>"),
        Tag::Link { dest_url, .. } => {
            if safe_url(&dest_url) {
                html.push_str("<a target=\"_blank\" rel=\"noopener noreferrer\" href=\"");
                escape(html, &dest_url);
                html.push_str("\">");
            } else {
                html.push_str("<a>");
            }
        }
        Tag::Table(_) => html.push_str("<div class=\"table\"><table>"),
        Tag::TableHead => html.push_str("<thead><tr>"),
        Tag::TableRow => html.push_str("<tr>"),
        Tag::TableCell => html.push_str("<td>"),
        // Images show as their text: the page loads only its own files.
        Tag::Image { .. } => {}
        _ => {}
    }
}

fn close(html: &mut String, tag: TagEnd) {
    let end = match tag {
        TagEnd::Paragraph => "</p>",
        TagEnd::Heading(level) => match level {
            HeadingLevel::H1 | HeadingLevel::H2 => "</h3>",
            HeadingLevel::H3 => "</h4>",
            _ => "</h5>",
        },
        TagEnd::BlockQuote(_) => "</blockquote>",
        TagEnd::CodeBlock => "</code></pre>",
        TagEnd::List(true) => "</ol>",
        TagEnd::List(false) => "</ul>",
        TagEnd::Item => "</li>",
        TagEnd::Emphasis => "</em>",
        TagEnd::Strong => "</strong>",
        TagEnd::Strikethrough => "</del>",
        TagEnd::Link => "</a>",
        TagEnd::Table => "</table></div>",
        TagEnd::TableHead => "</tr></thead>",
        TagEnd::TableRow => "</tr>",
        TagEnd::TableCell => "</td>",
        _ => "",
    };
    html.push_str(end);
}

/// A fenced code block's language, as its fence names it.
fn language_of<'a>(kind: &'a CodeBlockKind) -> Option<&'a str> {
    match kind {
        CodeBlockKind::Fenced(info) => info.split_whitespace().next(),
        CodeBlockKind::Indented => None,
    }
}

/// Closes a code block with its corner, which holds its language and its
/// copy button.
fn code_actions(html: &mut String, language: Option<&str>) {
    html.push_str("<div class=\"code-actions\">");
    if let Some(language) = language {
        html.push_str("<span class=\"code-lang\">");
        escape(html, language);
        html.push_str("</span>");
    }
    html.push_str(&crate::copy::code_button());
    html.push_str("</div></div>");
}

fn safe_url(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
}

/// Appends `text` escaped, with each web address in it as a link. An
/// address starts a word, and the punctuation that ends a sentence after it
/// isn't its.
fn link_addresses(html: &mut String, text: &str) {
    let mut rest = text;
    let mut previous = None;
    while let Some((at, _)) = rest.char_indices().find(|&(at, _)| {
        let starts = rest[at..]
            .get(..8)
            .is_some_and(|head| head.eq_ignore_ascii_case("https://"))
            || rest[at..]
                .get(..7)
                .is_some_and(|head| head.eq_ignore_ascii_case("http://"));
        let before = if at == 0 {
            previous
        } else {
            rest[..at].chars().next_back()
        };
        starts && before.is_none_or(|c: char| c.is_whitespace() || c == '(' || c == '<')
    }) {
        escape(html, &rest[..at]);
        let address = &rest[at..];
        let end = address
            .find(|c: char| c.is_whitespace() || c == '<' || c == '>')
            .unwrap_or(address.len());
        let address =
            address[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '\'', '"']);
        html.push_str("<a target=\"_blank\" rel=\"noopener noreferrer\" href=\"");
        escape(html, address);
        html.push_str("\">");
        escape(html, address);
        html.push_str("</a>");
        rest = &rest[at + address.len()..];
        previous = address.chars().next_back();
    }
    escape(html, rest);
}

/// Appends `text` with `&`, `<`, `>` and `"` escaped.
pub fn escape(html: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => html.push_str("&amp;"),
            '<' => html.push_str("&lt;"),
            '>' => html.push_str("&gt;"),
            '"' => html.push_str("&quot;"),
            c => html.push(c),
        }
    }
}

/// A streaming text's finished blocks and where its open block starts.
#[derive(Default)]
pub struct Blocks {
    /// How many bytes of the text the finished blocks hold. A block never
    /// ends inside a code fence, so the scan starts outside one.
    consumed: usize,
    /// The last update started over, so its finished blocks are all of them.
    pub started_over: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Open {
    Other,
    List,
    Quote,
}

impl Blocks {
    /// Reads `text`, which grew from what was read before (or starts over
    /// when it didn't). Returns the blocks it finished now, each its source
    /// and its HTML, and the HTML of the open block. With `streaming`, a
    /// table's open last row is held.
    pub fn update(&mut self, text: &str, streaming: bool) -> (Vec<(String, String)>, String) {
        self.started_over = false;
        if text.len() < self.consumed || !text.is_char_boundary(self.consumed) {
            *self = Blocks {
                started_over: true,
                ..Blocks::default()
            };
        }
        let mut finished = Vec::new();
        let mut start = self.consumed;
        let mut fence: Option<String> = None;
        let mut open: Option<Open> = None;
        let mut previous_blank = false;
        let mut at = start;
        // Only whole lines decide anything.
        while let Some(end) = text[at..].find('\n').map(|end| at + end) {
            let line = &text[at..end];
            let trimmed = line.trim_start();
            let top = trimmed.len() == line.len();
            if let Some(marker) = &fence {
                let closes = top
                    && trimmed.starts_with(marker.as_str())
                    && trimmed.trim_end().chars().all(|c| marker.starts_with(c));
                if closes {
                    // The fence closes, and its block is finished.
                    fence = None;
                    finished.push(finished_block(&text[start..end + 1]));
                    start = end + 1;
                    open = None;
                }
                previous_blank = false;
                at = end + 1;
                continue;
            }
            if line.trim().is_empty() {
                previous_blank = true;
                at = end + 1;
                continue;
            }
            let continues = match open {
                Some(Open::List) => is_item(trimmed),
                Some(Open::Quote) => trimmed.starts_with('>'),
                _ => false,
            };
            if previous_blank && top && start < at && !continues {
                finished.push(finished_block(&text[start..at]));
                start = at;
                open = None;
            }
            if top && let Some(marker) = fence_marker(trimmed) {
                // What came before the fence is a block of its own.
                if start < at {
                    finished.push(finished_block(&text[start..at]));
                }
                start = at;
                fence = Some(marker);
            }
            if open.is_none() {
                open = Some(if is_item(trimmed) {
                    Open::List
                } else if trimmed.starts_with('>') {
                    Open::Quote
                } else {
                    Open::Other
                });
            }
            previous_blank = false;
            at = end + 1;
        }
        self.consumed = start;
        let tail = &text[start..];
        // A fence still open shows plain, to the end.
        let html = to_html(if streaming {
            without_open_row(tail)
        } else {
            tail
        });
        (finished, html)
    }
}

/// A finished block: its source, kept to paint it again once its code's
/// spans come, and its HTML.
fn finished_block(source: &str) -> (String, String) {
    (source.to_string(), to_html(source))
}

/// The marker of a code fence that `line` opens (three or more backticks or
/// tildes).
fn fence_marker(line: &str) -> Option<String> {
    let c = line.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let count = line.chars().take_while(|x| *x == c).count();
    (count >= 3).then(|| c.to_string().repeat(count))
}

fn is_item(line: &str) -> bool {
    let mut chars = line.chars();
    match chars.next() {
        Some('-' | '*' | '+') => chars.next() == Some(' '),
        Some(c) if c.is_ascii_digit() => {
            let rest = line.trim_start_matches(|c: char| c.is_ascii_digit());
            rest.starts_with(". ") || rest.starts_with(") ")
        }
        _ => false,
    }
}

/// `text` without the open last row of a table it ends in: a line that
/// starts with a pipe and has fewer cells than the header or no closing
/// pipe yet.
fn without_open_row(text: &str) -> &str {
    let start = text.rfind('\n').map_or(0, |at| at + 1);
    let last = text[start..].trim();
    if !last.starts_with('|') || start == 0 {
        return text;
    }
    let header = text[..start]
        .lines()
        .find(|line| line.trim_start().starts_with('|'));
    let cells = |line: &str| line.trim().trim_matches('|').split('|').count();
    let short = header.is_some_and(|header| cells(last) < cells(header));
    if short || !last.ends_with('|') || last.len() < 2 {
        &text[..start]
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Streams `text` a few bytes at a time, and returns every block's HTML
    /// in order, the finished ones first.
    fn stream(text: &str, step: usize) -> (Vec<String>, String) {
        let mut blocks = Blocks::default();
        let mut finished = Vec::new();
        let mut tail = String::new();
        let mut at = 0;
        while at < text.len() {
            at = (at + step).min(text.len());
            while !text.is_char_boundary(at) {
                at += 1;
            }
            let (done, open) = blocks.update(&text[..at], true);
            finished.extend(done.into_iter().map(|(_, html)| html));
            tail = open;
        }
        (finished, tail)
    }

    #[test]
    fn markdown_reads_headings_emphasis_code_lists_tables_and_links() {
        let html = to_html(
            "# Title\n\nSome **bold**, *em* and `code`.\n\n- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n[x](https://e.com) [y](javascript:alert(1)) <b>raw</b>",
        );
        assert!(html.contains("<h3>Title</h3>"), "{html}");
        assert!(
            html.contains("<strong>bold</strong>")
                && html.contains("<em>em</em>")
                && html.contains("<code>code</code>")
        );
        assert!(html.contains("<ul><li>one</li>"));
        assert!(html.contains("<table>") && html.contains("<td>1</td>"));
        assert!(html.contains("href=\"https://e.com\""));
        assert!(!html.contains("javascript"));
        assert!(html.contains("&lt;b&gt;raw&lt;/b&gt;"));
    }

    #[test]
    fn a_code_blocks_spans_paint_it_once_they_come() {
        let text = "```rust\nfn main() {}\n```\n";
        assert!(to_html(text).contains("<code data-lang=\"rust\">fn main() {}\n</code>"));
        let name = |name: &str| {
            HIGHLIGHT_NAMES
                .iter()
                .position(|known| *known == name)
                .unwrap() as u32
        };
        remember(&[CodeSpans {
            hash: code_hash("fn main() {}\n"),
            // (A span past the code is left out.)
            spans: vec![
                [0, 2, name("keyword")],
                [3, 7, name("function")],
                [9, 99, name("string")],
            ],
        }]);
        let html = to_html(text);
        assert!(
            html.contains(
                r#"<span class="hl-keyword">fn</span> <span class="hl-function">main</span>() {}"#
            ),
            "{html}"
        );
        assert!(!html.contains("hl-string"), "{html}");
    }

    /// A code block's text in the HTML, its markup left out as a browser's
    /// `textContent` leaves it, is its code, which its copy button copies.
    #[test]
    fn a_code_block_has_its_language_and_a_copy_button_beside_its_code() {
        let code = "let s = \"<a & b>\";\nfn main() {}\n";
        remember(&[CodeSpans {
            hash: code_hash(code),
            spans: vec![[8, 18, 27]],
        }]);
        let html = to_html(&format!("```rust ignore\n{code}```\n\n    indented\n"));
        assert!(
            html.starts_with(r#"<div class="code"><pre><code data-lang="rust">"#),
            "{html}"
        );
        assert!(
            html.contains(
                r#"<div class="code-actions"><span class="code-lang">rust</span><button"#
            ),
            "{html}"
        );
        assert_eq!(
            html.matches(r#"aria-label="Copy code""#).count(),
            2,
            "{html}"
        );
        let inner = &html[html.find("<code").unwrap()..html.find("</code>").unwrap()];
        let inner = &inner[inner.find('>').unwrap() + 1..];
        let mut text = String::new();
        let mut in_tag = false;
        for c in inner.chars() {
            match c {
                '<' => in_tag = true,
                '>' if in_tag => in_tag = false,
                c if !in_tag => text.push(c),
                _ => {}
            }
        }
        let text = text
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&");
        assert!(inner.contains("<span class=\"hl-string\">"), "{inner}");
        assert_eq!(text, code);
    }

    #[test]
    fn a_web_address_in_the_text_is_a_link() {
        let html = to_html(
            "See https://example.com/a_b. Or (http://a.b/c), not xhttps://no, `https://code` or [x](https://x.y).",
        );
        assert!(
            html.contains(r#"href="https://example.com/a_b">https://example.com/a_b</a>. Or"#),
            "{html}"
        );
        assert!(html.contains(r#"(<a target="_blank" rel="noopener noreferrer" href="http://a.b/c">http://a.b/c</a>),"#), "{html}");
        assert!(!html.contains(r#"href="https://no"#), "{html}");
        assert!(html.contains("<code>https://code</code>"), "{html}");
        assert!(html.contains(r#"href="https://x.y">x</a>"#), "{html}");
    }

    #[test]
    fn a_streamed_reply_reads_the_same_as_the_whole_one_and_keeps_its_finished_blocks() {
        let text = "First paragraph\nstill first.\n\n```rust\nfn main() {}\n\nlet x = 1;\n```\nAfter the code.\n\n- a\n- b\n\n- c\n\nLast one.";
        for step in [1, 3, 7, 50] {
            let (finished, tail) = stream(text, step);
            let joined = finished.concat() + &tail;
            assert_eq!(joined, to_html(text), "step {step}");
            assert!(finished.len() >= 3, "step {step}: {finished:?}");
            assert!(
                finished[1].contains("<pre><code data-lang=\"rust\">"),
                "{finished:?}"
            );
        }
    }

    #[test]
    fn a_streaming_tables_last_row_shows_once_its_whole() {
        let mut blocks = Blocks::default();
        let (_, html) = blocks.update("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 |", true);
        assert!(
            html.contains("<td>2</td>") && !html.contains("<td>3</td>"),
            "{html}"
        );
        let (_, html) = blocks.update("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |", true);
        assert!(html.contains("<td>4</td>"), "{html}");
        // A text that's finished shows every row.
        let (_, html) = Blocks::default().update("| a | b |\n|---|---|\n| 3 |", false);
        assert!(html.contains("<td>3</td>"), "{html}");
    }
}

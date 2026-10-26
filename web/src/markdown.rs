//! Replies in Markdown, as HTML. Raw HTML in the text shows as text, and
//! links open only to web and mail addresses.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// `text` in Markdown, as safe HTML.
pub fn to_html(text: &str) -> String {
    let mut html = String::with_capacity(text.len() + text.len() / 4);
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(tag) => open(&mut html, tag),
            Event::End(tag) => close(&mut html, tag),
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
            html.push_str("<pre><code");
            if let CodeBlockKind::Fenced(language) = kind {
                let language = language.split_whitespace().next().unwrap_or("");
                if !language.is_empty() {
                    html.push_str(" data-lang=\"");
                    escape(html, language);
                    html.push('"');
                }
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

fn safe_url(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
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

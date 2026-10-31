//! What the `leave_out_secrets` setting leaves out of the session's words:
//! the values that context marks as secret. Only what such a marker shows
//! is caught. A secret written with nothing marking it, such as a bare
//! `Cookies$123`, can't be told from any other word, and the names stay
//! (`password_hash`, `--password-file` and its path).

use std::ops::Range;

/// Spans a value in quotes may run on for, to its closing quote.
const QUOTED_SPANS: usize = 16;

/// Programs whose flags take a password: `sshpass -p`, `mysql -p…`, `curl
/// -u user:…`.
const PASSWORD_PROGRAMS: &[&str] = &[
    "sshpass",
    "mysql",
    "mysqldump",
    "mysqladmin",
    "mariadb",
    "curl",
];

/// The byte ranges of `text` that context marks as secret, in order:
///
/// - the value after a password, key or token name (`API_KEY=…`,
///   `password: …`, `--token …`, `"client_secret": "…"`, `?token=…`), to its
///   closing quote when it's quoted;
/// - the rest of an `Authorization`, `Proxy-Authorization` or `Cookie`
///   header's line, and what follows `Bearer`;
/// - the password `sshpass -p`, `mysql -p…` and `curl -u user:…` take;
/// - emails, the user (and password) in a URL, and PEM blocks.
pub(super) fn secret_ranges(text: &str) -> Vec<Range<usize>> {
    let spans = spans(text);
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut pending = Pending::Nothing;
    // A text that has a block's end before any start began inside it.
    let mut pem = match (text.find("-----BEGIN"), text.find("-----END")) {
        (begin, Some(end)) if begin.is_none_or(|begin| begin > end) => Pem::Inside,
        _ => Pem::Outside,
    };
    // A program met in this line whose flags take a password.
    let mut program: Option<&str> = None;
    for (i, range) in spans.iter().enumerate() {
        let span = &text[range.clone()];
        let next = spans.get(i + 1).map(|next| &text[next.clone()]);
        if i == 0 || text[spans[i - 1].end..range.start].contains('\n') {
            program = None;
            if pending == Pending::Line {
                pending = Pending::Nothing;
            }
        }
        if pem != Pem::Outside || span.contains("-----BEGIN") {
            ranges.push(range.clone());
            pem = match pem {
                _ if !span.contains("-----END") && pem != Pem::Ending => Pem::Inside,
                _ if bare(span).ends_with("-----") => Pem::Outside,
                _ => Pem::Ending,
            };
            continue;
        }
        let bare = bare(span);
        match pending {
            Pending::Nothing => {}
            Pending::Values(left) => {
                ranges.push(range.clone());
                pending = match quote_opened(span) {
                    Some(mark) => Pending::Quote(mark, QUOTED_SPANS),
                    // ("token: Bearer …")
                    None if is_scheme(bare) => Pending::Values(1),
                    None if left > 1 => Pending::Values(left - 1),
                    None => Pending::Nothing,
                };
                continue;
            }
            Pending::Quote(mark, left) => {
                ranges.push(range.clone());
                let closes = span.matches(mark).count() % 2 == 1;
                pending = if closes || left == 1 {
                    Pending::Nothing
                } else {
                    Pending::Quote(mark, left - 1)
                };
                continue;
            }
            Pending::Line => {
                ranges.push(range.clone());
                // (A header in quotes ends with them: -H "Authorization: …".)
                if span.ends_with(['"', '\'']) {
                    pending = Pending::Nothing;
                }
                continue;
            }
            Pending::AfterColon => {
                if let Some(at) = span.find(':') {
                    ranges.push(range.start + at + 1..range.end);
                }
                pending = Pending::Nothing;
                continue;
            }
        }
        if matches!(bare, "&&" | "||" | "|" | ";") {
            program = None;
            continue;
        }
        if PASSWORD_PROGRAMS.contains(&bare) {
            program = Some(bare);
            continue;
        }
        match (program, bare) {
            (Some("sshpass"), "-p") => {
                pending = Pending::Values(1);
                continue;
            }
            (Some("curl"), "-u" | "--user") => {
                pending = Pending::AfterColon;
                continue;
            }
            (Some("mysql" | "mysqldump" | "mysqladmin" | "mariadb"), flag)
                if flag.len() > 2 && flag.starts_with("-p") && !flag.starts_with("--") =>
            {
                let at = span.find("-p").unwrap_or(0) + 2;
                ranges.push(range.start + at..range.end);
                continue;
            }
            _ => {}
        }
        if is_scheme(bare) && bare.eq_ignore_ascii_case("bearer") {
            pending = Pending::Values(1);
            continue;
        }
        if let Some(scheme) = span.find("://") {
            // The user (and password) in a URL, to the @.
            let authority = &span[scheme + 3..];
            let end = authority.find('/').unwrap_or(authority.len());
            if let Some(at) = authority[..end].rfind('@') {
                let start = range.start + scheme + 3;
                ranges.push(start..start + at + 1);
            }
        } else if is_email(bare) {
            ranges.push(range.clone());
            continue;
        }
        // Names with their values: `NAME=value`, `name: value`, `--name=value`,
        // `"name": "value"`, `a=1&token=…`.
        let mut start = 0;
        for piece in span.split_inclusive(['&', '?', ';', ',']) {
            let at = start;
            start += piece.len();
            let piece = piece.trim_end_matches(['&', '?', ';', ',']);
            let Some(separator) = piece.find(['=', ':']) else {
                continue;
            };
            if piece[separator..].starts_with("://") {
                continue;
            }
            let Some(marker) = marker(&piece[..separator]) else {
                continue;
            };
            let value = at + separator + 1..at + piece.len();
            if value.is_empty() {
                // The value is the next span (and so on to the line's end,
                // for a header).
                if start >= span.trim_end_matches(['&', '?', ';', ',']).len() {
                    pending = match marker {
                        Marker::Header => Pending::Line,
                        Marker::Value => Pending::Values(1),
                    };
                }
                continue;
            }
            ranges.push(range.start + value.start..range.start + value.end);
            pending = match (marker, quote_opened(&span[value.clone()])) {
                (Marker::Header, _) => Pending::Line,
                (_, Some(mark)) => Pending::Quote(mark, QUOTED_SPANS),
                _ => pending,
            };
        }
        if pending != Pending::Nothing {
            continue;
        }
        // `NAME = value`, and `--name value`.
        if marker(bare).is_some()
            && next.is_some_and(|next| matches!(next, "=" | ":" | ":=" | "=>"))
        {
            pending = Pending::Values(2);
        } else if let Some(flag) = bare.strip_prefix("--")
            && !flag.contains('=')
            && marker(flag).is_some()
        {
            pending = Pending::Values(1);
        }
    }
    ranges.sort_by_key(|range| range.start);
    ranges
}

/// What the next spans hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Nothing,
    /// This many values, one a span.
    Values(usize),
    /// A value in quotes, to its closing quote, within this many spans.
    Quote(char, usize),
    /// A header's value, to the line's end.
    Line,
    /// `user:password`, of which the password.
    AfterColon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pem {
    Outside,
    Inside,
    /// Within the line that ends the block.
    Ending,
}

/// What a name marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    /// A secret value: a password, a key, a token.
    Value,
    /// A header whose value runs to the line's end.
    Header,
}

/// What the name `name` marks, by its last word: `API_KEY`, `apiKey`,
/// `client_secret`, `GITHUB_TOKEN`, `password`, `--auth`. A key needs a word
/// before it (`key` alone is too common), and a name whose last word says
/// what else it is isn't one (`password_hash`, `--password-file`,
/// `token_count`).
fn marker(name: &str) -> Option<Marker> {
    let name = name.trim_matches(|c: char| !c.is_alphanumeric());
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    ) {
        return Some(Marker::Header);
    }
    let words: Vec<String> = name
        .split(|c: char| !c.is_alphanumeric())
        .flat_map(super::camel_words)
        .map(str::to_ascii_lowercase)
        .collect();
    let last = words.last()?.as_str();
    let secret = matches!(
        last,
        "password"
            | "passwd"
            | "passphrase"
            | "pwd"
            | "pass"
            | "secret"
            | "token"
            | "apikey"
            | "credential"
            | "credentials"
            | "auth"
            | "cookie"
    ) || (last == "key" && words.len() > 1);
    secret.then_some(Marker::Value)
}

/// An authorization scheme whose next span is the credential.
fn is_scheme(word: &str) -> bool {
    ["bearer", "basic", "token", "digest"]
        .iter()
        .any(|scheme| scheme.eq_ignore_ascii_case(word))
}

/// `local@domain.tld`.
fn is_email(word: &str) -> bool {
    word.split_once('@').is_some_and(|(local, domain)| {
        local.ends_with(|c: char| c.is_alphanumeric())
            && domain.contains('.')
            && domain.starts_with(|c: char| c.is_alphanumeric())
    })
}

/// The quote `span` leaves open, if any.
fn quote_opened(span: &str) -> Option<char> {
    ['"', '\'']
        .into_iter()
        .find(|mark| span.matches(*mark).count() % 2 == 1)
}

/// `span` without the quotes, brackets and punctuation around it.
fn bare(span: &str) -> &str {
    span.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';' | '.'
        )
    })
}

/// The runs of `text` between spaces.
fn spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = None;
    for (at, c) in text.char_indices().chain([(text.len(), ' ')]) {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(at),
            (true, Some(from)) => {
                spans.push(from..at);
                start = None;
            }
            _ => {}
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What of `text` stays, the secret ranges taken out.
    fn kept(text: &str) -> String {
        let mut kept = String::new();
        let mut at = 0;
        for range in secret_ranges(text) {
            if range.start >= at {
                kept.push_str(&text[at..range.start]);
                kept.push('·');
                at = range.end;
            }
        }
        kept.push_str(&text[at..]);
        kept
    }

    #[test]
    fn values_next_to_markers_go_and_names_stay() {
        for (text, expected) in [
            ("PASSWORD=Hunter2 next", "PASSWORD=· next"),
            ("export API_KEY=abc def", "export API_KEY=· def"),
            ("password: hunter2 next", "password: · next"),
            ("DB_PASSWORD = \"two words\" next", "DB_PASSWORD · · · next"),
            (
                r#"{"client_secret": "Banana_Split", "name": "x"}"#,
                r#"{"client_secret": · "name": "x"}"#,
            ),
            (
                "tool --password HunterSix --api-key=HunterSeven run",
                "tool --password · --api-key=· run",
            ),
            (
                "curl -H 'Authorization: Bearer abc' https://x.example/y\nnext",
                "curl -H 'Authorization: · · https://x.example/y\nnext",
            ),
            ("token: Bearer abc next", "token: · · next"),
            (
                "open https://x.example/v1?token=abc&page=2 now",
                "open https://x.example/v1?token=·&page=2 now",
            ),
            ("sshpass -p HunterFour ssh host", "sshpass -p · ssh host"),
            ("mysql -u root -pHunterThree db", "mysql -u root -p· db"),
            (
                "curl -u root:HunterEleven https://x",
                "curl -u root:· https://x",
            ),
            (
                "git clone https://jane:pw@github.com/org/repo",
                "git clone https://·github.com/org/repo",
            ),
            ("Mail jane.doe@example.com today", "Mail · today"),
            (
                "Key:\n-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAKj34\n-----END RSA PRIVATE KEY-----\nafter",
                "Key:\n· · · ·\n·\n· · · ·\nafter",
            ),
        ] {
            assert_eq!(kept(text), expected, "{text}");
        }
    }

    /// Names that say what else they are, a key without a word before it,
    /// and ordinary code and prose keep everything.
    #[test]
    fn other_names_and_text_keep_their_values() {
        for text in [
            "tool --password-file ~/.config/pw.txt run",
            "let password_hash = hash(input);",
            "let key = map.get(name);",
            "token_count: 12, max_tokens = 300",
            "cargo test -p arko-tui",
            "git push -u origin main",
            "@Override public void run()",
            "Note: the keyboard and the passage are fine.",
            "https://docs.rs/serde/latest/serde/",
        ] {
            assert_eq!(kept(text), text, "{text}");
        }
    }
}

//! How sessions are named. By default a session takes the first words of
//! its first message ([`short_name`]). The user can turn on naming by a
//! model instead, which runs in the background and falls back to the first
//! words when no way of asking one works.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// How many columns a name made from the first words takes at most,
/// including its "…".
pub const SHORT_NAME: usize = 30;

/// The first words of `text` as a session's name. It is one line, and when
/// it is longer than [`SHORT_NAME`] columns it is cut at a word boundary
/// with "…" after it ("Fix the flaky scroll test in…"). A word longer than
/// the room, or text with no spaces, is cut where it no longer fits. The
/// cut counts columns, so Chinese or emoji text is shorter in characters
/// and never splits a character that joins others (an emoji's skin tone, a
/// family).
pub fn short_name(text: &str) -> String {
    cut_words(text, SHORT_NAME, true)
}

/// `text` on one line of at most `limit` columns, cut at a word boundary
/// (with a "…" in the last column when `ellipsis` is set).
fn cut_words(text: &str, limit: usize, ellipsis: bool) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.width() <= limit {
        return line;
    }
    let room = limit.saturating_sub(usize::from(ellipsis));
    let mut end = 0;
    let mut used = 0;
    for (at, c) in line.char_indices() {
        used += c.width().unwrap_or(0);
        if used > room {
            break;
        }
        end = at + c.len_utf8();
    }
    // Not between a character and what joins to it.
    while let (Some(before), Some(after)) =
        (line[..end].chars().next_back(), line[end..].chars().next())
    {
        if after.width().unwrap_or(0) != 0 && before != ZERO_WIDTH_JOINER {
            break;
        }
        end -= before.len_utf8();
    }
    let start = if line[end..].starts_with(' ') {
        end
    } else {
        line[..end].rfind(' ').unwrap_or(end)
    };
    let kept = line[..start].trim_end_matches([' ', ',', ';', ':']);
    let kept = if kept.is_empty() { &line[..end] } else { kept };
    if ellipsis {
        format!("{kept}…")
    } else {
        kept.to_string()
    }
}

const ZERO_WIDTH_JOINER: char = '\u{200D}';

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_first_line_is_the_name() {
        assert_eq!(short_name("Fix the build"), "Fix the build");
        assert_eq!(short_name("  Fix \n the   build "), "Fix the build");
        assert_eq!(short_name(""), "");
    }

    #[test]
    fn a_long_one_is_cut_at_a_word_with_an_ellipsis() {
        assert_eq!(
            short_name("Fix the flaky scroll test in the sidebar"),
            "Fix the flaky scroll test in…"
        );
        assert_eq!(
            short_name("Explain how the session names are chosen when nothing is set"),
            "Explain how the session names…"
        );
    }

    #[test]
    fn a_name_that_fits_exactly_is_not_cut() {
        let exact = "Fix the flaky scroll test now!";
        assert_eq!(exact.chars().count(), 30);
        assert_eq!(short_name(exact), exact);
        // One more character, and the last word goes.
        assert_eq!(
            short_name("Fix the flaky scroll test now!!"),
            "Fix the flaky scroll test…"
        );
    }

    #[test]
    fn a_cut_name_with_its_ellipsis_fits_in_30_columns() {
        for text in [
            "aaaa bbbb cccc dddd eeee ffff gggg hhhh",
            "Fix the flaky scroll test in the sidebar",
            &"word ".repeat(40),
            &"x".repeat(100),
        ] {
            let name = short_name(text);
            assert!(name.width() <= 30, "{name} is {} columns", name.width());
            assert!(name.ends_with('…'), "{name}");
        }
        // A cut that falls right after a word ends there, without a space.
        let name = short_name(&format!("{} next", "a".repeat(29)));
        assert_eq!(name, format!("{}…", "a".repeat(29)));
    }

    #[test]
    fn text_without_spaces_is_cut_where_it_stops_fitting() {
        assert_eq!(short_name(&"x".repeat(30)), "x".repeat(30));
        assert_eq!(short_name(&"x".repeat(31)), format!("{}…", "x".repeat(29)));
    }

    #[test]
    fn a_trailing_comma_does_not_stay_before_the_ellipsis() {
        assert_eq!(
            short_name("Fix the flaky scroll test in, the sidebar"),
            "Fix the flaky scroll test in…"
        );
        assert_eq!(
            short_name("Fix the flaky scroll tests, and the sidebar"),
            "Fix the flaky scroll tests…"
        );
    }

    #[test]
    fn wide_characters_count_two_columns() {
        let name = short_name(&"字".repeat(40));
        assert_eq!(name, format!("{}…", "字".repeat(14)));
        assert!(name.width() <= 30);
        // Fifteen fit exactly, so a text of fifteen is not cut.
        assert_eq!(short_name(&"字".repeat(15)), "字".repeat(15));
        let emoji = short_name(&"🙂".repeat(20));
        assert_eq!(emoji, format!("{}…", "🙂".repeat(14)));
    }

    #[test]
    fn a_character_made_of_several_is_not_split() {
        let family = "👨\u{200D}👩\u{200D}👧";
        // The cut would fall inside the family.
        let name = short_name(&format!("{}{family} rest of the words", "a".repeat(26)));
        assert_eq!(name, format!("{}…", "a".repeat(26)));
        // An accent stays with its letter.
        let accented = format!("{}{}", "a".repeat(28), "e\u{301}".repeat(3));
        assert_eq!(
            short_name(&accented),
            format!("{}e\u{301}…", "a".repeat(28))
        );
        // The family itself is kept when it fits.
        let kept = short_name(&format!("{family} {}", "word ".repeat(10)));
        assert!(kept.starts_with(family), "{kept}");
    }
}

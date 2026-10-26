//! How sessions are named. By default a session takes the first words of
//! its first message ([`short_name`]). The user can turn on naming by a
//! model instead, with an ordered list of [`Way`]s to ask, kept in the Arko
//! folder ([`load`], [`save`]). It runs in the background, and the first
//! words stay the name when no way works.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{claude_code, credentials};

mod command;
mod naming;

/// The file in Arko's folder that holds the ways.
const FILE: &str = "session-names.json";

/// One way of asking a model for a session's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Way {
    /// Haiku 5.5 in Claude Code, on the user's Claude plan.
    #[serde(rename = "claude-plan")]
    ClaudePlan,
    /// GPT-6 Luna on the user's ChatGPT plan, through the ChatGPT login.
    #[serde(rename = "chatgpt-plan")]
    ChatGptPlan,
    /// Haiku 5.5 with the Anthropic API key, billed per token.
    #[serde(rename = "claude-key")]
    ClaudeKey,
    /// GPT-6 Luna with the OpenAI API key, billed per token.
    #[serde(rename = "openai-key")]
    OpenAiKey,
}

impl Way {
    /// The four ways, in the order they are offered.
    pub const ALL: [Way; 4] = [
        Way::ClaudePlan,
        Way::ChatGptPlan,
        Way::ClaudeKey,
        Way::OpenAiKey,
    ];

    /// The word that names it in a command and in the file.
    pub fn id(self) -> &'static str {
        match self {
            Way::ClaudePlan => "claude-plan",
            Way::ChatGptPlan => "chatgpt-plan",
            Way::ClaudeKey => "claude-key",
            Way::OpenAiKey => "openai-key",
        }
    }

    /// What it does, for a list.
    pub fn label(self) -> &'static str {
        match self {
            Way::ClaudePlan => "Haiku 5.5 on the Claude plan",
            Way::ChatGptPlan => "GPT-6 Luna on the ChatGPT plan",
            Way::ClaudeKey => "Haiku 5.5 with the Anthropic API key",
            Way::OpenAiKey => "GPT-6 Luna with the OpenAI API key",
        }
    }

    /// The way `word` names.
    pub fn from_id(word: &str) -> Option<Way> {
        Way::ALL.into_iter().find(|way| way.id() == word)
    }

    /// Whether it bills per token, which is why turning naming on leaves it
    /// off.
    pub fn billed(self) -> bool {
        matches!(self, Way::ClaudeKey | Way::OpenAiKey)
    }
}

/// What turning naming by a model on starts with, the two ways that use a
/// plan.
pub fn default_ways() -> Vec<Way> {
    Way::ALL.into_iter().filter(|way| !way.billed()).collect()
}

#[derive(Serialize)]
struct Saved {
    ways: Vec<Way>,
}

/// The ways to ask for a name, in the order to try them. None means the
/// session takes the first words of its first message. A missing or
/// unreadable file is none, and a way this version doesn't know is left out.
pub fn load(root: &Path) -> Vec<Way> {
    let Ok(text) = std::fs::read_to_string(root.join(FILE)) else {
        return Vec::new();
    };
    // Read one way at a time, so a way a later version adds doesn't spoil the list.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let mut ways = Vec::new();
    for way in value
        .get("ways")
        .and_then(|ways| ways.as_array())
        .into_iter()
        .flatten()
        .filter_map(|way| serde_json::from_value::<Way>(way.clone()).ok())
    {
        if !ways.contains(&way) {
            ways.push(way);
        }
    }
    ways
}

/// Keeps `ways` as the ones to ask, in this order (none for the first
/// words). A way listed twice counts once.
pub fn save(root: &Path, ways: &[Way]) -> Result<(), String> {
    let mut unique = Vec::new();
    for way in ways {
        if !unique.contains(way) {
            unique.push(*way);
        }
    }
    let text = serde_json::to_string_pretty(&Saved { ways: unique })
        .map_err(|error| format!("Couldn't save the session names ({error})"))?;
    std::fs::create_dir_all(root)
        .and_then(|()| crate::session::write_whole(&root.join(FILE), text.as_bytes()))
        .map_err(|error| format!("Couldn't save the session names ({error})"))
}

/// The four ways as a list shows them. The ones on come first, in their
/// order, and then the others in the usual order. The second value says
/// whether it is on.
pub fn arranged(ways: &[Way]) -> Vec<(Way, bool)> {
    ways.iter()
        .map(|way| (*way, true))
        .chain(
            Way::ALL
                .into_iter()
                .filter(|way| !ways.contains(way))
                .map(|way| (way, false)),
        )
        .collect()
}

/// The ways a `/session-names` line names. `words` is none, and the other
/// forms list the ways in the order to try them.
pub fn parse(words: &[&str]) -> Result<Vec<Way>, String> {
    let mut ways = Vec::new();
    if words == ["words"] {
        return Ok(ways);
    }
    for word in words {
        let way = Way::from_id(word)
            .ok_or_else(|| format!("`{word}` is not a way to name a session. {USAGE}"))?;
        if !ways.contains(&way) {
            ways.push(way);
        }
    }
    Ok(ways)
}

/// What `/session-names` takes.
pub const USAGE: &str = "/session-names words names sessions by their first words. /session-names claude-plan chatgpt-plan claude-key openai-key names them by a model, trying the ways in the order given.";

/// The setting in a sentence, for a command's reply.
pub fn describe(ways: &[Way]) -> String {
    if ways.is_empty() {
        return "Sessions are named by the first words of their first message.".to_string();
    }
    let tries: Vec<&str> = ways.iter().map(|way| way.label()).collect();
    format!(
        "Sessions are named by a model, trying {}. The first words are the last resort.",
        tries.join(", then ")
    )
}

/// What each way needs that is missing now, for a list to say beside it
/// (`None` when it can run). It reads the Arko folder's keys and ChatGPT
/// logins as they are, and runs `claude` to check Claude Code (up to 15 s
/// each time), so call it off a UI thread.
pub fn missing(root: &Path) -> Vec<(Way, Option<String>)> {
    crate::context::with_env(
        credentials::Env::for_folder_in(None, true, Some(root)),
        missing_here,
    )
}

/// [`missing`] for the settings this thread reads (a session's).
pub(crate) fn missing_here() -> Vec<(Way, Option<String>)> {
    missing_until(&AtomicBool::new(false))
}

/// [`missing_here`], where the check of Claude Code ends at once when
/// `cancel` is set (the menu that asked closed).
pub(crate) fn missing_until(cancel: &AtomicBool) -> Vec<(Way, Option<String>)> {
    Way::ALL
        .into_iter()
        .map(|way| (way, why_not(way, cancel)))
        .collect()
}

fn why_not(way: Way, cancel: &AtomicBool) -> Option<String> {
    match way {
        Way::ClaudePlan => claude_reason(&claude_code::check_plan(cancel)),
        Way::ChatGptPlan => login_reason(&credentials::chatgpt_logins(None)),
        Way::ClaudeKey => key_reason(&credentials::ANTHROPIC),
        Way::OpenAiKey => key_reason(&credentials::OPENAI),
    }
}

/// What the Claude plan way lacks, from a check of Claude Code made for it
/// ([`claude_code::check_plan`]). Only a login that is a Claude plan counts.
fn claude_reason(check: &Result<claude_code::Ready, claude_code::Problem>) -> Option<String> {
    Some(
        match check {
            Ok(ready) if ready.on_plan() => return None,
            Ok(_) => "Claude Code isn't signed in with a Claude plan",
            Err(claude_code::Problem::Missing) => "Claude Code isn't installed",
            Err(claude_code::Problem::SignedOut(_)) => "Claude Code isn't signed in",
            Err(claude_code::Problem::Outdated { .. }) => "Claude Code needs an update",
        }
        .to_string(),
    )
}

fn login_reason(logins: &[credentials::ChatGptLogin]) -> Option<String> {
    use credentials::LoginState;
    if logins.iter().any(|login| login.state == LoginState::Ready) {
        return None;
    }
    let broken = logins
        .iter()
        .any(|login| matches!(login.state, LoginState::Invalid(_)));
    Some(if broken {
        "The ChatGPT login doesn't work".to_string()
    } else {
        "No ChatGPT login".to_string()
    })
}

fn key_reason(key: &credentials::Key) -> Option<String> {
    credentials::key_value(key)
        .is_none()
        .then(|| format!("No {} API key", key.provider))
}

/// How many display columns a name made from the first words takes at most,
/// including its "…".
pub const SHORT_NAME: usize = 30;

/// The first words of `text` as a session's name. It is one line, and when
/// it is wider than [`SHORT_NAME`] display columns it is cut at a word
/// boundary with "…" after it ("Fix the flaky scroll test in…"). A word
/// longer than the room, or text with no spaces, is cut where it no longer
/// fits. The cut counts the columns each user-perceived character takes, so
/// Chinese text and emoji hold fewer characters, and it never splits one
/// (an emoji with a skin tone, a flag, a family, a letter with its accents).
pub fn short_name(text: &str) -> String {
    cut_words(text, SHORT_NAME, true)
}

/// The display columns of `text`, counted a user-perceived character at a
/// time, the way [`cut_words`] cuts.
fn columns(text: &str) -> usize {
    text.graphemes(true).map(UnicodeWidthStr::width).sum()
}

/// `text` on one line of at most `limit` columns, cut at a word boundary
/// (with a "…" in the last column when `ellipsis` is set).
fn cut_words(text: &str, limit: usize, ellipsis: bool) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if columns(&line) <= limit {
        return line;
    }
    let room = limit.saturating_sub(usize::from(ellipsis));
    let mut end = 0;
    let mut used = 0;
    for (at, character) in line.grapheme_indices(true) {
        used += character.width();
        if used > room {
            break;
        }
        end = at + character.len();
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
            assert!(columns(&name) <= 30, "{name} is {} columns", columns(&name));
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
        assert!(columns(&name) <= 30);
        // Fifteen fit exactly, so a text of fifteen is not cut.
        assert_eq!(short_name(&"字".repeat(15)), "字".repeat(15));
        let emoji = short_name(&"🙂".repeat(20));
        assert_eq!(emoji, format!("{}…", "🙂".repeat(14)));
    }

    /// What a user sees as one character, though it is several in the text.
    const CLUSTERS: [(&str, &str); 6] = [
        ("a skin tone", "👍\u{1F3FD}"),
        ("a flag", "🇺🇸"),
        ("a variation selector", "❤\u{FE0F}"),
        ("a family", "👨\u{200D}👩\u{200D}👧"),
        ("an accent", "e\u{301}\u{323}"),
        ("a Hangul syllable", "\u{1112}\u{1161}\u{11AB}"),
    ];

    /// A name is the first characters of the text as a user sees them, never
    /// part of one, whichever column the cut falls on.
    #[test]
    fn a_character_made_of_several_is_never_split() {
        for (what, cluster) in CLUSTERS {
            for padding in 0..=31 {
                let line = format!("{}{}xyz", "a".repeat(padding), cluster.repeat(4));
                let name = short_name(&line);
                let kept = name.strip_suffix('…').unwrap_or(&name);
                let whole: Vec<&str> = line.graphemes(true).collect();
                let shown: Vec<&str> = kept.graphemes(true).collect();
                assert_eq!(
                    shown,
                    whole[..shown.len()],
                    "{what} after {padding}: {name}"
                );
                assert!(columns(&name) <= 30, "{what} after {padding}: {name}");
            }
        }
    }

    #[test]
    fn a_skin_tone_a_flag_and_a_variation_selector_stay_whole() {
        let thumb = "👍\u{1F3FD}";
        // The whole emoji fits in its two columns, so it stays.
        assert_eq!(
            short_name(&format!("{}{thumb}xx", "a".repeat(27))),
            format!("{}{thumb}…", "a".repeat(27))
        );
        // The flag needs two, and only one is left.
        assert_eq!(
            short_name(&format!("{}🇺🇸xx", "a".repeat(28))),
            format!("{}…", "a".repeat(28))
        );
        assert_eq!(
            short_name(&format!("{}❤\u{FE0F}xx", "a".repeat(28))),
            format!("{}…", "a".repeat(28))
        );
        // An accent stays with its letter.
        let accented = format!("{}{}", "a".repeat(28), "e\u{301}".repeat(3));
        assert_eq!(
            short_name(&accented),
            format!("{}e\u{301}…", "a".repeat(28))
        );
    }

    /// A family is one character of two columns, however many it is made
    /// of, so as many fit as in any other text of wide characters.
    #[test]
    fn repeated_families_take_two_columns_each() {
        let family = "👨\u{200D}👩\u{200D}👧";
        assert_eq!(columns(family), 2);
        assert_eq!(short_name(&family.repeat(15)), family.repeat(15));
        assert_eq!(
            short_name(&family.repeat(20)),
            format!("{}…", family.repeat(14))
        );
        let kept = short_name(&format!("{family} {}", "word ".repeat(10)));
        assert!(kept.starts_with(family), "{kept}");
    }

    fn temp_root() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        (temp, root)
    }

    #[test]
    fn no_file_means_the_first_words() {
        let (_temp, root) = temp_root();
        assert_eq!(load(&root), Vec::<Way>::new());
    }

    #[test]
    fn the_ways_are_saved_and_loaded_in_their_order() {
        let (_temp, root) = temp_root();
        let ways = [Way::OpenAiKey, Way::ClaudePlan, Way::ChatGptPlan];
        save(&root, &ways).unwrap();
        assert_eq!(load(&root), ways);
        let text = std::fs::read_to_string(root.join("session-names.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["ways"],
            serde_json::json!(["openai-key", "claude-plan", "chatgpt-plan"])
        );
        // None goes back to the first words.
        save(&root, &[]).unwrap();
        assert_eq!(load(&root), Vec::<Way>::new());
    }

    #[test]
    fn a_way_listed_twice_counts_once() {
        let (_temp, root) = temp_root();
        save(&root, &[Way::ClaudeKey, Way::ClaudePlan, Way::ClaudeKey]).unwrap();
        assert_eq!(load(&root), [Way::ClaudeKey, Way::ClaudePlan]);
    }

    #[test]
    fn a_file_that_cant_be_read_means_the_first_words() {
        let (_temp, root) = temp_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session-names.json");
        for text in ["", "not json", "[]", r#"{"ways": "claude-plan"}"#] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(load(&root), Vec::<Way>::new(), "{text}");
        }
        // A way this version doesn't know is skipped and the others stay.
        std::fs::write(
            &path,
            r#"{"ways": ["claude-plan", "gemini-key", 7, "openai-key", "claude-plan"]}"#,
        )
        .unwrap();
        assert_eq!(load(&root), [Way::ClaudePlan, Way::OpenAiKey]);
    }

    #[test]
    fn turning_it_on_starts_with_the_two_plan_ways() {
        assert_eq!(default_ways(), [Way::ClaudePlan, Way::ChatGptPlan]);
        assert!(Way::ClaudeKey.billed() && Way::OpenAiKey.billed());
    }

    #[test]
    fn the_ways_on_come_first_in_their_order() {
        assert_eq!(
            arranged(&[Way::OpenAiKey, Way::ClaudePlan]),
            [
                (Way::OpenAiKey, true),
                (Way::ClaudePlan, true),
                (Way::ChatGptPlan, false),
                (Way::ClaudeKey, false),
            ]
        );
        assert_eq!(arranged(&[]).len(), 4);
        assert!(arranged(&[]).iter().all(|(_, on)| !on));
    }

    #[test]
    fn a_line_names_the_ways_or_the_first_words() {
        assert_eq!(parse(&["words"]), Ok(Vec::new()));
        assert_eq!(parse(&[]), Ok(Vec::new()));
        assert_eq!(
            parse(&["claude-key", "chatgpt-plan", "claude-key"]),
            Ok(vec![Way::ClaudeKey, Way::ChatGptPlan])
        );
        let error = parse(&["claude-plan", "haiku"]).unwrap_err();
        assert!(error.starts_with("`haiku` is not a way"), "{error}");
        assert!(error.contains(USAGE));
        for way in Way::ALL {
            assert_eq!(Way::from_id(way.id()), Some(way));
        }
    }

    #[test]
    fn the_setting_is_said_in_a_sentence() {
        assert_eq!(
            describe(&[]),
            "Sessions are named by the first words of their first message."
        );
        assert_eq!(
            describe(&[Way::ClaudePlan, Way::OpenAiKey]),
            "Sessions are named by a model, trying Haiku 5.5 on the Claude plan, then GPT-6 Luna with the OpenAI API key. The first words are the last resort."
        );
    }

    #[test]
    fn what_a_way_needs_is_said_in_a_few_words() {
        use claude_code::Problem;
        use credentials::{ChatGptLogin, LoginSource, LoginState};
        let ready = claude_code::Ready {
            path: "claude".into(),
            method: None,
            plan: None,
        };
        // Only a Claude plan's login counts. An API key's, or one Arko
        // couldn't read, may be billed.
        assert_eq!(
            claude_reason(&Ok(ready.clone())).as_deref(),
            Some("Claude Code isn't signed in with a Claude plan")
        );
        let keyed = claude_code::Ready {
            method: Some("api_key".into()),
            ..ready.clone()
        };
        assert!(claude_reason(&Ok(keyed)).is_some());
        let plan = claude_code::Ready {
            method: Some("claude.ai".into()),
            plan: Some("max".into()),
            ..ready
        };
        assert_eq!(claude_reason(&Ok(plan)), None);
        assert_eq!(
            claude_reason(&Err(Problem::Missing)).as_deref(),
            Some("Claude Code isn't installed")
        );
        assert_eq!(
            claude_reason(&Err(Problem::SignedOut("claude".into()))).as_deref(),
            Some("Claude Code isn't signed in")
        );
        assert_eq!(
            claude_reason(&Err(Problem::Outdated {
                version: "2.0.0".into()
            }))
            .as_deref(),
            Some("Claude Code needs an update")
        );
        let login = |state| ChatGptLogin {
            path: None,
            source: LoginSource::Default,
            state,
        };
        assert_eq!(login_reason(&[]).as_deref(), Some("No ChatGPT login"));
        assert_eq!(
            login_reason(&[login(LoginState::Missing)]).as_deref(),
            Some("No ChatGPT login")
        );
        assert_eq!(
            login_reason(&[login(LoginState::Invalid("x".into()))]).as_deref(),
            Some("The ChatGPT login doesn't work")
        );
        // One that works is enough.
        assert_eq!(
            login_reason(&[
                login(LoginState::Invalid("x".into())),
                login(LoginState::Ready)
            ]),
            None
        );
    }
}

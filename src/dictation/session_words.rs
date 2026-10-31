//! The words a dictation favours: those of its session that a speech model
//! would otherwise misspell, such as identifiers, file names, project names
//! and the session's models. Every front end builds the list with
//! [`session_words`] when a recording starts, so they all favour the same
//! words.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use arko_workflows::services::RUN_WORKFLOW_AGENT_TOOL_NAME;

use crate::details::Details;
use crate::remote::{Feed, Item, Kind};

/// The most words a list holds, the size Telmo's accuracy was measured
/// with.
pub const MAX_WORDS: usize = 300;

/// Characters past which a word is taken for noise (a long path, a key), not
/// something said.
const LONGEST: usize = 40;
/// How much of the transcript is read, newest first, in bytes of text.
const TRANSCRIPT_BYTES: usize = 256 * 1024;
/// A mention counts half as much for each this many bytes of transcript
/// after it.
const HALF_LIFE_BYTES: f64 = 32.0 * 1024.0;
/// What a source other than the transcript counts, against 1 for a mention
/// in the latest message. The project's names outweigh a fresh mention, and
/// the session's (models, skills, profiles, tools) equal one.
const PROJECT_WEIGHT: f64 = 2.0;
const TOP_LEVEL_WEIGHT: f64 = 0.5;
const SESSION_WEIGHT: f64 = 1.0;
/// Top-level files and folders read at most, for a workspace such as the
/// home directory.
const TOP_LEVEL_ENTRIES: usize = 200;

/// A word to favour, in its spelling. (A spoken form, such as "engine x"
/// for nginx, would join it here.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWord {
    /// As the session writes it ("DictationOptions", "Cargo.toml", "Opus
    /// 5.5").
    pub text: String,
    /// It comes from code (an identifier, a file name, a name with digits),
    /// so it's kept even with one or two letters or digits.
    pub code: bool,
}

/// The words of the session `feed` and `details` describe, best first, at
/// most [`MAX_WORDS`]. They come from three places.
///
/// - The transcript's identifiers, file names and names, by their shape. A
///   recent or repeated mention counts more.
/// - The workspace's folder name, the package names in its manifests, and
///   its top-level files and folders.
/// - The session's models, skills and worker profiles, and the tools its
///   agents called.
pub fn session_words(feed: &Feed, details: &Details) -> Vec<SessionWord> {
    let snapshot = feed.subscribe().0;
    let facts = details.session();
    let models: Vec<String> = std::iter::once(facts.main.model)
        .chain(facts.workers.map(|workers| workers.model.model))
        .chain(
            snapshot
                .workers
                .into_iter()
                .filter_map(|worker| worker.model),
        )
        .collect();
    let mut names: Vec<String> = details
        .skills()
        .map(|facts| facts.skills.into_iter().map(|skill| skill.name).collect())
        .unwrap_or_default();
    names.extend(
        details
            .profiles()
            .active
            .into_iter()
            .map(|found| found.profile.name),
    );
    rank(&snapshot.items, &facts.workspace, &models, &names)
}

/// The words of a transcript (`items`, oldest first), a workspace, the
/// models the agents run (as the session names them) and the skills and
/// profiles they have, best first.
fn rank(items: &[Item], workspace: &Path, models: &[String], names: &[String]) -> Vec<SessionWord> {
    let mut scores = Scores {
        home: std::env::home_dir()
            .map(|home| {
                home.iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default(),
        ..Scores::default()
    };
    // The workspace's and the session's names first, so that their plain
    // mentions in the transcript count.
    if !workspace.as_os_str().is_empty() {
        for name in project_names(workspace) {
            scores.add(&name, true, PROJECT_WEIGHT);
        }
        for name in top_level_names(workspace) {
            scores.add(&name, true, TOP_LEVEL_WEIGHT);
        }
    }
    let models: BTreeSet<String> = models
        .iter()
        .map(|model| crate::models::display_name(model))
        .collect();
    for model in &models {
        // The name whole ("GPT-6.1 Sol"), and each word of it with a letter
        // ("Sol").
        scores.add(model, false, SESSION_WEIGHT);
        let words = model
            .split(' ')
            .filter(|word| word.chars().any(char::is_alphabetic));
        for word in words.filter(|word| word != model) {
            scores.add(word, false, SESSION_WEIGHT);
        }
    }
    // (Each once: a tool called often isn't said more for it.)
    let tools = items.iter().filter_map(|item| item.tool.as_deref());
    let names: BTreeSet<&str> = names.iter().map(String::as_str).chain(tools).collect();
    for name in names {
        scores.add(name, true, SESSION_WEIGHT);
    }
    let mut read = 0;
    for item in items.iter().rev() {
        if read > TRANSCRIPT_BYTES {
            break;
        }
        let task = matches!(
            item.tool.as_deref(),
            Some(
                crate::crew::START_AGENT
                    | crate::crew::MESSAGE_AGENT
                    | RUN_WORKFLOW_AGENT_TOOL_NAME
            )
        );
        // The text, and whether it's code. Command output, Arko's notices,
        // the agents' thinking and the code a call writes aren't read.
        let (text, code) = match item.kind {
            Kind::User | Kind::Text | Kind::Message | Kind::Reply | Kind::Question => {
                (&item.text, false)
            }
            // A worker's task or a message to one, whole when the line has
            // only its start.
            Kind::Call if task => (item.detail.as_ref().unwrap_or(&item.text), false),
            Kind::Call => (&item.text, true),
            _ => continue,
        };
        let weight = 0.5f64.powf(read as f64 / HALF_LIFE_BYTES);
        scores.read(text, code, weight);
        read += text.len();
    }
    scores.best()
}

/// The words found so far.
#[derive(Default)]
struct Scores<'a> {
    words: HashMap<String, Entry>,
    /// How often each lowercase word came, which tells a name from a word
    /// capitalized out of habit.
    lowercase: HashMap<&'a str, u32>,
    /// The folder names of the home folder's path, which start most paths
    /// and aren't said.
    home: Vec<String>,
}

#[derive(Default)]
struct Entry {
    score: f64,
    code: bool,
    /// It's in the list whatever its mentions say, as code by its shape or
    /// a name the workspace or the session gives.
    sure: bool,
    /// It came in code (between backticks, in a path or a call), which
    /// makes words joined by hyphens code (keep-warm) rather than English
    /// (five-minute).
    in_code: bool,
    /// Its mentions within a sentence of prose, for a capitalized word.
    within_sentence: u32,
}

impl<'a> Scores<'a> {
    /// Counts `word`, a name the workspace or the session gives, unless it
    /// can't be said.
    fn add(&mut self, word: &str, code: bool, weight: f64) {
        if !sayable(word) {
            return;
        }
        let entry = self.words.entry(word.to_string()).or_default();
        entry.score += weight;
        entry.code |= code;
        entry.sure = true;
    }

    /// Counts the words of `text` (all of it code with `code`), each
    /// mention with `weight`.
    fn read(&mut self, text: &'a str, code: bool, weight: f64) {
        // Whether the next word starts a sentence, where a capital says
        // nothing.
        let mut opens = true;
        let mut in_code = code;
        let mut start = None;
        for (at, c) in text.char_indices().chain([(text.len(), '\n')]) {
            if c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/') {
                start.get_or_insert(at);
                continue;
            }
            if let Some(from) = start.take() {
                let run = &text[from..at];
                // (A word a shortened line ends in is cut.)
                if c != '…' {
                    self.run(run, in_code, opens, weight);
                }
                // (A full stop, a list's dash, a comment's slashes.)
                opens = run.ends_with(['.', '-', '/']);
            }
            match c {
                '`' => in_code = code || !in_code,
                '\n' | '!' | '?' | ':' | ';' | '|' | '#' | '>' | '=' | '{' | '}' => opens = true,
                // Space and opening marks leave it as it was.
                '"' | '\'' | '(' | '[' | '*' | '“' | '‘' => {}
                c if c.is_whitespace() => {}
                _ => opens = false,
            }
        }
    }

    /// A run of letters, digits and `_-./`. A path counts by its file and
    /// folder names, since a path said aloud is those with "slash" between,
    /// and a dotted name by its parts (`self.stats`) unless it's a file name,
    /// a domain or a version.
    fn run(&mut self, run: &'a str, in_code: bool, opens: bool, weight: f64) {
        let run = trimmed(run);
        let in_path = run.contains('/');
        for part in run.split('/').map(trimmed) {
            if in_path && self.home.iter().any(|home| home == part) {
                continue;
            }
            let whole = part
                .rsplit_once('.')
                .is_none_or(|(_, last)| ends_file_name(last));
            if whole {
                self.word(part, in_code || in_path, opens || in_path, weight);
            } else {
                for word in part.split('.').map(trimmed) {
                    self.word(word, in_code || in_path, true, weight);
                }
            }
        }
    }

    fn word(&mut self, word: &'a str, in_code: bool, opens: bool, weight: f64) {
        let Some(shape) = shape(word) else {
            return;
        };
        if shape == Shape::Plain {
            *self.lowercase.entry(word).or_default() += 1;
            // (A plain word counts only as a name the session gives.)
            if let Some(entry) = self.words.get_mut(word) {
                entry.score += weight;
            }
            return;
        }
        let entry = self.words.entry(word.to_string()).or_default();
        entry.score += weight;
        match shape {
            Shape::Code => {
                entry.code = true;
                entry.sure = true;
            }
            Shape::Hyphenated => {
                entry.code = true;
                entry.in_code |= in_code;
            }
            Shape::Capitalized => entry.within_sentence += u32::from(!opens && !in_code),
            Shape::Plain => {}
        }
    }

    /// The [`MAX_WORDS`] best, the highest score first. A capitalized word
    /// is a name when it came within sentences at least twice, and more
    /// often than in lowercase.
    fn best(self) -> Vec<SessionWord> {
        let lowercase = self.lowercase;
        let is_name = |word: &str, entry: &Entry| {
            let lower = lowercase.get(word.to_lowercase().as_str()).copied();
            entry.within_sentence >= 2 && entry.within_sentence > lower.unwrap_or(0)
        };
        let mut words: Vec<(String, Entry)> = self
            .words
            .into_iter()
            .filter(|(word, entry)| entry.sure || entry.in_code || is_name(word, entry))
            .collect();
        words.sort_by(|(a, a_entry), (b, b_entry)| {
            b_entry
                .score
                .total_cmp(&a_entry.score)
                .then_with(|| a.cmp(b))
        });
        words
            .into_iter()
            .take(MAX_WORDS)
            .map(|(text, entry)| SessionWord {
                text,
                code: entry.code,
            })
            .collect()
    }
}

/// What a word's shape says of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A word with digits (gpt-6, r56), snake_case, a file name
    /// (Cargo.toml), CamelCase or camelCase (iOS too), or ALL-CAPS.
    Code,
    /// Lowercase words joined by hyphens, which may be code (keep-warm) or
    /// English (five-minute).
    Hyphenated,
    /// A name, or a word that starts a sentence.
    Capitalized,
    Plain,
}

/// The shape of `word`, or `None` for one that can't be said ([`sayable`])
/// or a number (10ms, 2nd, 1e-9).
fn shape(word: &str) -> Option<Shape> {
    if !sayable(word) || word.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let capitals = word.chars().filter(|c| c.is_uppercase()).count();
    let capitalized = capitals == 1 && word.starts_with(char::is_uppercase);
    Some(
        if word.chars().any(|c| c.is_ascii_digit()) || word.contains(['_', '.']) {
            Shape::Code
        } else if word.contains('-') && (capitals == 0 || capitalized) {
            Shape::Hyphenated
        } else if capitals > 0 && !capitalized {
            Shape::Code
        } else if capitalized {
            Shape::Capitalized
        } else {
            Shape::Plain
        },
    )
}

/// Whether a dotted name ending in `last` is a file name (Cargo.toml), a
/// domain (docs.rs) or a version (gpt-6.1).
fn ends_file_name(last: &str) -> bool {
    last.chars().all(|c| c.is_ascii_digit())
        || EXTENSIONS
            .iter()
            .any(|extension| extension.eq_ignore_ascii_case(last))
}

/// The endings of file names and domains.
const EXTENSIONS: &[&str] = &[
    "rs",
    "toml",
    "lock",
    "md",
    "json",
    "jsonl",
    "yaml",
    "yml",
    "xml",
    "csv",
    "tsv",
    "txt",
    "log",
    "js",
    "mjs",
    "cjs",
    "ts",
    "tsx",
    "jsx",
    "vue",
    "svelte",
    "html",
    "css",
    "scss",
    "py",
    "ipynb",
    "go",
    "java",
    "kt",
    "swift",
    "c",
    "h",
    "cc",
    "cpp",
    "hpp",
    "cs",
    "rb",
    "php",
    "lua",
    "zig",
    "sh",
    "ps1",
    "sql",
    "proto",
    "wasm",
    "env",
    "ini",
    "cfg",
    "conf",
    "png",
    "jpg",
    "jpeg",
    "gif",
    "svg",
    "webp",
    "wav",
    "mp3",
    "mp4",
    "pdf",
    "zip",
    "gz",
    "tar",
    "pack",
    "gguf",
    "safetensors",
    "onnx",
    "bin",
    "com",
    "org",
    "net",
    "io",
    "dev",
    "ai",
    "app",
    "co",
];

/// Whether `word` could be said: it has a letter, three letters or digits
/// (two with a digit, such as r5), at most [`LONGEST`] characters, and
/// doesn't look like a hash, an ID or a key.
fn sayable(word: &str) -> bool {
    let alphanumeric = word.chars().filter(|c| c.is_alphanumeric()).count();
    let digits = word.chars().filter(char::is_ascii_digit).count();
    word.chars().any(char::is_alphabetic)
        && (alphanumeric >= 3 || (alphanumeric == 2 && digits > 0))
        && word.chars().count() <= LONGEST
        // Dates, times and IDs such as 20261029-184736.
        && digits < 6
        && !word.split(|c: char| !c.is_alphanumeric()).any(|part| {
            let mixed = part.chars().any(|c| c.is_ascii_digit())
                && part.chars().any(char::is_alphabetic);
            // A commit or a UUID's part, or a key.
            mixed
                && ((part.len() >= 7 && part.chars().all(|c| c.is_ascii_hexdigit()))
                    || part.len() >= 16)
        })
}

/// `word` without the punctuation at its ends (a sentence's full stop, a
/// path's leading slash, Markdown's underscores).
fn trimmed(word: &str) -> &str {
    word.trim_matches(['.', '-', '/', '_'])
}

/// The workspace's folder name and the package names its manifests give
/// (Cargo.toml, package.json, pyproject.toml, go.mod).
// shortcut: the top-level manifests only, not a Cargo workspace's members;
// read them too if members' names turn out to be said.
fn project_names(workspace: &Path) -> Vec<String> {
    let read = |file: &str| std::fs::read_to_string(workspace.join(file)).ok();
    let mut names: Vec<String> = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .into_iter()
        .collect();
    names.extend(read("Cargo.toml").and_then(|text| toml_name(&text, &["package"])));
    names.extend(
        read("pyproject.toml").and_then(|text| toml_name(&text, &["project", "tool.poetry"])),
    );
    names.extend(read("package.json").and_then(|text| {
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        // (@scope/name is said as its name.)
        let name = value["name"].as_str()?.rsplit('/').next()?;
        Some(name.to_string())
    }));
    names.extend(read("go.mod").and_then(|text| {
        let module = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("module "))?;
        Some(module.trim().rsplit('/').next()?.to_string())
    }));
    names
}

/// The `name` in the first of `tables` that has one, in a TOML file read
/// line by line.
fn toml_name(text: &str, tables: &[&str]) -> Option<String> {
    let mut table = "";
    for line in text.lines().map(str::trim) {
        if let Some(header) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            table = header.trim();
        } else if tables.contains(&table)
            && let Some(value) = line.strip_prefix("name")
            && let Some(value) = value.trim_start().strip_prefix('=')
        {
            let value = value.trim();
            let quote = value.chars().next().filter(|c| matches!(c, '"' | '\''))?;
            let name = value[1..].split(quote).next()?;
            return (!name.is_empty()).then(|| name.to_string());
        }
    }
    None
}

/// The names of the workspace's top-level files and folders, leaving out
/// hidden ones and those its ignore files leave out (build output).
fn top_level_names(workspace: &Path) -> Vec<String> {
    ignore::WalkBuilder::new(workspace)
        .max_depth(Some(1))
        .git_global(false)
        .require_git(false)
        .build()
        .flatten()
        .filter(|entry| entry.depth() == 1)
        .take(TOP_LEVEL_ENTRIES)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(words: &[SessionWord]) -> Vec<&str> {
        words.iter().map(|word| word.text.as_str()).collect()
    }

    /// The words of `text` as one message, with no workspace or session.
    fn of(text: &str) -> Vec<String> {
        let items = [Item::new(Kind::User, text)];
        texts(&rank(&items, Path::new(""), &[], &[]))
            .into_iter()
            .map(String::from)
            .collect()
    }

    #[test]
    fn identifiers_are_picked_by_their_shape() {
        let found = of(
            "Rename DictationOptions and camelCase in iOS code, then read_file and \
             `arko-tui` with gpt-6.1 and r56. The API and JSON are fine. See Cargo.toml \
             and src/dictation/service_tests.rs, then node.js, self.stats and \
             self.keep_warm in a five-minute run.",
        );
        for word in [
            "DictationOptions",
            "camelCase",
            "iOS",
            "read_file",
            "arko-tui",
            "gpt-6.1",
            "r56",
            "API",
            "JSON",
            "Cargo.toml",
            "service_tests.rs",
            "node.js",
            "keep_warm",
        ] {
            assert!(found.iter().any(|found| found == word), "{word}: {found:?}");
        }
        // A path and code count by their parts, and plain words and English
        // joined by hyphens aren't special.
        for word in [
            "src/dictation/service_tests.rs",
            "src",
            "dictation",
            "self.stats",
            "self.keep_warm",
            "stats",
            "five-minute",
            "Rename",
            "code",
            "then",
            "The",
            "fine",
        ] {
            assert!(
                !found.iter().any(|found| found == word),
                "{word}: {found:?}"
            );
        }
    }

    #[test]
    fn names_come_within_sentences_and_noise_doesnt() {
        let found = of(
            "Ask Telmo first. Then the Granite model runs, and Telmo spells it as \
             Granite does. Press Stop, then Stop again to stop it, stop it, stop it. \
             Commit e4d3e79 of session 20261029-184736-7e8f took 10ms on the 2nd \
             run, at 4K and 1e-9, e.g. with x2 and MB. Keys like \
             sk-Abcdefghijklmnop1234 stay out.",
        );
        // "Ask", "Then", "Press" and "Commit" start sentences, and "Stop" is
        // more often lowercase.
        assert_eq!(found, ["Granite", "Telmo", "x2"]);
    }

    #[test]
    fn paths_leave_out_the_home_folder() {
        let Some(home) = std::env::home_dir() else {
            return;
        };
        let found = of(&format!(
            "Open {}/projects/MyApp.swift now.",
            home.display()
        ));
        assert_eq!(found, ["MyApp.swift"]);
    }

    #[test]
    fn recent_and_repeated_words_rank_higher_and_the_list_is_capped() {
        let mut items = vec![Item::new(Kind::User, "OldName once.")];
        // Enough later text that the first mention fades.
        let filler: String = "plain words here ".repeat(4096);
        items.push(Item::new(Kind::Reply, filler));
        items.push(Item::new(
            Kind::Reply,
            "RepeatedName and RepeatedName, NewName.",
        ));
        let ranked = texts(&rank(&items, Path::new(""), &[], &[])).join(" ");
        assert_eq!(ranked, "RepeatedName NewName OldName");

        let many: String = (0..MAX_WORDS + 50).map(|n| format!("Word{n}x ")).collect();
        let items = [Item::new(Kind::User, many)];
        assert_eq!(rank(&items, Path::new(""), &[], &[]).len(), MAX_WORDS);
    }

    #[test]
    fn output_thinking_and_written_code_are_left_out() {
        let call = |tool: &str, text: &str, detail: &str| {
            let mut item = Item::new(Kind::Call, text).detail(detail);
            item.tool = Some(tool.into());
            item
        };
        let items = [
            Item::new(Kind::Output, "OutputWord"),
            Item::new(Kind::Thinking, "ThinkingWord"),
            call("shell", "cargo test -p arko-tui", ""),
            call("shell", "cat src/long_file_na…", ""),
            call("edit_file", "edit src/lib.rs", "fn CodeWritten() {}"),
            call(
                crate::crew::START_AGENT,
                "start worker “Fix it”",
                "Fix TaskWord now.",
            ),
        ];
        let words = rank(&items, Path::new(""), &[], &[]);
        let found = texts(&words);
        for word in ["arko-tui", "lib.rs", "TaskWord", "shell", "edit_file"] {
            assert!(found.contains(&word), "{word}: {found:?}");
        }
        for word in ["OutputWord", "ThinkingWord", "CodeWritten", "long_file_na"] {
            assert!(!found.contains(&word), "{word}: {found:?}");
        }
        assert!(words.iter().all(|word| word.code));
    }

    #[test]
    fn the_workspace_and_the_session_give_their_names() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("notate");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::create_dir_all(workspace.join("target")).unwrap();
        std::fs::create_dir_all(workspace.join(".hidden")).unwrap();
        std::fs::write(workspace.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nname = \"not-this\"\n\n[package]\nname = \"telmo\" # the crate\nversion = \"0.3.0\"\n\n[[bin]]\nname = \"other\"\n",
        )
        .unwrap();
        std::fs::write(
            workspace.join("package.json"),
            r#"{"name": "@mesa/notate-web"}"#,
        )
        .unwrap();
        let words = rank(
            &[Item::new(
                Kind::User,
                "Ask the reviewer, then the reviewer again.",
            )],
            &workspace,
            &["claude-opus-5-5".into(), "gpt-6.1-sol".into()],
            &["root-cause".into(), "reviewer".into()],
        );
        let found = texts(&words);
        // A name the session gives counts its plain mentions too. Then come
        // the project's names, and the rest by name at equal scores.
        assert_eq!(found[..4], ["reviewer", "notate", "notate-web", "telmo"]);
        for word in [
            "src",
            "Cargo.toml",
            "package.json",
            "Opus 5.5",
            "Opus",
            "GPT-6.1 Sol",
            "Sol",
            "root-cause",
            "reviewer",
        ] {
            assert!(found.contains(&word), "{word}: {found:?}");
        }
        // Hidden and ignored files, other tables' names, a version alone.
        for word in [
            "target",
            ".hidden",
            ".gitignore",
            "not-this",
            "other",
            "5.5",
        ] {
            assert!(!found.contains(&word), "{word}: {found:?}");
        }
        let model = words.iter().find(|word| word.text == "Sol").unwrap();
        assert!(!model.code);
    }

    /// Prints the list of a recorded session, with how long building it
    /// took. `ARKO_SESSION_WORDS` names the session's folder, whose
    /// `run/events.jsonl` is replayed into a feed and whose `session.json`
    /// gives the workspace and the models.
    #[test]
    #[ignore = "needs a recorded session"]
    fn a_recorded_sessions_words() {
        let Some(dir) = std::env::var_os("ARKO_SESSION_WORDS").map(std::path::PathBuf::from) else {
            return;
        };
        let read = |file: &str| std::fs::read_to_string(dir.join(file)).unwrap();
        let session: serde_json::Value = serde_json::from_str(&read("session.json")).unwrap();
        let feed = Feed::new(crate::remote::MAIN_AGENT);
        for line in read("run/events.jsonl").lines() {
            let Ok(event) = serde_json::from_str::<arko_workflows::WorkflowEvent>(line) else {
                continue;
            };
            let worker = event.agent_run_id.to_string();
            let label = (event.agent_name != crate::remote::MAIN_AGENT).then_some(worker.as_str());
            feed.event(&event, label);
        }
        let workspace = Path::new(session["workspace"].as_str().unwrap());
        let models: Vec<String> = ["main", "worker"]
            .iter()
            .filter_map(|role| session[role]["model"].as_str().map(String::from))
            .collect();
        // (Arko's folder holds `sessions/<id>`.)
        let root = dir.parent().and_then(Path::parent).unwrap().to_path_buf();
        let folders =
            crate::skills::folders_in(Some(workspace), &root, std::env::home_dir().as_deref());
        let mut names: Vec<String> = crate::skills::scan(&folders, &Default::default(), workspace)
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        let roots = crate::profiles::Roots::new(&root)
            .workspace(workspace)
            .session(&dir);
        names.extend(
            crate::profiles::list(&roots)
                .active
                .into_iter()
                .map(|found| found.profile.name),
        );
        let started = std::time::Instant::now();
        let snapshot = feed.subscribe().0;
        let snapshot_took = started.elapsed();
        let words = rank(&snapshot.items, workspace, &models, &names);
        let took = started.elapsed();
        let transcript = std::time::Instant::now();
        rank(&snapshot.items, Path::new(""), &[], &[]);
        eprintln!(
            "{} items, snapshot {snapshot_took:?}, list {took:?}, of which the transcript {:?}",
            snapshot.items.len(),
            transcript.elapsed()
        );
        for word in &words {
            eprintln!("{}{}", word.text, if word.code { "" } else { "  (name)" });
        }
    }
}

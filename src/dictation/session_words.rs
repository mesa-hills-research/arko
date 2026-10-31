//! The words a dictation favours: those of its session that a speech model
//! would otherwise misspell, such as identifiers, file names, project names
//! and the session's models. Every front end gives a recording its list
//! with [`favour_session_words`], which builds it with [`session_words`]
//! on a thread of its own, so they all favour the same words.
//!
//! Building it reads a bounded part of everything: the newest part of the
//! transcript, the listings of the workspace and of the skill and profile
//! folders, and small manifests.
//!
//! Words that can't be said, such as keys, tokens, hashes and IDs, are
//! always left out. With the `leave_out_secrets` setting, so are the values
//! that context marks as secret ([`secrets`]).

mod secrets;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arko_workflows::services::RUN_WORKFLOW_AGENT_TOOL_NAME;

use super::Dictation;
use super::settings::Settings;
use crate::builtin_skills::BuiltinSkills;
use crate::details::Details;
use crate::profiles::Roots;
use crate::remote::{Feed, Item, Kind};
use crate::shell::SHELL_TOOL_NAME;

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
/// The entries a folder's listing visits at most (the workspace's top
/// level, a folder of skills or profiles), for a folder such as the home
/// directory.
const LISTED: usize = 200;
/// The largest manifest or ignore file read, in bytes.
const SMALL_FILE_BYTES: u64 = 64 * 1024;

/// A word to favour, in its spelling. (A spoken form, such as "engine x"
/// for nginx, would join it here.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWord {
    /// As the session writes it ("DictationOptions", "Cargo.toml", "Opus
    /// 5.5").
    pub text: String,
    /// It comes from code (an identifier, a file name, a name with digits),
    /// so it's kept even with two letters or digits.
    pub code: bool,
}

/// Gives `dictation` the words of the session `feed` and `details` describe
/// ([`session_words`]), built on a thread of their own, unless the settings
/// in Arko's folder `root` turn session words off. The recording goes on
/// meanwhile and favours them from the next utterance, or from the first
/// when they come before it. A dictation that ended or was cancelled drops
/// them.
pub fn favour_session_words(
    dictation: &Dictation,
    root: &Path,
    feed: &Feed,
    details: &Arc<Details>,
) {
    let words = dictation.words_sender();
    let (root, feed, details) = (root.to_path_buf(), feed.clone(), details.clone());
    // (Without a thread for it, the recording goes on without them.)
    let _ = std::thread::Builder::new()
        .name("arko-session-words".into())
        .spawn(move || {
            let settings = Settings::load(&root);
            if words.wanted() && settings.session_words {
                words.send(session_words(&feed, &details, &settings));
            }
        });
}

/// The words of the session `feed` and `details` describe, best first, at
/// most [`MAX_WORDS`], with the dictation `settings` (whether to leave out
/// secrets). They come from three places.
///
/// - The transcript's identifiers, file names and names, by their shape. A
///   recent or repeated mention counts more.
/// - The workspace's folder name, the package names in its manifests, and
///   its top-level files and folders.
/// - The session's models, skills and worker profiles, and the tools its
///   agents called.
pub fn session_words(feed: &Feed, details: &Details, settings: &Settings) -> Vec<SessionWord> {
    let facts = details.session();
    let mut tail = Tail::default();
    feed.read_newest(|item| tail.take(item));
    let models: Vec<String> = std::iter::once(facts.main.model)
        .chain(facts.workers.map(|workers| workers.model.model))
        .collect();
    let mut names = skill_names(details.skill_folders(), &details.builtin_skills());
    names.extend(profile_names(&details.profile_roots()));
    rank(
        &tail,
        &facts.workspace,
        &models,
        &names,
        settings.leave_out_secrets,
    )
}

/// The newest part of a transcript, up to [`TRANSCRIPT_BYTES`] of text,
/// and the tools its calls called.
#[derive(Default)]
struct Tail {
    texts: Vec<Text>,
    tools: BTreeSet<String>,
    /// The bytes of text taken so far.
    read: usize,
}

/// A text of the transcript, where it's from, and what a mention in it
/// counts.
struct Text {
    text: String,
    context: Context,
    weight: f64,
}

/// Where a text is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    /// A message, the agents' text, a worker's task.
    Prose,
    /// A call's line, which is code.
    Call,
    /// A shell call's command line, whose commands are code whatever their
    /// shape (`rg`, `cargo`).
    Shell,
}

impl Tail {
    /// Takes `item`, the next newest, and says whether to go on. A text
    /// that doesn't fit keeps its end from the first whole line in it, since
    /// a cut through a line can take away what tells a word.
    fn take(&mut self, item: &Item) -> bool {
        if let Some(tool) = &item.tool
            && !self.tools.contains(tool)
        {
            self.tools.insert(tool.clone());
        }
        let Some((text, context)) = readable(item) else {
            return true;
        };
        let weight = 0.5f64.powf(self.read as f64 / HALF_LIFE_BYTES);
        let left = TRANSCRIPT_BYTES - self.read;
        let kept = match text.len().checked_sub(left) {
            None | Some(0) => text,
            Some(cut) => text.as_bytes()[cut..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or("", |at| &text[cut + at + 1..]),
        };
        self.read += text.len().min(left);
        if !kept.is_empty() {
            self.texts.push(Text {
                text: kept.to_string(),
                context,
                weight,
            });
        }
        self.read < TRANSCRIPT_BYTES
    }
}

/// The text of `item` that's read, and where it's from. Command output,
/// Arko's notices, the agents' thinking and the code a call writes aren't
/// read.
fn readable(item: &Item) -> Option<(&str, Context)> {
    let task = matches!(
        item.tool.as_deref(),
        Some(crate::crew::START_AGENT | crate::crew::MESSAGE_AGENT | RUN_WORKFLOW_AGENT_TOOL_NAME)
    );
    Some(match item.kind {
        Kind::User | Kind::Text | Kind::Message | Kind::Reply | Kind::Question => {
            (&item.text, Context::Prose)
        }
        // A worker's task or a message to one, whole when the line has only
        // its start.
        Kind::Call if task => (item.detail.as_deref().unwrap_or(&item.text), Context::Prose),
        // (Without the folder it ran in, "(in src) ".)
        Kind::Call if item.tool.as_deref() == Some(SHELL_TOOL_NAME) => (
            item.text
                .strip_prefix("(in ")
                .and_then(|line| line.split_once(") "))
                .map_or(item.text.as_str(), |(_, line)| line),
            Context::Shell,
        ),
        Kind::Call => (&item.text, Context::Call),
        _ => return None,
    })
}

/// The words of a transcript's tail, a workspace, the models the agents run
/// (as the session names them) and the skills and profiles they have, best
/// first, leaving out what context marks as secret with `secrets`.
fn rank(
    tail: &Tail,
    workspace: &Path,
    models: &[String],
    names: &[String],
    secrets: bool,
) -> Vec<SessionWord> {
    let mut scores = Scores {
        secrets,
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
    let names: BTreeSet<&str> = names
        .iter()
        .chain(&tail.tools)
        .map(String::as_str)
        .collect();
    for name in names {
        scores.add(name, true, SESSION_WEIGHT);
    }
    for text in &tail.texts {
        scores.read(&text.text, text.context, text.weight);
    }
    // (Each command once, as the tools.)
    for command in std::mem::take(&mut scores.commands) {
        scores.add(command, true, SESSION_WEIGHT);
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
    /// Leave out what context marks as secret.
    secrets: bool,
    /// The commands the shell lines ran.
    commands: BTreeSet<&'a str>,
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
    /// can't be said, or marks a secret when those are left out.
    fn add(&mut self, word: &str, code: bool, weight: f64) {
        if !sayable(word) || (self.secrets && !secrets::secret_ranges(word).is_empty()) {
            return;
        }
        let entry = self.words.entry(word.to_string()).or_default();
        entry.score += weight;
        entry.code |= code;
        entry.sure = true;
    }

    /// Counts the words of `text`, from `context`, each mention with
    /// `weight`. What context marks as secret takes no part when those are
    /// left out.
    fn read(&mut self, text: &'a str, context: Context, weight: f64) {
        let hidden = if self.secrets {
            secrets::secret_ranges(text)
        } else {
            Vec::new()
        };
        let mut hidden = hidden.into_iter().peekable();
        let (code, shell) = (context != Context::Prose, context == Context::Shell);
        // Whether the next word starts a sentence, where a capital says
        // nothing, and whether it's the command a shell line runs.
        let (mut opens, mut in_code, mut command) = (true, code, shell);
        // Where the word under way starts, and what comes before it.
        let (mut start, mut before, mut previous) = (None, '\n', '\n');
        for (at, c) in text.char_indices().chain([(text.len(), '\n')]) {
            while hidden.next_if(|range| range.end <= at).is_some() {}
            let secret = hidden.peek().is_some_and(|range| range.start <= at);
            if !secret && (c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')) {
                if start.is_none() {
                    start = Some(at);
                    before = previous;
                }
                previous = c;
                continue;
            }
            if let Some(from) = start.take() {
                let run = &text[from..at];
                if command && !run.contains('/') {
                    self.commands.insert(trimmed(run));
                // (A word a shortened line ends in is cut.)
                } else if c != '…' {
                    let alone = before == '`' && c == '`';
                    self.run(run, in_code, opens, weight, alone);
                }
                command = false;
                // (A full stop, a list's dash, a comment's slashes.)
                opens = run.ends_with(['.', '-', '/']);
            }
            previous = c;
            match c {
                _ if secret && c != '`' => {}
                '`' => in_code = code || !in_code,
                '|' | '&' | ';' if shell => {
                    command = true;
                    opens = true;
                }
                '\n' | '!' | '?' | ':' | ';' | '|' | '#' | '>' | '=' | '{' | '}' => opens = true,
                // Space and opening marks leave it as it was.
                '"' | '\'' | '(' | '[' | '*' | '“' | '‘' => {}
                c if c.is_whitespace() => {}
                _ => opens = false,
            }
        }
    }

    /// A run of letters, digits and `_-./`, `alone` between backticks. A
    /// path counts by its file and folder names, since a path said aloud is
    /// those with "slash" between, and a dotted name by its parts
    /// (`self.stats`) unless it's a file name, a domain or a version.
    fn run(&mut self, run: &'a str, in_code: bool, opens: bool, weight: f64, alone: bool) {
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
                let alone = alone && !in_path;
                self.word(part, in_code || in_path, opens || in_path, weight, alone);
            } else {
                for word in part.split('.').map(trimmed) {
                    self.word(word, in_code || in_path, true, weight, false);
                }
            }
        }
    }

    /// Counts `word`. A word alone between backticks (`alone`) is code
    /// whatever its shape. Without that, a plain word or one of two letters
    /// (`UI`, `rg`) counts only toward a name the workspace or the session
    /// gives.
    fn word(&mut self, word: &'a str, in_code: bool, opens: bool, weight: f64, alone: bool) {
        let Some(shape) = shape(word) else {
            return;
        };
        if shape == Shape::Plain {
            *self.lowercase.entry(word).or_default() += 1;
        }
        let short = word.chars().filter(|c| c.is_alphanumeric()).count() < 3
            && !word.chars().any(|c| c.is_ascii_digit());
        if !alone && (shape == Shape::Plain || short) {
            if let Some(entry) = self.words.get_mut(word) {
                entry.score += weight;
            }
            return;
        }
        let entry = self.words.entry(word.to_string()).or_default();
        entry.score += weight;
        if alone {
            entry.code = true;
            entry.sure = true;
            return;
        }
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

/// Whether `word` could be said: it has a letter, two letters or digits or
/// more, at most [`LONGEST`] characters, and nothing [`hard_to_say`].
fn sayable(word: &str) -> bool {
    word.chars().any(char::is_alphabetic)
        && word.chars().filter(|c| c.is_alphanumeric()).count() >= 2
        && word.chars().count() <= LONGEST
        && !hard_to_say(word)
}

/// Whether `word` holds a part (between characters other than letters and
/// digits) that can't be said, being a key, a token, a hash, a UUID or an
/// ID rather than a name: one of 32 characters or more, letters and digits
/// at random (16 or more, hexadecimal of 7 or more, or switching between
/// letters and digits three times in 8 or more), a number of 6 digits or
/// more, letters in mixed case at random, or letters with no vowel pattern
/// a word has ([`unpronounceable`]).
fn hard_to_say(word: &str) -> bool {
    word.split(|c: char| !c.is_alphanumeric()).any(|part| {
        let length = part.chars().count();
        let digits = part.chars().filter(char::is_ascii_digit).count();
        if length >= 32 {
            true
        } else if digits == 0 {
            random_case(part) || camel_words(part).into_iter().any(unpronounceable)
        } else if digits == length {
            length >= 6
        } else {
            length >= 16
                || (length >= 7 && part.chars().all(|c| c.is_ascii_hexdigit()))
                || (length >= 8 && alternations(part) >= 3)
        }
    })
}

/// The words a CamelCase or camelCase part holds: `XMLHttpRequest` holds
/// `XML`, `Http` and `Request`, and `Win32Error` `Win32` and `Error`.
fn camel_words(part: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = part.char_indices().collect();
    let mut starts = vec![0];
    for (i, &(at, c)) in chars.iter().enumerate().skip(1) {
        let previous = chars[i - 1].1;
        let next_lower = chars
            .get(i + 1)
            .is_some_and(|(_, next)| next.is_lowercase());
        if c.is_uppercase()
            && (previous.is_lowercase()
                || previous.is_ascii_digit()
                || (previous.is_uppercase() && next_lower))
        {
            starts.push(at);
        }
    }
    starts.push(part.len());
    starts
        .windows(2)
        .map(|pair| &part[pair[0]..pair[1]])
        .filter(|word| !word.is_empty())
        .collect()
}

/// Letters in mixed case at random, as in a key (`xKqzPwRtmB`): ten or
/// more, in three or more camel words averaging under two and a half
/// letters.
fn random_case(part: &str) -> bool {
    let words = camel_words(part).len();
    part.len() >= 10 && words >= 3 && part.len() * 2 < words * 5
}

/// A word of eight letters or more (of the English alphabet) with no vowel
/// pattern a word has: under one in five a vowel (with y), or seven
/// consonants in a row.
fn unpronounceable(word: &str) -> bool {
    if word.len() < 8 || !word.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    let vowel = |c: &u8| b"aeiouy".contains(&c.to_ascii_lowercase());
    let letters = word.as_bytes();
    let vowels = letters.iter().filter(|c| vowel(c)).count();
    let consonants = letters.split(vowel).map(<[u8]>::len).max().unwrap_or(0);
    vowels * 5 < letters.len() || consonants >= 7
}

/// How often `part` switches between letters and digits.
fn alternations(part: &str) -> usize {
    let digits: Vec<bool> = part.chars().map(|c| c.is_ascii_digit()).collect();
    digits.windows(2).filter(|pair| pair[0] != pair[1]).count()
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
    let read = |file: &str| read_small(&workspace.join(file));
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

/// The text of `path` when it's a regular file of at most
/// [`SMALL_FILE_BYTES`], opened without following a symbolic link or
/// waiting on a FIFO.
// shortcut: elsewhere than on Unix a link is followed; open it without
// following there too if a manifest turns out to be one.
fn read_small(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NONBLOCK | libc::O_NOFOLLOW,
    );
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > SMALL_FILE_BYTES {
        return None;
    }
    let mut text = String::new();
    file.take(SMALL_FILE_BYTES).read_to_string(&mut text).ok()?;
    Some(text)
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

/// The entries of `folder`, of the first [`LISTED`] it lists.
fn listed(folder: &Path) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(folder)
        .map(|entries| entries.take(LISTED).flatten().collect())
        .unwrap_or_default()
}

/// The names of the workspace's top-level files and folders, leaving out
/// hidden ones and those its `.gitignore` leaves out (build output).
// shortcut: the workspace's own .gitignore, not those above it or git's
// other ignore files; read those too if their entries turn out to show.
fn top_level_names(workspace: &Path) -> Vec<String> {
    let mut rules = ignore::gitignore::GitignoreBuilder::new(workspace);
    for line in read_small(&workspace.join(".gitignore"))
        .unwrap_or_default()
        .lines()
    {
        let _ = rules.add_line(None, line);
    }
    let ignored = rules
        .build()
        .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty());
    listed(workspace)
        .into_iter()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let folder = entry.file_type().is_ok_and(|kind| kind.is_dir());
            let shown =
                !name.starts_with('.') && !ignored.matched(entry.path(), folder).is_ignore();
            shown.then_some(name)
        })
        .collect()
}

/// The names of the skills in `folders` and of the built-in ones `builtin`
/// gives, without reading the skills: each folder's subfolders that hold a
/// `SKILL.md`, by name.
// shortcut: a skill goes by its folder's name here, though its SKILL.md may
// name it otherwise; read the names once arko_agents can list skills
// without loading them.
fn skill_names(folders: &[PathBuf], builtin: &BuiltinSkills) -> Vec<String> {
    let mut names: Vec<String> = builtin
        .given()
        .map(|skill| skill.name.to_string())
        .collect();
    for folder in folders {
        names.extend(
            listed(folder)
                .into_iter()
                .filter(|entry| entry.path().join("SKILL.md").is_file())
                .map(|entry| entry.file_name().to_string_lossy().into_owned()),
        );
    }
    names
}

/// The names of the worker profiles `roots` has, from their folders'
/// listings: each `.md` file's name without it.
fn profile_names(roots: &Roots) -> Vec<String> {
    crate::profiles::folders(roots)
        .into_iter()
        .flat_map(|(_, folder)| listed(&folder))
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".md").map(String::from)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tail of `items`, oldest first, as a feed gives it.
    fn tail(items: &[Item]) -> Tail {
        let mut tail = Tail::default();
        for item in items.iter().rev() {
            if !tail.take(item) {
                break;
            }
        }
        tail
    }

    fn texts(words: &[SessionWord]) -> Vec<&str> {
        words.iter().map(|word| word.text.as_str()).collect()
    }

    /// The words of `items`, with no workspace or session.
    fn words_of(items: &[Item]) -> Vec<SessionWord> {
        rank(&tail(items), Path::new(""), &[], &[], false)
    }

    /// The words of `text` as one message, with no workspace or session.
    fn of(text: &str) -> Vec<String> {
        texts(&words_of(&[Item::new(Kind::User, text)]))
            .into_iter()
            .map(String::from)
            .collect()
    }

    fn call(tool: &str, text: &str, detail: &str) -> Item {
        let mut item = Item::new(Kind::Call, text).detail(detail);
        item.tool = Some(tool.into());
        item
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
             run, at 4K and 1e-9, e.g. with x2 and MB.",
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
        let ranked = texts(&words_of(&items)).join(" ");
        assert_eq!(ranked, "RepeatedName NewName OldName");

        let many: String = (0..MAX_WORDS + 50).map(|n| format!("Word{n}x ")).collect();
        assert_eq!(words_of(&[Item::new(Kind::User, many)]).len(), MAX_WORDS);
    }

    #[test]
    fn output_thinking_and_written_code_are_left_out() {
        let items = [
            Item::new(Kind::Output, "OutputWord"),
            Item::new(Kind::Thinking, "ThinkingWord"),
            call("shell", "cargo build --package arko-tui", ""),
            call("shell", "cat src/long_file_na…", ""),
            call("edit_file", "edit src/lib.rs", "fn CodeWritten() {}"),
            call(
                crate::crew::START_AGENT,
                "start worker “Fix it”",
                "Fix TaskWord now.",
            ),
        ];
        let words = words_of(&items);
        let found = texts(&words);
        for word in ["arko-tui", "lib.rs", "TaskWord", "shell", "edit_file"] {
            assert!(found.contains(&word), "{word}: {found:?}");
        }
        for word in ["OutputWord", "ThinkingWord", "CodeWritten", "long_file_na"] {
            assert!(!found.contains(&word), "{word}: {found:?}");
        }
        assert!(words.iter().all(|word| word.code));
    }

    /// A word of two letters counts as code alone between backticks, as the
    /// command a shell line runs or as a name the session gives, and a short
    /// word of prose doesn't.
    #[test]
    fn short_words_need_code_or_the_sessions_names() {
        let items = [
            Item::new(
                Kind::User,
                "Search with `rg` in the UI, not AI. Open `UI` and go to it, it is ok.",
            ),
            call("shell", "(in src) cd web && rg -n Foo | head -3", ""),
        ];
        let words = rank(&tail(&items), Path::new(""), &[], &["qa".into()], false);
        let found = texts(&words);
        for word in ["rg", "UI", "cd", "head", "qa"] {
            assert!(found.contains(&word), "{word}: {found:?}");
        }
        for word in [
            "AI", "go", "it", "is", "ok", "in", "to", "web", "src", "Foo", "Search", "Open",
        ] {
            assert!(!found.contains(&word), "{word}: {found:?}");
        }
        assert!(words.iter().all(|word| word.code));
    }

    /// Keys, tokens, hashes and IDs can't be said, and names and
    /// identifiers can, however unusual.
    #[test]
    fn what_cant_be_said_is_left_out() {
        for word in [
            "DictationOptions",
            "snake_case_name",
            "Nginx",
            "kubectl",
            "gpt-6.1",
            "x86_64",
            "sha256",
            "utf8",
            "i18n",
            "k8s",
            "IOError",
            "XMLHttpRequest",
            "HTTPSConnectionPool",
            "iPhoneXSMax",
            "Win32Error",
            "Fauchelevent",
            "Nyiragongo",
            "Kowalczyk",
            "Wojciechowski",
            "Mkrtchyan",
            "ratatui",
            "pthreads",
            "llvmpipe",
            "systemctl",
            "h264",
            "base64",
            "python3",
            "md5sum",
            "iOS",
            "r56",
            "studio016-gates3",
            "AWS_SECRET_ACCESS_KEY",
            "Cargo.toml",
            "e2e",
        ] {
            assert!(sayable(word), "{word}");
        }
        for word in [
            "sk-ant-api03-Xq7Lm2Pw9Rt4Vb8Nc1Zd5Hj3Kf6Gs0Ya",
            "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8",
            "AKIAIOSFODNN7EXAMPLE",
            "e4d3e79",
            "96ebe7e88a63e5b129b73b5e5829e4bc1cf843ab",
            "550e8400-e29b-41d4-a716-446655440000",
            "build-20261029",
            "a1b2c3d4",
            "AbCdEfGhIjKl",
            "xKqzPwRtmBvLnJ",
            "qzxwvkpjrtmn",
            "eyJhbGciOiJIUzI1NiJ9",
            "dGhpcyBpcyBhIHRlc3Q",
        ] {
            assert!(!sayable(word), "{word}");
        }
    }

    /// Values that context marks as secret count unless `leave_out_secrets`
    /// is on, in prose, in a call's line and in a shell command, and the
    /// names next to them count either way.
    #[test]
    fn marked_secrets_are_left_out_only_when_asked() {
        let items = [
            Item::new(
                Kind::User,
                "Set PASSWORD=HunterTwo and API_KEY: Sk9Zz, then mail Jane.Doe@Example.com \
                 about ReadMe.md and password_hash.",
            ),
            call(
                "http_get",
                "fetch https://Svc.example/a?api_key=HunterNine",
                "",
            ),
            call(
                "shell",
                "curl -H 'Authorization: Bearer HunterTen' -u root:HunterEleven https://x.example/y \
                 && sshpass -p HunterThirteen ssh Host9 \
                 && tool --password-file ~/KeptFolder/pw.txt --password HunterFourteen",
                "",
            ),
        ];
        let words = |secrets| {
            texts(&rank(&tail(&items), Path::new(""), &[], &[], secrets))
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        };
        let (kept, left_out) = (words(false), words(true));
        let secret = |word: &String| {
            ["Hunter", "Sk9Zz", "Jane", "Doe", "Example.com"]
                .iter()
                .any(|part| word.contains(part))
        };
        assert!(kept.iter().any(secret), "{kept:?}");
        assert!(!left_out.iter().any(secret), "{left_out:?}");
        for word in [
            "PASSWORD",
            "API_KEY",
            "ReadMe.md",
            "password_hash",
            "KeptFolder",
            "pw.txt",
            "Host9",
        ] {
            assert!(
                left_out.iter().any(|found| found == word),
                "{word}: {left_out:?}"
            );
        }
    }

    /// The tail stops once its budget is full, and of the text that doesn't
    /// fit it keeps the whole lines after the cut.
    #[test]
    fn the_tail_keeps_whole_lines_within_its_budget() {
        let newest = Item::new(Kind::Reply, "w ".repeat((TRANSCRIPT_BYTES - 24) / 2));
        let cut = Item::new(Kind::User, "FirstLine CutWord éé\nSeenWord");
        let oldest = Item::new(Kind::User, "OldestWord");
        let tail = tail(&[oldest, cut, newest]);
        assert_eq!(tail.read, TRANSCRIPT_BYTES);
        assert_eq!(tail.texts.len(), 2);
        assert_eq!(tail.texts[1].text, "SeenWord");
        let found = texts(&rank(&tail, Path::new(""), &[], &[], false))
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        assert_eq!(found, ["SeenWord"]);
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
            &tail(&[Item::new(
                Kind::User,
                "Ask the reviewer, then the reviewer again.",
            )]),
            &workspace,
            &["claude-opus-5-5".into(), "gpt-6.1-sol".into()],
            &["root-cause".into(), "reviewer".into()],
            false,
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

    /// A manifest is read only when it's a small regular file: a link isn't
    /// followed, a FIFO isn't waited on and a large file isn't read.
    #[cfg(unix)]
    #[test]
    fn manifests_are_read_only_when_small_and_regular() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let elsewhere = temp.path().join("linked.toml");
        std::fs::write(&elsewhere, "[package]\nname = \"linked\"\n").unwrap();
        std::os::unix::fs::symlink(&elsewhere, workspace.join("Cargo.toml")).unwrap();
        let fifo = std::ffi::CString::new(
            workspace
                .join("package.json")
                .into_os_string()
                .into_encoded_bytes(),
        )
        .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let large = format!(
            "[project]\nname = \"large\"\n{}",
            "# padding\n".repeat(8 * 1024)
        );
        std::fs::write(workspace.join("pyproject.toml"), large).unwrap();
        std::fs::write(workspace.join("go.mod"), "module example.com/org/gomod\n").unwrap();
        assert_eq!(project_names(&workspace), ["project", "gomod"]);
    }

    #[test]
    fn listings_stop_at_their_limit() {
        let temp = tempfile::tempdir().unwrap();
        for n in 0..LISTED + 30 {
            std::fs::write(temp.path().join(format!("file{n}.txt")), "").unwrap();
        }
        assert_eq!(top_level_names(temp.path()).len(), LISTED);
    }

    #[test]
    fn skills_and_profiles_go_by_their_files_names() {
        let temp = tempfile::tempdir().unwrap();
        let skills = temp.path().join("skills");
        std::fs::create_dir_all(skills.join("alpha")).unwrap();
        std::fs::write(skills.join("alpha/SKILL.md"), "---\nname: alpha\n---\n").unwrap();
        std::fs::create_dir_all(skills.join("notes")).unwrap();
        std::fs::write(skills.join("readme.md"), "").unwrap();
        let none = BuiltinSkills {
            none: true,
            off: Vec::new(),
        };
        assert_eq!(skill_names(&[skills], &none), ["alpha"]);
        assert!(skill_names(&[], &BuiltinSkills::default()).contains(&"ponytail".to_string()));

        let (root, workspace) = (temp.path().join("arko"), temp.path().join("project"));
        std::fs::create_dir_all(root.join("profiles")).unwrap();
        std::fs::write(root.join("profiles/reviewer.md"), "").unwrap();
        std::fs::write(root.join("profiles/notes.txt"), "").unwrap();
        std::fs::create_dir_all(workspace.join(".arko/profiles")).unwrap();
        std::fs::write(workspace.join(".arko/profiles/impl-light.md"), "").unwrap();
        let mut profiles = profile_names(&Roots::new(&root).workspace(&workspace));
        profiles.sort();
        assert_eq!(profiles, ["impl-light", "reviewer"]);
    }

    /// Counts the list of a recorded session by kind, with how long each
    /// step took, and prints no word. `ARKO_SESSION_WORDS` names the
    /// session's folder, whose `run/events.jsonl` is replayed into a feed
    /// and whose `session.json` gives the workspace and the models.
    #[test]
    #[ignore = "needs a recorded session"]
    fn a_recorded_sessions_words() {
        use std::time::Instant;
        let Some(dir) = std::env::var_os("ARKO_SESSION_WORDS").map(PathBuf::from) else {
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
        let started = Instant::now();
        let mut tail = Tail::default();
        feed.read_newest(|item| tail.take(item));
        let tail_took = started.elapsed();
        let names_started = Instant::now();
        let folders =
            crate::skills::folders_in(Some(workspace), &root, std::env::home_dir().as_deref());
        let mut names = skill_names(&folders, &BuiltinSkills::default());
        names.extend(profile_names(
            &Roots::new(&root).workspace(workspace).session(&dir),
        ));
        let names_took = names_started.elapsed();
        let rank_started = Instant::now();
        let words = rank(&tail, workspace, &models, &names, false);
        let rank_took = rank_started.elapsed();
        let mut kinds: std::collections::BTreeMap<&str, usize> = Default::default();
        for word in &words {
            let kind = match shape(&word.text) {
                _ if word.text.contains(' ') => "several words",
                _ if word.text.chars().filter(|c| c.is_alphanumeric()).count() < 3 => "short",
                Some(Shape::Code) => "code",
                Some(Shape::Hyphenated) => "hyphenated",
                Some(Shape::Capitalized) => "capitalized",
                Some(Shape::Plain) => "plain",
                None => "other",
            };
            *kinds.entry(kind).or_default() += 1;
        }
        eprintln!(
            "{} texts ({} bytes), {} tools, {} skill and profile names; {} words, {} added as code; by kind {kinds:?}",
            tail.texts.len(),
            tail.read,
            tail.tools.len(),
            names.len(),
            words.len(),
            words.iter().filter(|word| word.code).count(),
        );
        eprintln!(
            "took {:?} in all: tail {tail_took:?}, names {names_took:?}, workspace and ranking {rank_took:?}",
            started.elapsed()
        );
    }
}

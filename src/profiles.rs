//! Worker profiles, the named setups the agent starts workers with. A
//! profile is a Markdown file with a few `key: value` lines in front, then
//! the standing instructions every worker started with it gets.
//!
//! ```text
//! ---
//! description: Judges 3D results against a reference image
//! model: sol
//! effort: high
//! ---
//! You judge 3D models. Compare the render with the reference image on
//! shape, proportions and materials.
//! ```
//!
//! The profile's name is its file's name without `.md`. A profile lives in
//! one of four places, and the nearest one wins when a name is in more
//! than one. The session's own folder (`profiles` in the session's folder)
//! beats the project's (`.arko/profiles` in the working directory), the
//! project's beats the repository's (`.arko/profiles` in the root of the
//! git repository the working directory is in, when that is another folder),
//! and the repository's beats the user's (`profiles` in Arko's folder).
//! [`Preferences`] turns the repository's folder off, and keeps the
//! project's `.arko` folder, when Arko makes it, out of git
//! ([`arko_dir`](crate::arko_dir)).
//!
//! A save writes `.arko/profiles` only. The project and the repository root
//! are also read in `.agents/profiles`, which earlier versions wrote and
//! other tools read, after the `.arko/profiles` of the same place. Arko
//! never moves a file there. A save that replaces a profile in it writes the
//! new file in `.arko/profiles` and deletes the old one.
//!
//! This module reads and writes the files without a session, as
//! [`extensions`](crate::extensions) and [`accounts`](crate::accounts) do
//! for their settings, so an app's settings page and a running session see
//! the same profiles. A session reads the files each time it starts a
//! worker or lists the profiles, so an edit reaches the next worker with no
//! restart. All of it reads and writes files, so call it off a UI thread.
//!
//! The keys of the front matter are the following. Only `description` is
//! required.
//!
//! | Key | Meaning |
//! |---|---|
//! | `description` | One line of up to 120 characters, shown in lists |
//! | `model` | A model as `/model` takes it. Without one, the session's worker model |
//! | `effort` | `low`, `medium`, `high`, `xhigh` or `max`, checked against the model |
//! | `fast` | `true` for fast mode where the model has it. Off by default |
//! | `cache` | `5m` or `1h`, the prompt cache of Claude workers on an API key, on OpenRouter or in Claude Code |
//! | `nested` | `true` lets the worker start workers of its own |
//! | `skills` | Skill names separated by commas, or `none`. Workers with the profile get only these, as far as the agent starting them has them |
//! | `mcp` | MCP server names, the same way |
//! | `made_by` | `agent` when an agent wrote the profile |
//!
//! A name is lowercase letters, digits and hyphens, up to 32 characters,
//! starting with a letter or a digit. `default`, `main`, `worker`, `lead` and
//! `all` are reserved. A file over 16 KB, or one that can't be read as a
//! profile, is left out of the list, shown among the profiles that can't be
//! read, and hides no other. Windows line endings and a byte order mark are
//! read as plain text. A key this version doesn't know is kept when a
//! profile is saved again, so a file written for a newer Arko survives an
//! older one. Comment lines (starting with `#`) in the front matter are read
//! and not kept.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agents::Effort;
use crate::providers;

/// The longest profile name, in characters.
pub const MAX_NAME: usize = 32;
/// The longest description, in characters.
pub const MAX_DESCRIPTION: usize = 120;
/// The longest instructions, in characters.
pub const MAX_INSTRUCTIONS: usize = 4_000;
/// The longest model, in characters. (Any `openrouter:<slug>` names a model,
/// so the length is the only limit on a slug.)
pub const MAX_MODEL: usize = 100;
/// The largest profile file Arko reads, in bytes.
pub const MAX_FILE_BYTES: u64 = 16 * 1024;
/// Names a profile can't take, because they mean something else to the
/// agents or the commands.
pub const RESERVED: [&str; 5] = ["default", "main", "worker", "lead", "all"];
/// The `made_by` value of a profile an agent wrote.
pub const MADE_BY_AGENT: &str = "agent";

/// The folder in the project (the working directory) and in a repository's
/// root that profiles are saved in.
const PROJECT_FOLDER: [&str; 2] = [crate::arko_dir::DIR, "profiles"];
/// The folder in the project and in a repository's root that earlier versions
/// saved profiles in. It is still read.
const OLDER_PROJECT_FOLDER: [&str; 2] = [".agents", "profiles"];
/// The folder in Arko's folder and in a session's folder.
const FOLDER: &str = "profiles";
const EXTENSION: &str = "md";

/// Where a profile lives. The places are listed nearest first, and the
/// nearest wins when a name is in more than one. They sort in that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Place {
    /// The session's own folder. An agent saves here without asking.
    Session,
    /// The project's `.arko/profiles`, kept with the workspace, and its
    /// older `.agents/profiles`.
    Project,
    /// The `.arko/profiles` and the older `.agents/profiles` in the root of
    /// the git repository the project is in, when the project is a subfolder
    /// of it. The commands and the agent's tool read it and don't save to it.
    Repository,
    /// `profiles` in Arko's folder, for every project.
    User,
}

impl Place {
    /// Every place, nearest first.
    pub const ALL: [Place; 4] = [
        Place::Session,
        Place::Project,
        Place::Repository,
        Place::User,
    ];

    /// The word the commands and the agent's tool take: `session`,
    /// `project` or `user`. (The repository's is `repository`, which they
    /// don't take.)
    pub fn name(self) -> &'static str {
        match self {
            Place::Session => "session",
            Place::Project => "project",
            Place::Repository => "repository",
            Place::User => "user",
        }
    }

    /// Parses [`name`](Self::name), in any case, for the places the commands
    /// and the agent's tool take.
    pub fn parse(text: &str) -> Option<Place> {
        Place::ALL
            .into_iter()
            .filter(|place| *place != Place::Repository)
            .find(|place| place.name().eq_ignore_ascii_case(text.trim()))
    }

    /// Who can use a profile in this place, as a phrase that follows
    /// "for": `this session`, `this project`, `this repository` or `every
    /// project`.
    pub fn reach(self) -> &'static str {
        match self {
            Place::Session => "this session",
            Place::Project => "this project",
            Place::Repository => "this repository",
            Place::User => "every project",
        }
    }
}

/// How long a worker's prompt cache lasts, for the models that take a
/// choice. `OneHour` is the long cache (an hour on Claude models, and 12
/// hours on OpenAI GPT-5.6 and later with an API key), and `FiveMinutes`
/// the short one (5 minutes on Claude, 30 on OpenAI). They are named for
/// Claude's, the first they meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Cache {
    FiveMinutes,
    OneHour,
}

impl Cache {
    /// `5m` or `1h`, as the file writes it, so earlier versions read it.
    pub fn as_str(self) -> &'static str {
        match self {
            Cache::FiveMinutes => "5m",
            Cache::OneHour => "1h",
        }
    }
}

impl std::str::FromStr for Cache {
    type Err = String;

    /// `long` or `short`, or `1h` or `5m`.
    fn from_str(text: &str) -> Result<Self, String> {
        match crate::crew::long_cache_asked(text) {
            Ok(true) => Ok(Cache::OneHour),
            Ok(false) => Ok(Cache::FiveMinutes),
            Err(_) => Err("the cache is long or short".to_string()),
        }
    }
}

/// A worker profile. An app builds one with [`Profile::new`] and sets the
/// fields it has, so a field added later breaks no one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Profile {
    /// The name, which is the file's name without `.md`.
    pub name: String,
    /// One line of up to [`MAX_DESCRIPTION`] characters.
    pub description: String,
    /// A model as `/model` takes it (an alias, an ID, `openrouter:<slug>`
    /// or `cc:<model>`). `None` is the session's worker model.
    pub model: Option<String>,
    /// The reasoning effort. `None` is the nearest one the model takes to
    /// the session's worker effort.
    pub effort: Option<Effort>,
    /// Fast mode, for the models that have it.
    pub fast: bool,
    /// The prompt cache's lifetime. `None` is the session's choice.
    pub cache: Option<Cache>,
    /// The worker may start workers of its own.
    pub nested: bool,
    /// The skills a worker with the profile gets, by name. `None` is its
    /// starter's, and an empty list none.
    pub skills: Option<Vec<String>>,
    /// The MCP servers a worker with the profile reaches, in the same way.
    pub mcp: Option<Vec<String>>,
    /// Who wrote the file when it isn't the user: [`MADE_BY_AGENT`].
    pub made_by: Option<String>,
    /// The standing instructions, up to [`MAX_INSTRUCTIONS`] characters.
    pub instructions: String,
    /// The keys this version doesn't know, as the file wrote them.
    pub extra: Vec<(String, String)>,
}

impl Profile {
    /// A profile named `name` with `description` and nothing else set.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            model: None,
            effort: None,
            fast: false,
            cache: None,
            nested: false,
            skills: None,
            mcp: None,
            made_by: None,
            instructions: String::new(),
            extra: Vec::new(),
        }
    }

    /// An agent wrote the profile.
    pub fn made_by_agent(&self) -> bool {
        self.made_by.as_deref() == Some(MADE_BY_AGENT)
    }

    /// Reads a profile named `name` from the text of its file.
    pub fn parse(name: &str, text: &str) -> Result<Self, String> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let text = text.replace("\r\n", "\n");
        let mut profile = Profile::new(name, "");
        let mut described = false;
        let mut seen: Vec<String> = Vec::new();
        let mut closed = false;
        let mut position = 0;
        for (number, raw) in text.split_inclusive('\n').enumerate() {
            position += raw.len();
            let line = raw.trim_end_matches('\n');
            if number == 0 {
                if line.trim() != "---" {
                    return Err("it doesn't start with a --- line and a description".to_string());
                }
                continue;
            }
            if line.trim() == "---" {
                closed = true;
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                return Err(format!("line {} isn't `key: value`", number + 1));
            };
            let key = key.trim();
            if key.is_empty()
                || !key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(format!("line {} isn't `key: value`", number + 1));
            }
            let value = unquote(value.trim());
            let lowered = key.to_ascii_lowercase();
            if KNOWN_KEYS.contains(&lowered.as_str()) {
                if seen.contains(&lowered) {
                    return Err(format!("`{lowered}` is given twice"));
                }
                seen.push(lowered.clone());
            }
            match lowered.as_str() {
                "description" => {
                    profile.description = value;
                    described = true;
                }
                "model" => profile.model = Some(value).filter(|value| !value.is_empty()),
                "effort" => {
                    profile.effort = match value.as_str() {
                        "" => None,
                        other => Some(other.parse::<Effort>().map_err(|()| {
                            format!(
                                "`effort: {}` isn't an effort (low, medium, high, xhigh or max)",
                                cut(other, 40)
                            )
                        })?),
                    }
                }
                "fast" => profile.fast = boolean("fast", &value)?,
                "cache" => {
                    profile.cache = match value.as_str() {
                        "" => None,
                        other => Some(other.parse::<Cache>().map_err(|_| {
                            format!(
                                "`cache: {}` isn't a prompt cache (long or short)",
                                cut(other, 40)
                            )
                        })?),
                    }
                }
                "nested" => profile.nested = boolean("nested", &value)?,
                "skills" => profile.skills = names("skills", &value)?,
                "mcp" => profile.mcp = names("mcp", &value)?,
                "made_by" => profile.made_by = Some(value).filter(|value| !value.is_empty()),
                _ => profile.extra.push((key.to_string(), value)),
            }
        }
        if !closed {
            return Err("the front matter isn't closed with a --- line".to_string());
        }
        if !described || profile.description.trim().is_empty() {
            return Err("it has no description".to_string());
        }
        if profile.description.chars().count() > MAX_DESCRIPTION {
            return Err(format!(
                "the description is over {MAX_DESCRIPTION} characters"
            ));
        }
        let body = text.get(position..).unwrap_or_default();
        profile.instructions = body.trim().to_string();
        if profile.instructions.chars().count() > MAX_INSTRUCTIONS {
            return Err(format!(
                "the instructions are over {} characters",
                thousands(MAX_INSTRUCTIONS)
            ));
        }
        if profile
            .model
            .as_ref()
            .is_some_and(|model| model.chars().count() > MAX_MODEL)
        {
            return Err(format!("the model is over {MAX_MODEL} characters"));
        }
        for (what, text) in [
            ("description", Some(&profile.description)),
            ("instructions", Some(&profile.instructions)),
            ("model", profile.model.as_ref()),
        ] {
            if text.is_some_and(|text| unprintable(text).is_some()) {
                return Err(format!(
                    "the {what} can't have a control or invisible character"
                ));
            }
        }
        Ok(profile)
    }

    /// The text of the file that holds the profile.
    pub fn render(&self) -> String {
        let mut text = String::from("---\n");
        let mut put = |key: &str, value: &str| {
            text.push_str(&format!("{key}: {}\n", quoted(value)));
        };
        put("description", &one_line(&self.description));
        if let Some(model) = &self.model {
            put("model", model);
        }
        if let Some(effort) = self.effort {
            put("effort", &effort.to_string());
        }
        if self.fast {
            put("fast", "true");
        }
        if let Some(cache) = self.cache {
            put("cache", cache.as_str());
        }
        if self.nested {
            put("nested", "true");
        }
        for (key, names) in [("skills", &self.skills), ("mcp", &self.mcp)] {
            if let Some(names) = names {
                put(
                    key,
                    &if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    },
                );
            }
        }
        if let Some(made_by) = &self.made_by {
            put("made_by", made_by);
        }
        for (key, value) in &self.extra {
            put(key, value);
        }
        text.push_str("---\n");
        let instructions = self.instructions.trim();
        if !instructions.is_empty() {
            text.push_str(instructions);
            text.push('\n');
        }
        text
    }

    /// Whether the profile can be saved, as a user or an agent would write
    /// it. That is a valid name, a description, instructions within the
    /// limit, and a model Arko knows with an effort it takes. Whether the
    /// session has the model's credential is not a question for the file.
    pub fn check(&self) -> Result<(), String> {
        check_name(&self.name)?;
        let description = one_line(&self.description);
        if description.is_empty() {
            return Err("A profile needs a description, one line saying what it is for.".into());
        }
        if description.chars().count() > MAX_DESCRIPTION {
            return Err(format!(
                "The description is {} characters, and the limit is {MAX_DESCRIPTION}.",
                description.chars().count()
            ));
        }
        let length = self.instructions.trim().chars().count();
        if length > MAX_INSTRUCTIONS {
            return Err(format!(
                "The instructions are {} characters, and the limit is {}.",
                thousands(length),
                thousands(MAX_INSTRUCTIONS)
            ));
        }
        if let Some(model) = &self.model
            && model.chars().count() > MAX_MODEL
        {
            return Err(format!(
                "The model is {} characters, and the limit is {MAX_MODEL}.",
                model.chars().count()
            ));
        }
        for (what, text) in [
            ("description", Some(&self.description)),
            ("instructions", Some(&self.instructions)),
            ("model", self.model.as_ref()),
        ] {
            if text.is_some_and(|text| unprintable(text).is_some()) {
                return Err(format!(
                    "The {what} can't have a control or invisible character."
                ));
            }
        }
        for (what, names) in [("skills", &self.skills), ("mcp", &self.mcp)] {
            if let Some(problem) = names
                .iter()
                .flatten()
                .find_map(|name| crate::crew::name_problem(name))
            {
                return Err(format!("The {what} list {problem}."));
            }
        }
        if let Some(problem) = self.model_problem() {
            return Err(problem);
        }
        Ok(())
    }

    /// What workers with the profile are limited to, as sentences for its
    /// approval card: "Workers with it get only the skills ponytail and
    /// release-notes." Nothing for a set the profile leaves to the starter.
    pub fn limits(&self) -> Vec<String> {
        crate::crew::narrowing_phrases(self.skills.as_deref(), self.mcp.as_deref())
            .into_iter()
            .map(|phrase| format!("Workers with it get {phrase}."))
            .collect()
    }

    /// What is wrong with the profile's model and effort, when something is.
    /// The model must be one Arko knows, and the effort one it takes.
    pub fn model_problem(&self) -> Option<String> {
        let name = self.model.as_deref()?;
        let model = match providers::resolve_model(name) {
            Ok(model) => model,
            Err(error) => return Some(providers::briefly(&error).to_string()),
        };
        let effort = self.effort?;
        let takes = providers::efforts(&model);
        if takes.is_empty() || takes.contains(&effort) {
            return None;
        }
        Some(format!(
            "{} takes {} effort, not {effort}.",
            providers::model_name(&model),
            providers::efforts_text(&model)
        ))
    }

    /// What the profile runs on, as a phrase: `sol at high effort`, `opus`,
    /// or `the session's worker model`.
    pub fn runs_on(&self) -> String {
        let model = self
            .model
            .clone()
            .unwrap_or_else(|| "the session's worker model".into());
        match self.effort {
            Some(effort) => format!("{model} at {effort} effort"),
            None => model,
        }
    }

    /// The file's name for a profile called `name`.
    pub fn file_name(name: &str) -> String {
        format!("{name}.{EXTENSION}")
    }
}

const KNOWN_KEYS: [&str; 9] = [
    "description",
    "model",
    "effort",
    "fast",
    "cache",
    "nested",
    "skills",
    "mcp",
    "made_by",
];

/// The names a `skills` or `mcp` line gives, `None` for a line with nothing
/// in it.
fn names(key: &str, value: &str) -> Result<Option<Vec<String>>, String> {
    crate::crew::names_asked(value).map_err(|why| format!("`{key}` {why}"))
}

fn boolean(key: &str, value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" | "" => Ok(false),
        other => Err(format!("`{key}: {other}` isn't true or false")),
    }
}

/// `value` without the quotes around it, when it has them.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if value.len() >= 2 && bytes[0] == b'"' && bytes[value.len() - 1] == b'"' {
        // Quotes inside that nothing escapes mean the outer ones aren't a
        // pair around the whole value (`"a" and "b"`): it is read as written.
        let inner = &value[1..value.len() - 1];
        let mut escaped = false;
        for c in inner.chars() {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => return value.to_string(),
                _ => {}
            }
        }
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        }
        out
    } else if value.len() >= 2 && bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'' {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// `value` as the file writes it: bare when that reads back the same,
/// else in double quotes.
fn quoted(value: &str) -> String {
    let plain = value == value.trim()
        && !value.is_empty()
        && !value.starts_with(['"', '\'', '#'])
        && !value.contains(['\n', '\r']);
    if plain {
        return value.to_string();
    }
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace("\r\n", "\\n")
            .replace('\n', "\\n")
    )
}

/// The first character of `text` that a profile can't hold: a control
/// character other than a line break or a tab (an escape sequence moves the
/// cursor, a lone carriage return overwrites a line), a direction override
/// or a zero-width character. They would make the question that asks the
/// user to approve the profile show something other than the text.
fn unprintable(text: &str) -> Option<char> {
    text.chars().find(|c| is_unprintable(*c))
}

fn is_unprintable(c: char) -> bool {
    (c.is_control() && !matches!(c, '\n' | '\t'))
        || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
}

/// `text` cut to `most` characters, with an ellipsis when it was cut, and
/// with a replacement mark for each character [`unprintable`] finds. A value
/// or a name read from a file goes into a reason or a list this way, so a
/// file can't make it as long as it likes or move the cursor of the
/// terminal that shows it.
fn cut(text: &str, most: usize) -> String {
    let clean = |c: char| if is_unprintable(c) { '\u{fffd}' } else { c };
    if text.chars().count() <= most {
        text.chars().map(clean).collect()
    } else {
        format!(
            "{}…",
            text.chars().take(most).map(clean).collect::<String>()
        )
    }
}

/// `text` on one line: its lines joined by single spaces.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `4000` as `4,000`.
fn thousands(number: usize) -> String {
    let digits: Vec<char> = number.to_string().chars().rev().collect();
    let groups: Vec<String> = digits
        .chunks(3)
        .map(|group| group.iter().rev().collect())
        .collect();
    groups.into_iter().rev().collect::<Vec<_>>().join(",")
}

/// Names Windows keeps for devices. A file by one of them can't be made
/// there, with or without an extension, and profiles travel through git.
const DEVICE_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Whether `name` is one of the names Windows keeps for devices.
fn is_device_name(name: &str) -> bool {
    DEVICE_NAMES.contains(&name)
}

/// What a profile name is.
const NAME_RULE: &str = "A name is lowercase letters, digits and hyphens, up to 32 characters, starting with a letter or a digit.";

/// Whether `name` is a valid profile name, else why not.
pub fn check_name(name: &str) -> Result<(), String> {
    let first = name.chars().next();
    let valid = first.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name.chars().count() <= MAX_NAME;
    if !valid {
        return Err(format!("`{name}` isn't a profile name. {NAME_RULE}"));
    }
    if RESERVED.contains(&name) {
        return Err(format!(
            "`{name}` is reserved. Choose another name for the profile."
        ));
    }
    if is_device_name(name) {
        return Err(format!(
            "`{name}` is a name Windows keeps for a device, so a file can't have it there. Choose another name for the profile."
        ));
    }
    Ok(())
}

/// The file in Arko's folder that holds the [`Preferences`].
const PREFERENCES_FILE: &str = "profiles.json";

/// What the user chose for every project about where profiles are kept, in
/// `profiles.json` in Arko's folder. [`Roots`] and [`save`] read the file
/// each time, so a change reaches the next worker started with no restart.
/// Without the file, or when it can't be read, both are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// "Keep agents' project files out of git". When Arko makes a project's
    /// `.arko` folder, it writes a `.gitignore` in it, so git ignores the
    /// folder and the file ([`arko_dir`](crate::arko_dir)). The field keeps
    /// its name from when the choice covered the profiles folder only.
    pub keep_out_of_git: bool,
    /// Also look in the profiles folders of the repository's root, when
    /// the project is in a subfolder of a git repository.
    pub repo_root: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            keep_out_of_git: true,
            repo_root: true,
        }
    }
}

impl Preferences {
    /// The preferences in Arko's folder `root`.
    pub fn load(root: &Path) -> Self {
        std::fs::read_to_string(root.join(PREFERENCES_FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Saves them in Arko's folder `root`. A project's `.arko/.gitignore`
    /// stays as it is, so use [`save_in`](Self::save_in) where there is a
    /// project.
    pub fn save(&self, root: &Path) -> Result<(), String> {
        let file = root.join(PREFERENCES_FILE);
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(root)
            .and_then(|()| crate::session::write_whole(&file, (text + "\n").as_bytes()))
            .map_err(|error| format!("Couldn't write {} ({})", file.display(), reason_of(&error)))
    }

    /// Saves them in Arko's folder `root`, and when
    /// [`keep_out_of_git`](Self::keep_out_of_git) changed, adds or removes
    /// Arko's `.gitignore` in the project `workspace`'s `.arko` folder
    /// ([`arko_dir::apply_setting`](crate::arko_dir::apply_setting)). The
    /// preferences are saved also when that fails and this returns its error.
    pub fn save_in(&self, root: &Path, workspace: &Path) -> Result<(), String> {
        let before = Preferences::load(root).keep_out_of_git;
        self.save(root)?;
        if before != self.keep_out_of_git {
            crate::arko_dir::apply_setting(workspace, self.keep_out_of_git)?;
        }
        Ok(())
    }
}

/// The folders profiles are read from and saved in. The session's, the
/// project's and the repository's are optional, as an app's settings page
/// has none of them.
#[derive(Debug, Clone)]
pub struct Roots {
    root: PathBuf,
    workspace: Option<PathBuf>,
    session: Option<PathBuf>,
}

impl Roots {
    /// Arko's folder (the user's profiles are in its `profiles`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            workspace: None,
            session: None,
        }
    }

    /// Adds the project, the session's working directory.
    pub fn workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Adds the session's folder.
    pub fn session(mut self, session_dir: impl Into<PathBuf>) -> Self {
        self.session = Some(session_dir.into());
        self
    }

    /// The folder that holds the profiles of `place`, and the one a save
    /// writes, whether it exists or not. For the project and the repository
    /// it is `.arko/profiles`, and [`folders`] has the older
    /// `.agents/profiles` too. `None` when there is no session or project,
    /// and for the repository when the project is not in a subfolder of a
    /// git repository or [`Preferences::repo_root`] is off.
    pub fn folder(&self, place: Place) -> Option<PathBuf> {
        match place {
            Place::Session => self.session.as_ref().map(|dir| dir.join(FOLDER)),
            Place::Project | Place::Repository => self
                .project_dir(place)
                .map(|dir| dir.join(PROJECT_FOLDER[0]).join(PROJECT_FOLDER[1])),
            Place::User => Some(self.root.join(FOLDER)),
        }
    }

    /// The folder of `place` that earlier versions saved in, which is still
    /// read, after [`folder`](Self::folder).
    fn older_folder(&self, place: Place) -> Option<PathBuf> {
        self.project_dir(place).map(|dir| {
            dir.join(OLDER_PROJECT_FOLDER[0])
                .join(OLDER_PROJECT_FOLDER[1])
        })
    }

    /// Every folder profiles of `place` are read from, the one a save writes
    /// first.
    fn read_folders(&self, place: Place) -> Vec<PathBuf> {
        self.folder(place)
            .into_iter()
            .chain(self.older_folder(place))
            .collect()
    }

    /// The directory that holds the `.arko` of a project place: the working
    /// directory, or the root of the repository it is in.
    fn project_dir(&self, place: Place) -> Option<PathBuf> {
        let workspace = self.workspace.as_deref()?;
        match place {
            Place::Project => Some(workspace.to_path_buf()),
            Place::Repository => {
                // The root has a `.git` folder, or a `.git` file in a worktree.
                let top = workspace
                    .ancestors()
                    .find(|dir| dir.join(".git").exists())?;
                (top != workspace && Preferences::load(&self.root).repo_root)
                    .then(|| top.to_path_buf())
            }
            Place::Session | Place::User => None,
        }
    }
}

/// Every folder profiles are read from, nearest first, for "open the
/// folder" and for text that names them. The project and the repository have
/// two, `.arko/profiles` first and then the older `.agents/profiles`.
pub fn folders(roots: &Roots) -> Vec<(Place, PathBuf)> {
    Place::ALL
        .into_iter()
        .flat_map(|place| {
            roots
                .read_folders(place)
                .into_iter()
                .map(move |folder| (place, folder))
        })
        .collect()
}

/// A profile that was read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Found {
    pub profile: Profile,
    pub place: Place,
    /// The file it was read from.
    pub file: PathBuf,
    /// Why a worker can't be started with it as it is, when its model isn't
    /// one Arko knows or doesn't take its effort.
    pub note: Option<String>,
}

impl Found {
    /// The file is in the older `.agents/profiles` of its place, not in
    /// `.arko/profiles`.
    pub fn in_older_folder(&self) -> bool {
        in_older_folder(self.place, &self.file)
    }

    /// [`Place::reach`], with the folder named when it is the older one:
    /// `this project (.agents/profiles)`.
    pub fn reach(&self) -> String {
        reach_of(self.place, &self.file)
    }
}

/// Whether `file`, a profile of `place`, is in the older `.agents/profiles`.
fn in_older_folder(place: Place, file: &Path) -> bool {
    matches!(place, Place::Project | Place::Repository)
        && file.parent().is_some_and(|folder| {
            folder.ends_with(Path::new(OLDER_PROJECT_FOLDER[0]).join(OLDER_PROJECT_FOLDER[1]))
        })
}

fn reach_of(place: Place, file: &Path) -> String {
    if in_older_folder(place, file) {
        format!(
            "{} ({}/{})",
            place.reach(),
            OLDER_PROJECT_FOLDER[0],
            OLDER_PROJECT_FOLDER[1]
        )
    } else {
        place.reach().to_string()
    }
}

/// A profile file that couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Unreadable {
    pub place: Place,
    pub file: PathBuf,
    /// The name its file's name gives it.
    pub name: String,
    /// What is wrong, suitable for display.
    pub reason: String,
}

impl Unreadable {
    /// [`Found::reach`] for a file that couldn't be read.
    pub fn reach(&self) -> String {
        reach_of(self.place, &self.file)
    }
}

/// The profiles of all places.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Profiles {
    /// The profiles in force, one for each name (the nearest), by name.
    pub active: Vec<Found>,
    /// The profiles a nearer one of the same name hides, by name and then
    /// from the nearest.
    pub hidden: Vec<Found>,
    /// The files that couldn't be read.
    pub unreadable: Vec<Unreadable>,
}

impl Profiles {
    /// The profile in force under `name`.
    pub fn get(&self, name: &str) -> Option<&Found> {
        self.active.iter().find(|found| found.profile.name == name)
    }
}

/// Why [`read`] found no profile.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadError {
    /// `name` is not a valid profile name.
    BadName(String),
    /// No place has a file of that name.
    Missing,
    /// A file of that name is there and couldn't be read.
    Unreadable(Unreadable),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::BadName(why) => f.write_str(why),
            ReadError::Missing => f.write_str("There is no such profile."),
            ReadError::Unreadable(problem) => write!(
                f,
                "{} can't be used ({}).",
                problem.file.display(),
                problem.reason
            ),
        }
    }
}

impl std::error::Error for ReadError {}

/// All the profiles, from every place `roots` has. A file that can't be
/// read is listed among the unreadable and hides no profile.
pub fn list(roots: &Roots) -> Profiles {
    let mut profiles = Profiles::default();
    let mut folders_seen: Vec<PathBuf> = Vec::new();
    // (A place's folders are read `.arko/profiles` first, so a name in both
    // is the first's and the other is hidden.)
    for (place, folder) in folders(roots) {
        // The same folder reached twice (a project in Arko's folder, say)
        // is read once, in its nearest place.
        if let Ok(real) = std::fs::canonicalize(&folder) {
            if folders_seen.contains(&real) {
                continue;
            }
            folders_seen.push(real);
        }
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == EXTENSION)
                    && path.is_file()
            })
            .collect();
        files.sort();
        for file in files {
            let name = file
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            // A README beside the profiles is not one.
            if name.eq_ignore_ascii_case("readme") {
                continue;
            }
            match read_file(place, &file, &name) {
                Ok(found) => {
                    if profiles
                        .active
                        .iter()
                        .any(|other| other.profile.name == found.profile.name)
                    {
                        profiles.hidden.push(found);
                    } else {
                        profiles.active.push(found);
                    }
                }
                Err(problem) => profiles.unreadable.push(problem),
            }
        }
    }
    profiles
        .active
        .sort_by(|a, b| a.profile.name.cmp(&b.profile.name));
    profiles
        .hidden
        .sort_by(|a, b| (&a.profile.name, a.place).cmp(&(&b.profile.name, b.place)));
    profiles
}

/// The profile `name`, from the nearest place that has a file of that name
/// that can be read.
pub fn read(roots: &Roots, name: &str) -> Result<Found, ReadError> {
    check_file_name(name).map_err(ReadError::BadName)?;
    let mut problem: Option<Unreadable> = None;
    for (place, folder) in folders(roots) {
        let file = folder.join(Profile::file_name(name));
        if !file.is_file() || !has_entry(&folder, &Profile::file_name(name)) {
            continue;
        }
        match read_file(place, &file, name) {
            Ok(found) => return Ok(found),
            Err(unreadable) => {
                problem.get_or_insert(unreadable);
            }
        }
    }
    Err(problem.map_or(ReadError::Missing, ReadError::Unreadable))
}

/// Whether `folder` has an entry called exactly `name`. A file system that
/// ignores case (macOS, Windows) finds `Reviewer.md` for `reviewer.md`, and
/// `list` calls that file unreadable for its name, so a lookup by name must
/// not find it either.
fn has_entry(folder: &Path, name: &str) -> bool {
    std::fs::read_dir(folder).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name() == std::ffi::OsStr::new(name))
    })
}

/// A name that can be a file's name here: the file name rules without the
/// reserved names and the device names (those can't be saved, and a file by
/// one is shown as unreadable).
fn check_file_name(name: &str) -> Result<(), String> {
    match check_name(name) {
        Err(why) if !RESERVED.contains(&name) && !is_device_name(name) => Err(why),
        _ => Ok(()),
    }
}

fn read_file(place: Place, file: &Path, name: &str) -> Result<Found, Unreadable> {
    let unreadable = |reason: String| Unreadable {
        place,
        file: file.to_path_buf(),
        name: cut(name, MAX_NAME * 2),
        reason,
    };
    if check_name(name).is_err() {
        // (The reserved names get their own reason.)
        return Err(unreadable(if RESERVED.contains(&name) {
            format!("its name, `{name}`, is reserved")
        } else if is_device_name(name) {
            format!("its name, `{name}`, is one Windows keeps for a device")
        } else {
            "its name isn't a profile name (lowercase letters, digits and hyphens, up to 32 characters, starting with a letter or a digit)".to_string()
        }));
    }
    let size = std::fs::metadata(file)
        .map_err(|error| unreadable(format!("it can't be read ({})", reason_of(&error))))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(unreadable(format!(
            "it is over {} KB",
            MAX_FILE_BYTES / 1024
        )));
    }
    let bytes = std::fs::read(file)
        .map_err(|error| unreadable(format!("it can't be read ({})", reason_of(&error))))?;
    let text =
        String::from_utf8(bytes).map_err(|_| unreadable("it isn't text in UTF-8".to_string()))?;
    let profile = Profile::parse(name, &text).map_err(unreadable)?;
    let note = profile.model_problem();
    Ok(Found {
        profile,
        place,
        file: file.to_path_buf(),
        note,
    })
}

/// An I/O error as a short phrase.
pub(crate) fn reason_of(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        std::io::ErrorKind::NotFound => "not found".to_string(),
        _ => error.to_string(),
    }
}

/// Who writes a profile. A user's edit takes the file whole. An agent's
/// can't replace what a user made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Author {
    /// The user, in an app or a command.
    User,
    /// An agent, through a tool.
    Agent,
}

/// What [`save`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Saved {
    pub file: PathBuf,
    /// A profile of that name was in that place already.
    pub replaced: bool,
    /// A nearer place has a profile of the same name, which this one stays
    /// behind.
    pub hidden_by: Option<Place>,
}

/// Saves `profile` in `place`, as `author` writes it. The profile is
/// checked first ([`Profile::check`]). The file is written whole through a
/// temporary file, so a reader or a second writer never finds half of it.
///
/// An agent's save is marked `made_by: agent`, and can't replace or hide a
/// profile that isn't (one the user made, in any place), and a user's save
/// takes the mark off, as the profile is the user's now.
///
/// The project and the repository get the file in `.arko/profiles`. A
/// profile of the same name in the older `.agents/profiles` of that place is
/// replaced by it, so the old file is deleted and the name isn't listed
/// twice. [`arko_dir::ensure`](crate::arko_dir::ensure) makes the `.arko`
/// folder, with its `.gitignore` while [`Preferences::keep_out_of_git`] is
/// on.
pub fn save(
    roots: &Roots,
    place: Place,
    profile: &Profile,
    author: Author,
) -> Result<Saved, String> {
    profile.check()?;
    let folder = roots.folder(place).ok_or_else(|| match place {
        Place::Session => "There is no session to keep a profile in.".to_string(),
        _ => "There is no project to keep a profile in.".to_string(),
    })?;
    let file = folder.join(Profile::file_name(&profile.name));
    if author == Author::Agent {
        check_agent_save(roots, &profile.name)?;
    }
    let mut written = profile.clone();
    written.description = one_line(&profile.description);
    written.instructions = profile.instructions.trim().to_string();
    written.made_by = match author {
        Author::Agent => Some(MADE_BY_AGENT.to_string()),
        Author::User => None,
    };
    // The same profile in the older folder of the place.
    let older: Vec<PathBuf> = roots
        .older_folder(place)
        .into_iter()
        .filter(|folder| has_entry(folder, &Profile::file_name(&profile.name)))
        .map(|folder| folder.join(Profile::file_name(&profile.name)))
        .filter(|older| older.is_file())
        .collect();
    let replaced = file.is_file() || !older.is_empty();
    if let Some(dir) = roots.project_dir(place) {
        // (Neither `.arko` nor its profiles folder may be a link.)
        crate::arko_dir::ensure_in(&dir, &roots.root, PROJECT_FOLDER[1])?;
    }
    std::fs::create_dir_all(&folder)
        .map_err(|error| format!("Couldn't make {} ({})", folder.display(), reason_of(&error)))?;
    let text = written.render();
    let write = if author == Author::Agent && !replaced {
        // The check above and this write are two steps. Someone who wrote
        // the same name between them keeps their file, and the agent is
        // refused as if it had been there.
        match crate::session::write_new(&file, text.as_bytes()) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                check_agent_save(roots, &profile.name)?;
                crate::session::write_whole(&file, text.as_bytes())
            }
            other => other,
        }
    } else {
        crate::session::write_whole(&file, text.as_bytes())
    };
    write.map_err(|error| format!("Couldn't write {} ({})", file.display(), reason_of(&error)))?;
    for old in older {
        // (A link that makes the two folders one has the old file be the new.)
        if same_file(&old, &file) {
            continue;
        }
        std::fs::remove_file(&old).map_err(|error| {
            format!(
                "Saved {}, but couldn't delete the older {} ({}). Delete it so the profile isn't listed twice.",
                file.display(),
                old.display(),
                reason_of(&error)
            )
        })?;
    }
    let hidden_by = Place::ALL
        .into_iter()
        .take_while(|nearer| *nearer < place)
        .find(|nearer| {
            roots
                .read_folders(*nearer)
                .iter()
                .any(|folder| folder.join(Profile::file_name(&profile.name)).is_file())
        });
    Ok(Saved {
        file,
        replaced,
        hidden_by,
    })
}

/// Whether an agent may save a profile called `name`: not when a profile of
/// that name that the user made is in any place (it would replace it, or
/// hide it behind a nearer one), nor over a file that can't be read. A
/// profile an agent made, in any place, may be replaced. [`save`] checks this
/// itself, and a tool that asks the user first checks it before asking.
pub fn check_agent_save(roots: &Roots, name: &str) -> Result<(), String> {
    for (place, folder) in folders(roots) {
        let existing = folder.join(Profile::file_name(name));
        if !existing.is_file() || !has_entry(&folder, &Profile::file_name(name)) {
            continue;
        }
        match read_file(place, &existing, name) {
            Ok(found) if found.profile.made_by_agent() => {}
            Ok(_) => {
                return Err(format!(
                    "There is already a profile `{name}` made by the user. Choose another name."
                ));
            }
            Err(problem) => {
                return Err(format!(
                    "There is already a file {} that can't be read, so Arko won't replace it. Choose another name.",
                    problem.file.display()
                ));
            }
        }
    }
    Ok(())
}

/// Whether the user is asked before an agent saves the profile `name` in
/// `place`. Never for the session. For the project not when git ignores the
/// profiles folder (inside a git work tree, as `git check-ignore` says, or
/// a `.arko` folder [`save`] is about to make with its own `.gitignore`), and
/// outside a git repository the user is asked, as for the other places. A
/// folder that a link leads out of the project is asked about too.
pub fn needs_approval(roots: &Roots, place: Place, name: &str) -> bool {
    match place {
        Place::Session => false,
        Place::Project => !roots
            .workspace
            .as_deref()
            .zip(roots.folder(Place::Project))
            .is_some_and(|(workspace, folder)| {
                is_ignored(
                    workspace,
                    &folder,
                    name,
                    Preferences::load(&roots.root).keep_out_of_git,
                )
            }),
        _ => true,
    }
}

/// Whether git ignores the profile `name` in the project's `folder`
/// (`.arko/profiles`), which is in the `workspace`.
fn is_ignored(workspace: &Path, folder: &Path, name: &str, keep_out_of_git: bool) -> bool {
    // The folder may not be made yet.
    let Some(existing) = folder.ancestors().find(|dir| dir.is_dir()) else {
        return false;
    };
    // `.arko` may be a link to another place, which is not ignored by
    // the project's repository, so the user is asked.
    let inside = existing
        .canonicalize()
        .and_then(|existing| Ok(existing.starts_with(workspace.canonicalize()?)));
    if !inside.unwrap_or(false) {
        return false;
    }
    if crate::sessions::git(existing, &["rev-parse", "--is-inside-work-tree"]).as_deref()
        != Some("true")
    {
        return false;
    }
    // With the setting off a save takes Arko's own `.gitignore` out first, so
    // git is to track what it writes.
    if !keep_out_of_git && crate::arko_dir::has_own_ignore(workspace) {
        return false;
    }
    // A save makes `.arko` with its own `.gitignore`.
    if !crate::arko_dir::dir(workspace).exists() && keep_out_of_git {
        return true;
    }
    let file = folder.join(Profile::file_name(name));
    crate::sessions::git(
        existing,
        &["check-ignore", "-q", "--", &file.to_string_lossy()],
    )
    .is_some()
}

/// Whether `a` and `b` are one file, reached by two paths.
fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Deletes the profile `name` from `place`, the one [`read`] finds there
/// (`.arko/profiles` before the older `.agents/profiles`). Returns the file it
/// removed.
pub fn delete(roots: &Roots, place: Place, name: &str) -> Result<PathBuf, String> {
    check_file_name(name)?;
    let folders = roots.read_folders(place);
    if folders.is_empty() {
        return Err(format!(
            "There is no {} to delete a profile from.",
            place.name()
        ));
    }
    let Some(file) = folders
        .iter()
        .find(|folder| {
            folder.join(Profile::file_name(name)).is_file()
                && has_entry(folder, &Profile::file_name(name))
        })
        .map(|folder| folder.join(Profile::file_name(name)))
    else {
        return Err(format!(
            "There is no profile `{name}` for {}.",
            place.reach()
        ));
    };
    std::fs::remove_file(&file)
        .map_err(|error| format!("Couldn't delete {} ({})", file.display(), reason_of(&error)))?;
    Ok(file)
}

/// Copies the profiles in the session folder `from` to the session folder
/// `to`, for a branch of the session, whose conversation can name them.
pub(crate) fn copy_session_profiles(from: &Path, to: &Path) -> std::io::Result<()> {
    let Ok(entries) = std::fs::read_dir(from.join(FOLDER)) else {
        return Ok(());
    };
    let target = to.join(FOLDER);
    for entry in entries.flatten() {
        let file = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_file())
            && file.extension().is_some_and(|ext| ext == EXTENSION)
        {
            std::fs::create_dir_all(&target)?;
            std::fs::copy(&file, target.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// "812 characters" or "812 characters (28 lines)".
fn size_of(text: &str) -> String {
    let characters = text.chars().count();
    match text.lines().count() {
        0 | 1 => format!("{characters} characters"),
        lines => format!("{characters} characters ({lines} lines)"),
    }
}

/// The question that asks the user to approve the main agent's save of
/// `profile` in `place`. It reads the same in the terminal, on the web page
/// and in an app, and carries the whole profile so the user judges its
/// instructions as well as its model. [`approval`] has its parts for an
/// approval card.
pub fn approval_text(profile: &Profile, place: Place) -> String {
    let wherever = match place {
        Place::User => "in your Arko folder, for every project",
        Place::Project => "in this project's .arko/profiles folder",
        Place::Repository => "in the repository's .arko/profiles folder",
        Place::Session => "for this session",
    };
    let mut text = format!(
        "The main agent wants to save the worker profile {} {wherever}.\n\n{}\nRuns on {}",
        profile.name,
        one_line(&profile.description),
        profile.runs_on()
    );
    if profile.fast {
        text.push_str(" in fast mode");
    }
    if let Some(cache) = profile.cache {
        text.push_str(&format!(" with a {} prompt cache", cache.as_str()));
    }
    text.push('.');
    if profile.nested {
        text.push_str(" A worker started with it can start workers of its own.");
    }
    for limit in profile.limits() {
        text.push_str(&format!(" {limit}"));
    }
    text.push('\n');
    let instructions = profile.instructions.trim();
    if instructions.is_empty() {
        text.push_str("\nIt has no instructions.\n");
    } else {
        // Right after the model, where a short view of the question still
        // shows it, so the user knows how much there is to read.
        text.push_str(&format!(
            "Its instructions are {}.\n\nInstructions\n{instructions}\n",
            size_of(instructions)
        ));
    }
    text.trim_end().to_string()
}

/// What the approval tray shows of the main agent's save of `profile` in
/// `place`, which is its name, model, effort, scope, description, what it
/// narrows for its workers ([`Profile::limits`]) and instructions.
pub fn approval(profile: &Profile, place: Place) -> crate::approvals::ProfileApproval {
    crate::approvals::ProfileApproval {
        name: profile.name.clone(),
        model: profile
            .model
            .clone()
            .unwrap_or_else(|| "the session's worker model".into()),
        effort: profile.effort.map(|effort| effort.to_string()),
        scope: place.reach().to_string(),
        description: one_line(&profile.description),
        limits: profile.limits(),
        instructions: profile.instructions.trim().to_string(),
    }
}

#[cfg(test)]
mod tests;

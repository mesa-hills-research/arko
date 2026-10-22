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
//! one of three places, and the nearest one wins when a name is in more
//! than one. The session's own folder (`profiles` in the session's folder)
//! beats the project's (`.agents/profiles` in the working directory), and
//! the project's beats the user's (`profiles` in Arko's folder).
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
//! | `cache` | `5m` or `1h`, the prompt cache of Claude workers on an API key |
//! | `nested` | `true` lets the worker start workers of its own |
//! | `isolated` | `true` has the worker work in a git worktree of its own |
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

use crate::agents::Effort;
use crate::providers;

/// The longest profile name, in characters.
pub const MAX_NAME: usize = 32;
/// The longest description, in characters.
pub const MAX_DESCRIPTION: usize = 120;
/// The longest instructions, in characters.
pub const MAX_INSTRUCTIONS: usize = 4_000;
/// The largest profile file Arko reads, in bytes.
pub const MAX_FILE_BYTES: u64 = 16 * 1024;
/// Names a profile can't take, because they mean something else to the
/// agents or the commands.
pub const RESERVED: [&str; 5] = ["default", "main", "worker", "lead", "all"];
/// The `made_by` value of a profile an agent wrote.
pub const MADE_BY_AGENT: &str = "agent";

/// The folder in the project (the working directory) that holds profiles.
const PROJECT_FOLDER: [&str; 2] = [".agents", "profiles"];
/// The folder in Arko's folder and in a session's folder.
const FOLDER: &str = "profiles";
const EXTENSION: &str = "md";

/// Where a profile lives. The places are listed nearest first, and the
/// nearest wins when a name is in more than one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Place {
    /// The session's own folder. An agent saves here without asking.
    Session,
    /// The project's `.agents/profiles`, shared with its team.
    Project,
    /// `profiles` in Arko's folder, for every project.
    User,
}

impl Place {
    /// Every place, nearest first.
    pub const ALL: [Place; 3] = [Place::Session, Place::Project, Place::User];

    /// The word the commands and the agent's tool take: `session`,
    /// `project` or `user`.
    pub fn name(self) -> &'static str {
        match self {
            Place::Session => "session",
            Place::Project => "project",
            Place::User => "user",
        }
    }

    /// Parses [`name`](Self::name), in any case.
    pub fn parse(text: &str) -> Option<Place> {
        Place::ALL
            .into_iter()
            .find(|place| place.name().eq_ignore_ascii_case(text.trim()))
    }

    /// Who can use a profile in this place, as a phrase that follows
    /// "for": `this session`, `this project` or `every project`.
    pub fn reach(self) -> &'static str {
        match self {
            Place::Session => "this session",
            Place::Project => "this project",
            Place::User => "every project",
        }
    }
}

/// How long a worker's prompt cache lasts, for the Claude models on an API
/// key or OpenRouter that take a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cache {
    FiveMinutes,
    OneHour,
}

impl Cache {
    /// `5m` or `1h`, as the file and the tools write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Cache::FiveMinutes => "5m",
            Cache::OneHour => "1h",
        }
    }
}

impl std::str::FromStr for Cache {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        match text.trim().to_ascii_lowercase().as_str() {
            "5m" => Ok(Cache::FiveMinutes),
            "1h" => Ok(Cache::OneHour),
            _ => Err(()),
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
    /// The worker works in a git worktree of its own.
    pub isolated: bool,
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
            isolated: false,
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
                "effort" => profile.effort = match value.as_str() {
                    "" => None,
                    other => Some(other.parse::<Effort>().map_err(|()| {
                        format!(
                            "`effort: {other}` isn't an effort (low, medium, high, xhigh or max)"
                        )
                    })?),
                },
                "fast" => profile.fast = boolean("fast", &value)?,
                "cache" => {
                    profile.cache = match value.as_str() {
                        "" => None,
                        other => Some(other.parse::<Cache>().map_err(|()| {
                            format!("`cache: {other}` isn't a prompt cache (5m or 1h)")
                        })?),
                    }
                }
                "nested" => profile.nested = boolean("nested", &value)?,
                "isolated" => profile.isolated = boolean("isolated", &value)?,
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
        if self.isolated {
            put("isolated", "true");
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
        if let Some(problem) = self.model_problem() {
            return Err(problem);
        }
        Ok(())
    }

    /// What is wrong with the profile's model and effort, when something is.
    /// The model must be one Arko knows, and the effort one it takes.
    pub fn model_problem(&self) -> Option<String> {
        let name = self.model.as_deref()?;
        let model = match providers::resolve_model(name) {
            Ok(model) => model,
            Err(error) => return Some(error),
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

const KNOWN_KEYS: [&str; 8] = [
    "description",
    "model",
    "effort",
    "fast",
    "cache",
    "nested",
    "isolated",
    "made_by",
];

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
        let mut out = String::new();
        let mut chars = value[1..value.len() - 1].chars();
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
    Ok(())
}

/// The three folders profiles are read from and saved in. The session's
/// and the project's are optional, as an app's settings page has neither.
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

    /// The folder that holds the profiles of `place`, whether it exists or
    /// not. `None` when there is no session or project.
    pub fn folder(&self, place: Place) -> Option<PathBuf> {
        match place {
            Place::Session => self.session.as_ref().map(|dir| dir.join(FOLDER)),
            Place::Project => self
                .workspace
                .as_ref()
                .map(|dir| dir.join(PROJECT_FOLDER[0]).join(PROJECT_FOLDER[1])),
            Place::User => Some(self.root.join(FOLDER)),
        }
    }
}

/// Every folder profiles are read from, nearest first, for "open the
/// folder" and for text that names them.
pub fn folders(roots: &Roots) -> Vec<(Place, PathBuf)> {
    Place::ALL
        .into_iter()
        .filter_map(|place| roots.folder(place).map(|folder| (place, folder)))
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
    for place in Place::ALL {
        let Some(folder) = roots.folder(place) else {
            continue;
        };
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
    for place in Place::ALL {
        let Some(folder) = roots.folder(place) else {
            continue;
        };
        let file = folder.join(Profile::file_name(name));
        if !file.is_file() {
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

/// A name that can be a file's name here: the file name rules without the
/// reserved names (those can't be saved, and a file by one is shown as
/// unreadable).
fn check_file_name(name: &str) -> Result<(), String> {
    match check_name(name) {
        Err(why) if !RESERVED.contains(&name) => Err(why),
        _ => Ok(()),
    }
}

fn read_file(place: Place, file: &Path, name: &str) -> Result<Found, Unreadable> {
    let unreadable = |reason: String| Unreadable {
        place,
        file: file.to_path_buf(),
        name: name.to_string(),
        reason,
    };
    if check_name(name).is_err() {
        // (The reserved names get their own reason.)
        return Err(unreadable(if RESERVED.contains(&name) {
            format!("its name, `{name}`, is reserved")
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
fn reason_of(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        std::io::ErrorKind::NotFound => "not found".to_string(),
        _ => error.to_string(),
    }
}

/// Who writes a profile. A user's edit takes the file whole. An agent's
/// can't replace what a user made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    let replaced = file.is_file();
    std::fs::create_dir_all(&folder)
        .map_err(|error| format!("Couldn't make {} ({})", folder.display(), reason_of(&error)))?;
    crate::session::write_whole(&file, written.render().as_bytes())
        .map_err(|error| format!("Couldn't write {} ({})", file.display(), reason_of(&error)))?;
    let hidden_by = Place::ALL
        .into_iter()
        .take_while(|nearer| *nearer < place)
        .find(|nearer| {
            roots
                .folder(*nearer)
                .is_some_and(|folder| folder.join(Profile::file_name(&profile.name)).is_file())
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
    for place in Place::ALL {
        let Some(folder) = roots.folder(place) else {
            continue;
        };
        let existing = folder.join(Profile::file_name(name));
        if !existing.is_file() {
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

/// Deletes the profile `name` from `place`. Returns the file it removed.
pub fn delete(roots: &Roots, place: Place, name: &str) -> Result<PathBuf, String> {
    check_file_name(name)?;
    let folder = roots
        .folder(place)
        .ok_or_else(|| format!("There is no {} to delete a profile from.", place.name()))?;
    let file = folder.join(Profile::file_name(name));
    if !file.is_file() {
        return Err(format!(
            "There is no profile `{name}` for {}.",
            place.reach()
        ));
    }
    std::fs::remove_file(&file)
        .map_err(|error| format!("Couldn't delete {} ({})", file.display(), reason_of(&error)))?;
    Ok(file)
}

/// The question that asks the user to approve the main agent's save of
/// `profile` in `place`. It reads the same in the terminal, on the web page
/// and in an app, and carries the whole profile so the user judges its
/// instructions as well as its model. Answer it with [`is_approval`].
pub fn approval_text(profile: &Profile, place: Place) -> String {
    let wherever = match place {
        Place::User => "in your Arko folder, for every project",
        Place::Project => "in this project's .agents/profiles folder, for everyone who works on it",
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
    if profile.isolated {
        text.push_str(" Its workers work in a worktree of their own.");
    }
    if profile.nested {
        text.push_str(" Its workers can start workers of their own.");
    }
    text.push('\n');
    let instructions = profile.instructions.trim();
    if instructions.is_empty() {
        text.push_str("\nIt has no standing instructions.\n");
    } else {
        text.push_str(&format!("\nInstructions\n{instructions}\n"));
    }
    text.push_str("\nReply yes to save it, or tell the agent what to change.");
    text
}

/// Whether the user's answer to [`approval_text`] approves it. Only "yes",
/// "y", "ok", "okay" or "allow" do, in any case and with punctuation after.
/// Anything else is the user's feedback for the agent, so "yes, but use
/// high effort" saves nothing.
pub fn is_approval(answer: &str) -> bool {
    let word = answer
        .trim()
        .trim_end_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_ascii_lowercase();
    matches!(word.as_str(), "yes" | "y" | "ok" | "okay" | "allow")
}

#[cfg(test)]
mod tests;

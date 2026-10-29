//! The main agent's two tools for skills. `add_skill` saves a skill for the
//! project (in its `.arko/skills`) or for every project (in Arko's
//! `skills`), and `remove_skill` removes one an agent saved.
//!
//! A skill comes from a folder with a `SKILL.md`, a Markdown file, a `.zip`
//! or `.skill` bundle, or an https URL of one of the files. Arko reads it
//! where it is, or into memory, by the code that loads skills, and checks it
//! before anyone is asked: its front matter, its name (one no other skill
//! has), its size, links and files. Nothing reaches a skill folder before the
//! user approves, except a folder the agent wrote at the project's target
//! itself, which is used in place (and deleted when the user turns it
//! down). A skill for every project, and a downloaded skill, always ask. A
//! project skill asks when the user turned on Review project skills. Files
//! are written without the executable bit, and nothing in a skill runs when
//! it is saved.
//!
//! The user approves a save in the session's approval tray, whose card
//! shows the skill's text and files.
//!
//! Each save is recorded in Arko's folder ([`agent_added`]), so only a skill
//! an agent saved can be removed by `remove_skill` or `/skills remove`.
//!
//! The tools are offered to the main agent from the start of every
//! conversation, with `load_skill`, so its tool list never changes and its
//! prompt cache holds. `load_skill` reads the skill folders at each call, so
//! it loads a saved skill at once, and the skill list in the system prompt
//! shows it from the next conversation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arko_agents::{
    TaskContext, Tool, ToolDefinition, ToolError, ToolParameterDefinition, ToolParameterType,
    ToolTypeDetails,
};
use async_trait::async_trait;
use serde_json::Value;

use crate::agent_added::{self, Scope, SkillRecord};
use crate::approvals::{Answer, Approval, SkillApproval, SkillFile, SkillSource};
use crate::builtin_skills::BuiltinSkills;
use crate::remote::Level;
use crate::skills;

pub const ADD_SKILL: &str = "add_skill";
pub const REMOVE_SKILL: &str = "remove_skill";
/// The main agent's tools for skills.
pub const TOOL_NAMES: [&str; 2] = [ADD_SKILL, REMOVE_SKILL];

/// The most files a skill may have.
const MAX_FILES: usize = 200;
/// The most a skill's folder or Markdown file may hold, and the most a
/// download or a bundle file may be.
const MAX_BYTES: u64 = 1024 * 1024;
/// The most a bundle may hold unpacked.
const MAX_UNPACKED: u64 = 4 * 1024 * 1024;
/// The longest name and description a skill may have.
const MAX_NAME: usize = 64;
const MAX_DESCRIPTION: usize = 1024;
/// How many redirects a download follows, and how long it may take.
const MAX_REDIRECTS: usize = 5;
const DOWNLOAD_TIME: Duration = Duration::from_secs(30);
/// How a zip archive starts.
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";

/// What `add_skill` tells the agent of itself.
const ADD_DESCRIPTION: &str = "Saves a skill that you and later conversations load with load_skill. Use it when instructions would help later conversations or workers, such as this project's release steps or a workaround for a tool. A SKILL.md opens with front matter that gives name, description (what it is for and when to load it, in one or two sentences) and, if needed, models. models lists the models whose agents see the skill in their skill list. An exact id such as claude-haiku-5-5 pins it to that release, which suits a workaround for one version's quirk. A short name or pattern such as haiku or gpt-6* follows the family across releases, which suits advice about a kind of model. Other agents can still load it by name. load_skill finds the skill at once, and the skill list shows it from the next conversation. Project skills go in the project's .arko/skills, which git ignores until the user turns off Keep agents' project files out of git in Arko's settings.";

/// Whom a save asks: the session's approval tray.
#[async_trait]
pub(crate) trait Approver: Send + Sync {
    /// Whether anyone can answer (there is a terminal, or an app).
    fn can_answer(&self) -> bool;
    /// Asks the user about `card`, for the call of `context`, and waits.
    async fn request(&self, context: &TaskContext<'_>, card: SkillApproval) -> Answer;
}

#[async_trait]
impl Approver for crate::approvals::Approvals {
    fn can_answer(&self) -> bool {
        crate::approvals::Approvals::can_answer(self)
    }

    async fn request(&self, context: &TaskContext<'_>, card: SkillApproval) -> Answer {
        crate::approvals::Approvals::request(self, context, Approval::Skill(card)).await
    }
}

/// What downloads a skill from a URL.
#[async_trait]
pub(crate) trait Fetch: Send + Sync {
    /// The body at `url`, at most [`MAX_BYTES`].
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// Downloads over https: redirects only to https and at most
/// [`MAX_REDIRECTS`] of them, [`DOWNLOAD_TIME`] in all, no cookies or
/// credentials, and at most [`MAX_BYTES`]. (Tests reach a local server over
/// plain http.)
pub(crate) struct Https;

/// Whether a download may go to `url`.
fn reachable(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        || (cfg!(test) && url.scheme() == "http" && url.host_str() == Some("127.0.0.1"))
}

#[async_trait]
impl Fetch for Https {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > MAX_REDIRECTS {
                attempt.error(format!("it redirects more than {MAX_REDIRECTS} times"))
            } else if !reachable(attempt.url()) {
                let to = attempt.url().to_string();
                attempt.error(format!("it redirects to {to}, which isn't https"))
            } else {
                attempt.follow()
            }
        });
        let client = arko_ai::http_client_builder()
            .redirect(policy)
            .timeout(DOWNLOAD_TIME)
            .build()
            .map_err(|error| error.to_string())?;
        let fetching = async {
            let mut response = client.get(url).send().await.map_err(|error| {
                // (The policy's own words, without reqwest's.)
                let mut source: Option<&dyn std::error::Error> = Some(&error);
                let mut deepest = error.to_string();
                while let Some(cause) = source {
                    deepest = cause.to_string();
                    source = cause.source();
                }
                deepest
            })?;
            if !response.status().is_success() {
                return Err(format!(
                    "the server answered {}",
                    response.status().as_u16()
                ));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
                body.extend_from_slice(&chunk);
                if body.len() as u64 > MAX_BYTES {
                    return Err(format!("it is over {}", size_words(MAX_BYTES)));
                }
            }
            Ok(body)
        };
        tokio::time::timeout(DOWNLOAD_TIME, fetching)
            .await
            .map_err(|_| "it took over 30 seconds".to_string())?
    }
}

/// Tells the user something, in the transcript.
pub(crate) type Notice = Arc<dyn Fn(Level, &str) + Send + Sync>;

/// What the tools need of the session.
pub(crate) struct Context {
    /// Arko's folder.
    pub root: PathBuf,
    /// The project.
    pub workspace: PathBuf,
    /// The user's home directory, for its skill folders and `~/`.
    pub home: Option<PathBuf>,
    /// The session's ID, and its name now.
    pub session: String,
    pub title: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// The names the main agent's model goes by, for its skill list.
    pub model_names: Vec<String>,
    pub approver: Arc<dyn Approver>,
    pub fetch: Arc<dyn Fetch>,
    /// Tells the user something, in the transcript.
    pub notice: Notice,
    /// Saves one at a time.
    pub writing: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Context")
    }
}

/// The two tools, for the main agent.
pub(crate) fn tools(context: Arc<Context>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(AddSkill(context.clone())),
        Arc::new(RemoveSkill(context)),
    ]
}

/// "this project" or "every project".
fn reach(scope: Scope) -> &'static str {
    match scope {
        Scope::Project => "this project",
        Scope::User => "every project",
    }
}

/// The folder skills of `scope` go in.
fn target(context: &Context, scope: Scope) -> PathBuf {
    match scope {
        Scope::Project => skills::project_folder(&context.workspace),
        Scope::User => skills::user_folder(&context.root),
    }
}

/// `path` absolute, without `.` and `..`, as records hold paths.
fn normal(path: &Path) -> PathBuf {
    arko_agents::utils::normalize_path(&std::path::absolute(path).unwrap_or(path.to_path_buf()))
}

/// The folder of the skill `name` saved for `scope`.
fn skill_dir(context: &Context, scope: Scope, name: &str) -> PathBuf {
    normal(&target(context, scope).join(name))
}

fn text_parameter(name: &str, description: &str, required: bool) -> ToolParameterDefinition {
    ToolParameterDefinition {
        name: name.into(),
        description: description.into(),
        param_type: ToolParameterType::String,
        required,
        is_path: false,
        flag: None,
        default_value: None,
    }
}

fn refused(why: impl Into<String>) -> ToolError {
    ToolError::ExecutionFailed(why.into())
}

/// The text parameter `key`, trimmed, `None` when left out or blank.
fn text<'a>(params: &'a HashMap<String, Value>, key: &str) -> Result<Option<&'a str>, ToolError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.trim()).filter(|value| !value.is_empty())),
        Some(_) => Err(refused(format!("{key} must be text"))),
    }
}

fn required<'a>(params: &'a HashMap<String, Value>, key: &str) -> Result<&'a str, ToolError> {
    text(params, key)?.ok_or_else(|| refused(format!("{key} is required")))
}

/// `workers`, as true or false, or as text that says one.
fn workers_parameter(params: &HashMap<String, Value>) -> Result<Option<bool>, ToolError> {
    match params.get("workers") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(Value::String(value)) => match value.trim().to_ascii_lowercase().as_str() {
            "" => Ok(None),
            "true" | "yes" => Ok(Some(true)),
            "false" | "no" => Ok(Some(false)),
            other => Err(refused(format!("workers is true or false, not {other}"))),
        },
        Some(_) => Err(refused("workers is true or false")),
    }
}

/// Why `name` can't name a skill, if it can't: kebab-case, letters and
/// digits in lower case and hyphens, up to [`MAX_NAME`] characters.
fn name_problem(name: &str) -> Option<String> {
    let kebab = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-');
    if !kebab {
        Some(format!(
            "{name} isn't a skill name. Use lowercase letters, digits and hyphens, such as release-notes."
        ))
    } else if name.len() > MAX_NAME {
        Some(format!(
            "The name is {} characters, and a skill's name is at most {MAX_NAME}.",
            name.len()
        ))
    } else {
        None
    }
}

/// Whether `entry` of `models` uses only the characters a pattern may have.
fn pattern_problem(entry: &str) -> bool {
    !entry
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ".-_/:*?".contains(c))
}

/// Where a skill to save comes from.
enum Origin {
    /// The folder the agent wrote at the target, used in place.
    InPlace,
    /// A folder or a file the agent gave.
    Copied(PathBuf),
    Downloaded {
        url: String,
        bytes: u64,
    },
}

/// A skill read from its source, before anything is written.
struct Draft {
    /// Its files by their paths in the skill (`/` between folders), with
    /// `SKILL.md` among them as given.
    files: Vec<(String, Vec<u8>)>,
    /// A bundle's bytes, unpacked once the save is approved.
    bundle: Option<Vec<u8>>,
    origin: Origin,
}

impl Draft {
    fn skill_md(&self) -> Option<&str> {
        self.files
            .iter()
            .find(|(path, _)| path == "SKILL.md")
            .and_then(|(_, bytes)| std::str::from_utf8(bytes).ok())
    }
}

/// What reading a skill's `SKILL.md` makes of it, by the loading code.
struct Parsed {
    name: String,
    description: String,
    auto_load: bool,
    models: Vec<String>,
    workers: bool,
}

/// Reads `text` as a `SKILL.md`, as loading reads one.
fn parse_skill_md(name: &str, text: &str) -> Result<Parsed, String> {
    let config = arko_agents::SkillsConfig {
        embedded: vec![arko_agents::EmbeddedSkill::new(name, text)],
        ..Default::default()
    };
    let scan = arko_agents::skill::scan(&config, Path::new("."));
    let Some(skill) = scan.into_iter().next() else {
        return Err("SKILL.md couldn't be read.".into());
    };
    if let Some(why) = skill.skipped {
        return Err(format!("SKILL.md doesn't load ({why})."));
    }
    Ok(Parsed {
        name: skill.name,
        description: skill.description,
        auto_load: skill.auto_load,
        models: skill.models,
        workers: skill.workers,
    })
}

/// `text` with its front matter's `workers` set: `workers: false` for
/// `false`, and no line (the default, true) for `true`. `None` when it has
/// no front matter.
fn set_workers(text: &str, workers: bool) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let opens = lines.first()?.trim_end();
    if opens != "---" {
        return None;
    }
    let close = lines
        .iter()
        .skip(1)
        .position(|line| line.trim_end() == "---")?
        + 1;
    let ending = if lines[0].ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut out = String::with_capacity(text.len() + 16);
    out.push_str(lines[0]);
    for line in &lines[1..close] {
        let key = line.split(':').next().unwrap_or_default();
        if key.trim_end() == "workers" && !line.starts_with([' ', '\t']) {
            continue;
        }
        out.push_str(line);
    }
    if !workers {
        out.push_str("workers: false");
        out.push_str(ending);
    }
    for line in &lines[close..] {
        out.push_str(line);
    }
    Some(out)
}

/// The files of the folder `root`, by their paths in it, without hidden
/// ones (as loading leaves them out). Refuses links and more than
/// [`MAX_FILES`] files or [`MAX_BYTES`].
fn read_folder(root: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    fn walk(
        root: &Path,
        folder: &Path,
        files: &mut Vec<(String, Vec<u8>)>,
        total: &mut u64,
    ) -> Result<(), String> {
        let mut entries: Vec<_> = std::fs::read_dir(folder)
            .map_err(|error| format!("{} can't be read ({error}).", folder.display()))?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let inside = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|part| part.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("/");
            let kind = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("{} can't be read ({error}).", path.display()))?;
            if kind.file_type().is_symlink() {
                return Err(format!(
                    "{inside} is a link. Copy what it points to into the skill instead."
                ));
            }
            if kind.is_dir() {
                walk(root, &path, files, total)?;
                continue;
            }
            if !kind.is_file() {
                return Err(format!("{inside} isn't a file or a folder."));
            }
            *total += kind.len();
            if files.len() >= MAX_FILES {
                return Err(format!(
                    "The skill has more than {MAX_FILES} files, the most a saved skill may have."
                ));
            }
            if *total > MAX_BYTES {
                return Err(format!(
                    "The skill's files hold more than {}, the most a saved skill may.",
                    size_words(MAX_BYTES)
                ));
            }
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("{} can't be read ({error}).", path.display()))?;
            files.push((inside, bytes));
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, root, &mut files, &mut 0)?;
    for (path, bytes) in &files {
        if path.to_ascii_lowercase().ends_with(".md") && std::str::from_utf8(bytes).is_err() {
            return Err(format!("{path} isn't UTF-8 text."));
        }
    }
    Ok(files)
}

/// The files of a bundle, read by unpacking it into a folder of its own
/// outside the skill folders, which is then deleted. Refuses more than
/// [`MAX_FILES`] files or [`MAX_UNPACKED`].
fn bundle_files(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let scratch = std::env::temp_dir().join(format!("arko-skill-{}", uuid::Uuid::new_v4()));
    let folder = scratch.join("skill");
    let unpacked = arko_agents::skill::unpack_bundle(bytes, &folder)
        .map_err(|error| format!("The bundle doesn't load ({error})."));
    let files = unpacked.and_then(|_| {
        let mut files = Vec::new();
        let mut total = 0u64;
        collect(&folder, &folder, &mut files, &mut total)?;
        Ok(files)
    });
    let _ = std::fs::remove_dir_all(&scratch);
    return files;

    fn collect(
        root: &Path,
        folder: &Path,
        files: &mut Vec<(String, Vec<u8>)>,
        total: &mut u64,
    ) -> Result<(), String> {
        let mut entries: Vec<_> = std::fs::read_dir(folder)
            .map_err(|error| error.to_string())?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                collect(root, &path, files, total)?;
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
            *total += bytes.len() as u64;
            if files.len() >= MAX_FILES {
                return Err(format!(
                    "The bundle has more than {MAX_FILES} files, the most a saved skill may have."
                ));
            }
            if *total > MAX_UNPACKED {
                return Err(format!(
                    "The bundle holds more than {} unpacked, the most a saved skill may.",
                    size_words(MAX_UNPACKED)
                ));
            }
            let inside = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|part| part.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("/");
            files.push((inside, bytes));
        }
        Ok(())
    }
}

/// `source` as a path: `~/` is the home directory, and a relative path is
/// in the project.
fn source_path(context: &Context, source: &str) -> PathBuf {
    let path = match (source.strip_prefix("~/"), &context.home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(source),
    };
    normal(&context.workspace.join(path))
}

/// Whether `source` is a URL.
fn is_url(source: &str) -> bool {
    source.contains("://")
}

/// Reads a bundle's `bytes` as loading would, then its files.
fn read_bundle_draft(bytes: Vec<u8>, origin: Origin) -> Result<Draft, String> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err(format!(
            "The bundle is over {}, the most a saved skill's bundle may be.",
            size_words(MAX_BYTES)
        ));
    }
    arko_agents::skill::read_bundle(&bytes)
        .map_err(|error| format!("The bundle doesn't load ({error})."))?;
    let files = bundle_files(&bytes)?;
    Ok(Draft {
        files,
        bundle: Some(bytes),
        origin,
    })
}

/// A Markdown skill's text, as `SKILL.md`.
fn markdown_draft(bytes: Vec<u8>, origin: Origin) -> Result<Draft, String> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err(format!(
            "The file is over {}, the most a saved skill may hold.",
            size_words(MAX_BYTES)
        ));
    }
    if std::str::from_utf8(&bytes).is_err() {
        return Err("The file isn't UTF-8 text.".into());
    }
    Ok(Draft {
        files: vec![("SKILL.md".to_string(), bytes)],
        bundle: None,
        origin,
    })
}

/// A skill to save, checked and ready to ask about.
struct Plan {
    name: String,
    scope: Scope,
    dest: PathBuf,
    draft: Draft,
    /// `SKILL.md` as it will be saved, with `workers` set.
    skill_md: String,
    parsed: Parsed,
    /// The record of the agent's skill it replaces.
    replaces: Option<SkillRecord>,
    why: String,
    /// What the agent gave as the source, for the record.
    source: Option<String>,
}

#[derive(Debug)]
struct AddSkill(Arc<Context>);

impl AddSkill {
    /// Reads the skill at `source` into a draft for the skill folder `dest`.
    async fn read(&self, source: &str, scope: Scope, dest: &Path) -> Result<Draft, String> {
        let context = &self.0;
        if is_url(source) {
            let url = reqwest::Url::parse(source)
                .map_err(|_| format!("{source} isn't a URL Arko can read."))?;
            if !reachable(&url) {
                return Err("Give an https URL.".into());
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err("Give the URL without a user name or password.".into());
            }
            let bytes = context
                .fetch
                .fetch(url.as_str())
                .await
                .map_err(|error| format!("Couldn't download {source}, because {error}."))?;
            let origin = Origin::Downloaded {
                url: source.to_string(),
                bytes: bytes.len() as u64,
            };
            if bytes.starts_with(ZIP_MAGIC) {
                return read_bundle_draft(bytes, origin);
            }
            let start = String::from_utf8_lossy(&bytes[..bytes.len().min(256)])
                .trim_start()
                .to_ascii_lowercase();
            if start.starts_with("<!doctype") || start.starts_with("<html") {
                return Err(format!(
                    "{source} is a web page, not a skill. Give the URL of the raw .md file or of a .zip or .skill bundle, or fetch the skill's folder with the shell and give the folder."
                ));
            }
            return markdown_draft(bytes, origin);
        }
        let path = source_path(context, source);
        let kind = std::fs::symlink_metadata(&path)
            .map_err(|_| format!("There's nothing at {}.", path.display()))?;
        if kind.file_type().is_symlink() {
            return Err(format!(
                "{} is a link. Give the folder or file it points to.",
                path.display()
            ));
        }
        if kind.is_dir() {
            let in_place = path == dest
                || std::fs::canonicalize(&path)
                    .is_ok_and(|real| std::fs::canonicalize(dest).is_ok_and(|dest| dest == real));
            if in_place && scope == Scope::User {
                return Err(
                    "Write the skill somewhere else, such as a scratch folder, and give that folder. Arko copies a skill for every project into its folder once the user approves it."
                        .into(),
                );
            }
            // (Links and sizes first, so the loader reads nothing they refuse.)
            let files = read_folder(&path)?;
            arko_agents::skill::read_candidate(&path)
                .map_err(|error| format!("The skill doesn't load ({error})."))?;
            let origin = if in_place {
                Origin::InPlace
            } else {
                Origin::Copied(path)
            };
            return Ok(Draft {
                files,
                bundle: None,
                origin,
            });
        }
        let lower = path.to_string_lossy().to_lowercase();
        if lower.ends_with(".zip") || lower.ends_with(".skill") {
            if kind.len() > MAX_BYTES {
                return Err(format!(
                    "The bundle is over {}, the most a saved skill's bundle may be.",
                    size_words(MAX_BYTES)
                ));
            }
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("{} can't be read ({error}).", path.display()))?;
            return read_bundle_draft(bytes, Origin::Copied(path));
        }
        if lower.ends_with(".md") {
            if kind.len() > MAX_BYTES {
                return Err(format!(
                    "The file is over {}, the most a saved skill may hold.",
                    size_words(MAX_BYTES)
                ));
            }
            arko_agents::skill::read_candidate(&path)
                .map_err(|error| format!("The skill doesn't load ({error})."))?;
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("{} can't be read ({error}).", path.display()))?;
            return markdown_draft(bytes, Origin::Copied(path));
        }
        Err(format!(
            "{} isn't a skill. Give a folder with a SKILL.md, a .md file, a .zip or .skill bundle, or an https URL of one.",
            path.display()
        ))
    }

    /// Why the skill `name` can't be saved in `dest` for `scope`, or the
    /// record of the agent's skill it replaces.
    fn place(
        &self,
        name: &str,
        scope: Scope,
        dest: &Path,
        origin: &Origin,
    ) -> Result<Option<SkillRecord>, String> {
        let in_place = matches!(origin, Origin::InPlace);
        let context = &self.0;
        if crate::builtin_skills::find(name).is_some() {
            return Err(format!(
                "{name} is the name of a skill built into Arko. Choose another name."
            ));
        }
        let other = match scope {
            Scope::Project => Scope::User,
            Scope::User => Scope::Project,
        };
        let other_dir = skill_dir(context, other, name);
        let replaces = agent_added::skill_at(&context.root, dest);
        let folders = skills::folders_in(
            Some(&context.workspace),
            &context.root,
            context.home.as_deref(),
        );
        let none = BuiltinSkills {
            none: true,
            off: Vec::new(),
        };
        if let Some(config) = skills::config(&folders, &none) {
            for skill in arko_agents::skill::scan(&config, &context.workspace) {
                let stem = skill
                    .path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().to_string());
                if skill.name != name && stem.as_deref() != Some(name) {
                    continue;
                }
                let path = normal(&skill.path);
                if path == dest {
                    if replaces.is_some() || in_place {
                        continue;
                    }
                    return Err(format!(
                        "The user has a skill named {name} in {}. Choose another name.",
                        path.display()
                    ));
                }
                if let Origin::Copied(source) = origin
                    && *source == path
                {
                    return Err(format!(
                        "{} is in a skill folder, so it is a skill already. Write the skill in another folder, such as a scratch folder, and give that folder.",
                        path.display()
                    ));
                }
                if path == other_dir && agent_added::skill_at(&context.root, &path).is_some() {
                    return Err(format!(
                        "An agent saved a skill named {name} for {} ({}). Remove it with remove_skill first, or choose another name.",
                        reach(other),
                        path.display()
                    ));
                }
                return Err(format!(
                    "The user has a skill named {name} in {}. Choose another name.",
                    path.display()
                ));
            }
        }
        if replaces.is_none() && !in_place && dest.symlink_metadata().is_ok() {
            return Err(format!(
                "{} is there already. Choose another name.",
                dest.display()
            ));
        }
        Ok(replaces)
    }

    /// Reads and checks the skill the call asks to save.
    async fn plan(&self, params: &HashMap<String, Value>) -> Result<Plan, ToolError> {
        let context = &self.0;
        let name = required(params, "name")?;
        let source = required(params, "source")?;
        let why = required(params, "why")?;
        let scope = match text(params, "scope")? {
            None | Some("project") => Scope::Project,
            Some("user") => Scope::User,
            Some(other) => {
                return Err(refused(format!("scope is project or user, not {other}")));
            }
        };
        let workers = workers_parameter(params)?;
        if let Some(problem) = name_problem(name) {
            return Err(refused(problem));
        }
        // Who would approve it, before anything is read.
        if !context.approver.can_answer() {
            if is_url(source) {
                return Err(refused(
                    "No one can approve a downloaded skill in this session. Tell the user where it is.",
                ));
            }
            if scope == Scope::User {
                return Err(refused(
                    "No one can approve a skill for every project in this session. Save it for the project, or tell the user what it would hold.",
                ));
            }
        }
        let dest = skill_dir(context, scope, name);
        let draft = self.read(source, scope, &dest).await.map_err(refused)?;
        let given = draft
            .skill_md()
            .ok_or_else(|| refused("The skill has no SKILL.md."))?
            .to_string();
        let skill_md = match workers {
            None => given,
            Some(workers) => set_workers(&given, workers).ok_or_else(|| {
                refused("SKILL.md has no front matter. It opens with a line of three dashes, the name and description, and another line of three dashes.")
            })?,
        };
        let parsed = parse_skill_md(name, &skill_md).map_err(refused)?;
        if parsed.name != name {
            return Err(refused(format!(
                "SKILL.md's front matter names the skill {}, and the name you gave is {name}. Make them the same.",
                parsed.name
            )));
        }
        if let Some(workers) = workers
            && parsed.workers != workers
        {
            return Err(refused(
                "Arko couldn't set workers in SKILL.md's front matter. Write workers: false there yourself.",
            ));
        }
        let length = parsed.description.chars().count();
        if length > MAX_DESCRIPTION {
            return Err(refused(format!(
                "The description is {} characters, and a skill's is at most {}. Say what it is for and when to load it in a sentence or two.",
                thousands(length),
                thousands(MAX_DESCRIPTION)
            )));
        }
        if parsed.auto_load {
            return Err(refused(
                "An agent's skill can't have auto_load: true, which would put it in every later system prompt. Take it out. The user can set it by hand.",
            ));
        }
        if let Some(entry) = parsed.models.iter().find(|entry| pattern_problem(entry)) {
            return Err(refused(format!(
                "models has {entry}, and an entry may have only letters, digits and . - _ / : * ?"
            )));
        }
        let replaces = self
            .place(name, scope, &dest, &draft.origin)
            .map_err(refused)?;
        // (A path as the user reads it.)
        let given = match &draft.origin {
            Origin::InPlace => None,
            Origin::Copied(path) => Some(with_tilde(path, context.home.as_deref())),
            Origin::Downloaded { url, .. } => Some(url.clone()),
        };
        Ok(Plan {
            name: name.to_string(),
            scope,
            dest,
            source: given,
            draft,
            skill_md,
            parsed,
            replaces,
            why: why.to_string(),
        })
    }

    /// The card for `plan`.
    fn card(&self, plan: &Plan) -> SkillApproval {
        let home = &self.0.home;
        let mut files: Vec<SkillFile> = vec![SkillFile {
            path: "SKILL.md".into(),
            bytes: plan.skill_md.len() as u64,
            text: Some(plan.skill_md.clone()),
        }];
        files.extend(
            plan.draft
                .files
                .iter()
                .filter(|(path, _)| path != "SKILL.md")
                .map(|(path, bytes)| SkillFile {
                    path: path.clone(),
                    bytes: bytes.len() as u64,
                    text: std::str::from_utf8(bytes).ok().map(String::from),
                }),
        );
        let scripts = plan
            .draft
            .files
            .iter()
            .map(|(path, _)| path)
            .filter(|path| is_script(path))
            .cloned()
            .collect();
        SkillApproval {
            name: plan.name.clone(),
            scope: match plan.scope {
                Scope::Project => crate::approvals::Scope::Project,
                Scope::User => crate::approvals::Scope::User,
            },
            description: plan.parsed.description.clone(),
            source: match &plan.draft.origin {
                Origin::InPlace => SkillSource::Agent,
                Origin::Copied(path) => SkillSource::Copied {
                    path: with_tilde(path, home.as_deref()),
                },
                Origin::Downloaded { url, bytes } => SkillSource::Downloaded {
                    url: url.clone(),
                    bytes: *bytes,
                },
            },
            workers: plan.parsed.workers,
            models: plan.parsed.models.clone(),
            scripts,
            replaces: plan.replaces.as_ref().map(|record| day(&record.added)),
            why: plan.why.clone(),
            files,
        }
    }

    /// Writes the skill of `plan` to its folder and records it.
    async fn save(&self, plan: &Plan) -> Result<(), String> {
        let context = &self.0;
        let _writing = context.writing.lock().await;
        let parent = match plan.scope {
            Scope::Project => {
                crate::arko_dir::ensure(&context.workspace, &context.root)
                    .map_err(|error| format!("{}.", error.trim_end_matches('.')))?;
                skills::project_folder(&context.workspace)
            }
            Scope::User => skills::user_folder(&context.root),
        };
        std::fs::create_dir_all(&parent)
            .map_err(|error| format!("Couldn't make {} ({error}).", parent.display()))?;
        if matches!(plan.draft.origin, Origin::InPlace) {
            if plan.draft.skill_md() != Some(plan.skill_md.as_str()) {
                let file = plan.dest.join("SKILL.md");
                crate::session::write_whole(&file, plan.skill_md.as_bytes())
                    .map_err(|error| format!("Couldn't write {} ({error}).", file.display()))?;
            }
        } else {
            // Another save may have taken the folder while the user read the
            // card.
            let recorded = agent_added::skill_at(&context.root, &plan.dest);
            if plan.dest.symlink_metadata().is_ok()
                && recorded.as_ref().map(|record| record.added)
                    != plan.replaces.as_ref().map(|record| record.added)
            {
                return Err(format!(
                    "{} changed while the user decided. Call add_skill again.",
                    plan.dest.display()
                ));
            }
            let staging = parent.join(format!(".{}.saving-{}", plan.name, uuid::Uuid::new_v4()));
            let written = self
                .stage(plan, &staging)
                .and_then(|()| put(&staging, &plan.dest));
            if let Err(error) = written {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Writes the files of `plan` to the new folder `staging`.
    fn stage(&self, plan: &Plan, staging: &Path) -> Result<(), String> {
        match &plan.draft.bundle {
            Some(bundle) => {
                arko_agents::skill::unpack_bundle(bundle, staging)
                    .map_err(|error| format!("Couldn't unpack the bundle ({error})."))?;
            }
            None => {
                for (path, bytes) in &plan.draft.files {
                    let file = staging.join(path);
                    if let Some(folder) = file.parent() {
                        std::fs::create_dir_all(folder).map_err(|error| {
                            format!("Couldn't make {} ({error}).", folder.display())
                        })?;
                    }
                    std::fs::write(&file, bytes)
                        .map_err(|error| format!("Couldn't write {} ({error}).", file.display()))?;
                }
            }
        }
        let file = staging.join("SKILL.md");
        std::fs::write(&file, plan.skill_md.as_bytes())
            .map_err(|error| format!("Couldn't write {} ({error}).", file.display()))
    }

    /// The note in the transcript and the result for the agent, once saved.
    fn saved(&self, plan: &Plan, words: Option<&str>) -> String {
        let context = &self.0;
        let mut notice = format!(
            "The main agent saved the skill {} for {}.",
            plan.name,
            reach(plan.scope)
        );
        if plan.scope == Scope::User {
            let home = &context.home;
            notice.push_str(&format!(
                " It is in {}.",
                with_tilde(&plan.dest, home.as_deref())
            ));
        }
        (context.notice)(Level::Info, &notice);
        let mut result = match &plan.replaces {
            None => format!(
                "Saved the skill {} for {}. load_skill loads it now, and your skill list shows it from the next conversation.",
                plan.name,
                reach(plan.scope)
            ),
            Some(_) => format!(
                "Saved the skill {} for {} in place of the one saved before. load_skill loads the new one now.",
                plan.name,
                reach(plan.scope)
            ),
        };
        if !arko_agents::skill::models_match(&plan.parsed.models, &context.model_names) {
            result.push_str(" Your skill list won't show it, since its models don't include yours, but load_skill loads it by name.");
        }
        result.push_str(if plan.parsed.workers {
            " Workers you start from now on get it."
        } else {
            " Workers don't get it."
        });
        let record = SkillRecord {
            path: plan.dest.clone(),
            scope: plan.scope,
            session: context.session.clone(),
            session_title: (context.title)(),
            added: chrono::Utc::now(),
            why: plan.why.clone(),
            source: plan.source.clone(),
        };
        if let Err(error) = agent_added::record_skill(&context.root, record) {
            result.push_str(&format!(
                " Arko couldn't record that an agent saved it ({error}), so only the user can remove it."
            ));
        }
        if let Some(words) = words.filter(|words| !words.trim().is_empty()) {
            result.push_str(&format!(
                " The user approved it and wrote \"{}\". Act on what they wrote.",
                words.trim()
            ));
        }
        result
    }

    /// What the agent reads when the skill wasn't saved. A draft the agent
    /// wrote in place is deleted, since nothing unapproved stays in a
    /// skill folder.
    fn not_saved(&self, plan: &Plan, answer: &Answer) -> String {
        let reach = reach(plan.scope);
        let mut result = match answer {
            Answer::Rejected { words: Some(words) } if !words.trim().is_empty() => {
                (self.0.notice)(Level::Info, "Not saved. Arko asks again.");
                format!(
                    "The user didn't save the skill {} for {reach} and wrote \"{}\". If those words ask for changes to the skill, make them and call add_skill again, which shows them the changed skill. If they don't (a new request, or words about something else), they are a message to you, not changes. Respond to it, call add_skill again with the same skill, and end with a short reminder that it is still waiting for their decision.",
                    plan.name,
                    words.trim()
                )
            }
            Answer::Rejected { .. } => format!(
                "The user chose not to save the skill {} for {reach}, so don't ask again unless they want it.",
                plan.name
            ),
            _ => format!(
                "The user didn't answer, so the skill {} wasn't saved.",
                plan.name
            ),
        };
        if matches!(plan.draft.origin, Origin::InPlace) {
            match std::fs::remove_dir_all(&plan.dest) {
                Ok(()) => result.push_str(&format!(
                    " Arko deleted your draft in {}, so write it again before you call add_skill again.",
                    plan.dest.display()
                )),
                Err(error) => result.push_str(&format!(
                    " Arko couldn't delete your draft in {} ({error}). Delete it with the shell.",
                    plan.dest.display()
                )),
            }
        }
        result
    }
}

/// Renames the folder `staging` to `dest`, in place of an earlier one.
fn put(staging: &Path, dest: &Path) -> Result<(), String> {
    let old = dest.with_file_name(format!(
        ".{}.old-{}",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let replacing = dest.symlink_metadata().is_ok();
    if replacing {
        std::fs::rename(dest, &old)
            .map_err(|error| format!("Couldn't replace {} ({error}).", dest.display()))?;
    }
    if let Err(error) = std::fs::rename(staging, dest) {
        if replacing {
            let _ = std::fs::rename(&old, dest);
        }
        return Err(format!("Couldn't save {} ({error}).", dest.display()));
    }
    if replacing {
        let _ = std::fs::remove_dir_all(&old);
    }
    Ok(())
}

/// Whether the file at `path` in a skill is a script an agent may run.
fn is_script(path: &str) -> bool {
    const EXTENSIONS: [&str; 13] = [
        "sh", "bash", "zsh", "fish", "py", "js", "mjs", "ts", "rb", "pl", "ps1", "bat", "cmd",
    ];
    path.starts_with("scripts/")
        || Path::new(path)
            .extension()
            .is_some_and(|extension| EXTENSIONS.contains(&extension.to_string_lossy().as_ref()))
}

#[async_trait]
impl Tool for AddSkill {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: ADD_SKILL.into(),
            description: ADD_DESCRIPTION.into(),
            output_based_example: None,
            parameters: vec![
                text_parameter(
                    "name",
                    "The skill's name in kebab-case, as its SKILL.md's front matter gives it.",
                    true,
                ),
                text_parameter(
                    "source",
                    "A folder you wrote with a SKILL.md, a Markdown file, or an https URL of a .md file or a .zip or .skill bundle.",
                    true,
                ),
                text_parameter(
                    "scope",
                    "project (the default, this project's .arko/skills) or user (every project, in Arko's folder, which the user approves).",
                    false,
                ),
                ToolParameterDefinition {
                    name: "workers".into(),
                    description: "false keeps it to you. Workers get it by default.".into(),
                    param_type: ToolParameterType::Bool,
                    required: false,
                    is_path: false,
                    flag: None,
                    default_value: None,
                },
                text_parameter(
                    "why",
                    "Why you add it, why this scope, and why workers do or don't get it, in a sentence or two the user reads.",
                    true,
                ),
            ],
            // Several saves of one response wait for the user together.
            parallel_safe: true,
            tool_type_details: ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: None,
        }
    }

    /// The call may wait for the user's answer, which no time limit cuts
    /// short.
    fn uses_host_timeout(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let plan = self.plan(&params).await?;
        let downloaded = matches!(plan.draft.origin, Origin::Downloaded { .. });
        let asks = plan.scope == Scope::User
            || downloaded
            || crate::extension_prefs::load(&self.0.root).review_project_skills;
        let mut words = None;
        if asks {
            if !self.0.approver.can_answer() {
                // (A user-scope or downloaded save was refused before.)
                let mut why = "No one can approve a skill in this session, and the user reviews the skills agents save. Tell the user what it would hold.".to_string();
                if matches!(plan.draft.origin, Origin::InPlace) {
                    let _ = std::fs::remove_dir_all(&plan.dest);
                    why.push_str(&format!(
                        " Arko deleted your draft in {}.",
                        plan.dest.display()
                    ));
                }
                return Err(refused(why));
            }
            match self.0.approver.request(context, self.card(&plan)).await {
                Answer::Approved { words: said, .. } => words = said,
                answer => return Ok(self.not_saved(&plan, &answer)),
            }
        }
        self.save(&plan).await.map_err(refused)?;
        Ok(self.saved(&plan, words.as_deref()))
    }
}

/// Removes the skill an agent saved in the folder `path`: deletes the
/// folder and drops its record. Refuses a skill no agent saved. For
/// `/skills remove` and an app's Remove (see
/// [`extensions`](crate::extensions)).
pub fn remove_saved(root: &Path, path: &Path) -> Result<String, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    if agent_added::skill_at(root, path).is_none() {
        return Err(format!("{name} wasn't added by an agent."));
    }
    std::fs::remove_dir_all(path).map_err(|error| {
        format!(
            "Couldn't delete {} ({}).",
            path.display(),
            crate::profiles::reason_of(&error)
        )
    })?;
    // (A record left behind is dropped once its folder is gone.)
    let _ = agent_added::forget_skill(root, path);
    Ok(format!("Removed the skill {name}."))
}

/// The record of the skill `name` an agent saved, for the project
/// `workspace` or for every project.
pub(crate) fn saved_skill(root: &Path, workspace: &Path, name: &str) -> Option<SkillRecord> {
    if name_problem(name).is_some() {
        return None;
    }
    [skills::project_folder(workspace), skills::user_folder(root)]
        .into_iter()
        .find_map(|folder| agent_added::skill_at(root, &normal(&folder.join(name))))
}

/// Where the skill `name` is, by the skill folders of the project
/// `workspace`, if there is one.
pub(crate) fn skill_path(
    root: &Path,
    workspace: &Path,
    home: Option<&Path>,
    name: &str,
) -> Option<PathBuf> {
    let folders = skills::folders_in(Some(workspace), root, home);
    let none = BuiltinSkills {
        none: true,
        off: Vec::new(),
    };
    let config = skills::config(&folders, &none)?;
    arko_agents::skill::scan(&config, workspace)
        .into_iter()
        .find(|skill| skill.name == name && skill.skipped.is_none())
        .map(|skill| skill.path)
}

#[derive(Debug)]
struct RemoveSkill(Arc<Context>);

#[async_trait]
impl Tool for RemoveSkill {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: REMOVE_SKILL.into(),
            description: "Removes a skill an agent saved with add_skill, for this project or every project. load_skill stops finding it at once, and the next conversation's skill list leaves it out.".into(),
            output_based_example: None,
            parameters: vec![
                text_parameter("name", "A skill an agent saved.", true),
                text_parameter("why", "Why, in a sentence the user reads.", true),
            ],
            parallel_safe: false,
            tool_type_details: ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: None,
        }
    }

    async fn execute(
        &self,
        _context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let context = &self.0;
        let name = required(&params, "name")?;
        required(&params, "why")?;
        let Some(record) = saved_skill(&context.root, &context.workspace, name) else {
            return Err(refused(
                match skill_path(
                    &context.root,
                    &context.workspace,
                    context.home.as_deref(),
                    name,
                ) {
                    Some(path) => format!(
                        "{name} wasn't saved by an agent, so remove_skill can't remove it. The user removes it by deleting {}.",
                        path.display()
                    ),
                    None if crate::builtin_skills::find(name).is_some() => format!(
                        "{name} is built into Arko. The user leaves it out with /builtin-skills {name} off."
                    ),
                    None => format!("There's no skill named {name}."),
                },
            ));
        };
        let _writing = context.writing.lock().await;
        remove_saved(&context.root, &record.path).map_err(refused)?;
        (context.notice)(
            Level::Info,
            &format!("The main agent removed the skill {name}."),
        );
        Ok(format!(
            "Removed the skill {name}. load_skill no longer finds it, and the next conversation's skill list leaves it out."
        ))
    }
}

/// `path`, with the home directory written as `~`.
pub(crate) fn with_tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

/// A day as the cards and lists say it, "26 Oct", in local time.
pub(crate) fn day(at: &chrono::DateTime<chrono::Utc>) -> String {
    at.with_timezone(&chrono::Local)
        .format("%-d %b")
        .to_string()
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

/// A size in bytes as people read it: "300 bytes", "14 KB", "1 MB".
fn size_words(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        let tenths = (bytes * 10).div_ceil(1024 * 1024);
        if tenths.is_multiple_of(10) {
            format!("{} MB", tenths / 10)
        } else {
            format!("{}.{} MB", tenths / 10, tenths % 10)
        }
    }
}

#[cfg(test)]
mod tests;

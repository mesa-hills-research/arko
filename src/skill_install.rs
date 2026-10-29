//! The main agent's two tools for skills. `add_skill` saves a skill for the
//! project (in its `.arko/skills`) or for every project (in Arko's
//! `skills`), and `remove_skill` removes one an agent saved.
//!
//! A skill comes from a folder with a `SKILL.md`, a Markdown file, a `.zip`
//! or `.skill` bundle, or an https URL of one of the files. A source in a
//! folder skills are read from is refused, since agents would load it
//! before anyone approved it, so an agent writes its draft somewhere else,
//! such as a scratch folder. Arko reads the source into memory once, by the
//! code that loads skills, and checks it before anyone is asked: its front
//! matter, its name (one no other skill has), its size, links and files.
//! That copy is what the card shows and what is saved, so a change to the
//! source after it was read isn't saved, and nothing reaches a skill folder
//! before the user approves. A skill for every project, and a downloaded
//! skill, always ask. A project skill asks when the user turned on Review
//! project skills. Files are written without the executable bit, and nothing
//! in a skill runs when it is saved.
//!
//! The user approves a save in the session's approval tray, whose card
//! shows the skill's text and files.
//!
//! Once approved, a save takes a lock in Arko's folder that every session's
//! saves and removals take ([`LOCK_FILE`]), checks that the skill's folder
//! is as it was when the card was made (not there, or the agent's skill it
//! replaces, unchanged), writes the skill into a hidden folder beside it and
//! renames that into place without replacing anything, then records it. A
//! change while the user decided stops the save, and the agent asks again.
//! Neither the skill's folder nor a folder on the way to it may be a link
//! (see [`arko_dir::folder`](crate::arko_dir::folder)).
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
use crate::arko_dir::Folder;
use crate::arko_dir::folder::{Entry, Kind, link_refused};
use crate::builtin_skills::BuiltinSkills;
use crate::profiles::reason_of;
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
/// The lock file in Arko's folder that saves and removals of skills take,
/// in every session, from the last check of a skill's folder to its record.
const LOCK_FILE: &str = "skills.lock";

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

/// The folder skills of `scope` go in, opened with no link on the way from
/// the project or Arko's folder. With `make` it is made (the project's
/// `.arko` as [`crate::arko_dir::ensure`] makes it), else it is `None` while
/// it isn't there.
fn open_target(context: &Context, scope: Scope, make: bool) -> Result<Option<Folder>, String> {
    let opened = match (scope, make) {
        (Scope::Project, true) => crate::arko_dir::ensure_dir(&context.workspace, &context.root)
            .and_then(|dir| dir.open(skills::SAVED_FOLDER, true))
            .map(|opened| opened.map(|(folder, _)| folder)),
        (Scope::Project, false) => crate::arko_dir::open_in(
            &context.workspace,
            &[crate::arko_dir::DIR, skills::SAVED_FOLDER],
            false,
        ),
        (Scope::User, make) => {
            crate::arko_dir::open_in(&context.root, &[skills::SAVED_FOLDER], make)
        }
    };
    opened.map_err(|error| sentence(&error))
}

/// `text` ending with a full stop.
fn sentence(text: &str) -> String {
    format!("{}.", text.trim_end_matches('.'))
}

/// Takes the lock of [`LOCK_FILE`], which is held until the file is dropped.
fn lock_saves(root: &Path) -> Result<std::fs::File, String> {
    let path = root.join(LOCK_FILE);
    let file = std::fs::create_dir_all(root)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
        })
        .map_err(|error| format!("Couldn't open {} ({}).", path.display(), reason_of(&error)))?;
    file.lock()
        .map_err(|error| format!("Couldn't lock {} ({}).", path.display(), reason_of(&error)))?;
    Ok(file)
}

/// What the folder of the agent's skill a save replaces held when the card
/// was made. A save goes ahead only while it holds the same, and deletes the
/// folder it put aside only when that still holds the same.
#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    /// The folder's device and inode, where the system has them.
    id: Option<(u64, u64)>,
    /// A hash of the names, kinds and bytes of everything in it.
    hash: [u8; 32],
}

/// The most a stamp reads of a folder: its files, folders and other
/// entries, their bytes, and how deep its folders go. A skill's folder that
/// holds more isn't replaced, since the user has put more in it than a save
/// does.
const STAMP_ENTRIES: usize = 1_000;
const STAMP_BYTES: u64 = 4 * MAX_UNPACKED;
const STAMP_DEPTH: usize = 16;

/// The stamp of the folder `name` in `folder`, read within the limits
/// above (`FileTooLarge` past them). Files are read in small pieces, and
/// only regular files are opened.
fn stamp(folder: &Folder, name: &str) -> std::io::Result<Stamp> {
    use sha2::Digest;
    use std::io::Read;
    struct Reading {
        hasher: sha2::Sha256,
        entries: usize,
        bytes: u64,
    }
    fn too_big() -> std::io::Error {
        std::io::Error::from(std::io::ErrorKind::FileTooLarge)
    }
    fn hash(folder: &Folder, reading: &mut Reading, depth: usize) -> std::io::Result<()> {
        if depth > STAMP_DEPTH {
            return Err(too_big());
        }
        // (Only as many names as the budget has left are read.)
        for name in folder.names_up_to(STAMP_ENTRIES - reading.entries)? {
            let Some(entry) = folder.entry(&name)? else {
                continue;
            };
            reading.entries += 1;
            if reading.entries > STAMP_ENTRIES {
                return Err(too_big());
            }
            reading.hasher.update(name.as_encoded_bytes());
            reading.hasher.update([0]);
            match entry.kind {
                Kind::Folder => {
                    reading.hasher.update(b"d");
                    if let Some((inner, _)) =
                        folder.open(&name, false).map_err(std::io::Error::other)?
                    {
                        hash(&inner, reading, depth + 1)?;
                    }
                    reading.hasher.update(b"/");
                }
                Kind::File => {
                    reading.hasher.update(b"f");
                    let mut file = folder.open_file(&name)?;
                    let mut buffer = [0u8; 64 * 1024];
                    let mut length = 0u64;
                    loop {
                        let read = file.read(&mut buffer)?;
                        if read == 0 {
                            break;
                        }
                        length += read as u64;
                        reading.bytes += read as u64;
                        if reading.bytes > STAMP_BYTES {
                            return Err(too_big());
                        }
                        reading.hasher.update(&buffer[..read]);
                    }
                    reading.hasher.update(length.to_le_bytes());
                }
                Kind::Link | Kind::Other => {
                    reading.hasher.update(b"l");
                    if let Some((device, inode)) = entry.id {
                        reading.hasher.update(device.to_le_bytes());
                        reading.hasher.update(inode.to_le_bytes());
                    }
                }
            }
        }
        Ok(())
    }
    let id = folder.entry(name)?.and_then(|entry| entry.id);
    let inner = folder
        .open(name, false)
        .map_err(std::io::Error::other)?
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?
        .0;
    let mut reading = Reading {
        hasher: sha2::Sha256::new(),
        entries: 0,
        bytes: 0,
    };
    hash(&inner, &mut reading, 0)?;
    Ok(Stamp {
        id,
        hash: reading.hasher.finalize().into(),
    })
}

/// How putting a staged skill in place of an agent's went wrong.
#[derive(Debug)]
enum NotSwapped {
    /// What was there isn't the folder the card showed. It is back.
    Changed,
    /// A step failed, and what was there is back.
    Failed(std::io::Error),
    /// A step failed, and what was there couldn't be put back, so it is
    /// kept under the hidden name `old`.
    Stranded { old: String, error: std::io::Error },
}

/// Puts the staged folder `staging` in place of the folder `name` in
/// `target`, which must be the one `id` names. That folder is renamed aside
/// to a hidden name and checked there, then the staged one is renamed into
/// place without replacing anything. A step that fails puts the folder
/// back, without replacing anything either. Returns the hidden name of the
/// folder put aside. (`displaced` runs once it is aside, for the tests.)
fn swap_in(
    target: &Folder,
    name: &str,
    staging: &str,
    id: Option<(u64, u64)>,
    displaced: impl FnOnce(),
) -> Result<String, NotSwapped> {
    let old = format!(".{name}.old-{}", uuid::Uuid::new_v4().simple());
    target.rename(name, &old).map_err(NotSwapped::Failed)?;
    let put_back = |error: Option<std::io::Error>| match target.rename_new(&old, name) {
        Ok(()) => error.map_or(NotSwapped::Changed, NotSwapped::Failed),
        Err(restoring) => NotSwapped::Stranded {
            old: old.clone(),
            error: error.unwrap_or(restoring),
        },
    };
    match target.entry(&old) {
        Ok(Some(Entry {
            kind: Kind::Folder,
            id: now,
        })) if now == id => {}
        Ok(_) => return Err(put_back(None)),
        Err(error) => return Err(put_back(Some(error))),
    }
    displaced();
    if let Err(error) = target.rename_new(staging, name) {
        return Err(put_back(Some(error)));
    }
    Ok(old)
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
    /// A folder or a file the agent gave.
    Copied(PathBuf),
    Downloaded {
        url: String,
        bytes: u64,
    },
}

/// A skill read from its source, before anything is written. It is the
/// snapshot the card shows and a save writes, held in memory, so nothing
/// can change it once read.
struct Draft {
    /// Its files by their paths in the skill (`/` between folders), with
    /// `SKILL.md` among them as given. A bundle's are those it unpacks to.
    files: Vec<(String, Vec<u8>)>,
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
    Ok(Draft { files, origin })
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
    /// What was at `dest` when the card was made: nothing, or the folder of
    /// the agent's skill it replaces.
    found: Option<Stamp>,
    why: String,
    /// What the agent gave as the source, for the record.
    source: String,
}

#[derive(Debug)]
struct AddSkill(Arc<Context>);

impl AddSkill {
    /// The folder skills are read from that holds `path`, if one does. A
    /// draft there would load before anyone approved it.
    fn skill_folder_holding(&self, path: &Path) -> Option<PathBuf> {
        let context = &self.0;
        let real = agent_added::real_path(path);
        skills::every_folder(&context.workspace, &context.root, context.home.as_deref())
            .into_iter()
            .map(|folder| normal(&folder))
            .find(|folder| {
                path.starts_with(folder) || real.starts_with(agent_added::real_path(folder))
            })
    }

    /// Reads the skill at `source` into a draft.
    async fn read(&self, source: &str) -> Result<Draft, String> {
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
        if let Some(folder) = self.skill_folder_holding(&path) {
            return Err(format!(
                "{} is in the skill folder {}. Write the skill somewhere else, such as a scratch folder in the project, and add it from there.",
                path.display(),
                folder.display()
            ));
        }
        if kind.is_dir() {
            // (Links and sizes first, so the loader reads nothing they refuse.)
            let files = read_folder(&path)?;
            arko_agents::skill::read_candidate(&path)
                .map_err(|error| format!("The skill doesn't load ({error})."))?;
            return Ok(Draft {
                files,
                origin: Origin::Copied(path),
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

    /// Why the skill `name` can't be saved in `dest` for `scope`, by the
    /// names of the other skills, or the record of the agent's skill it
    /// replaces. ([`found_at`] checks what is at `dest`.)
    fn place(&self, name: &str, scope: Scope, dest: &Path) -> Result<Option<SkillRecord>, String> {
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
                if path == dest && replaces.is_some() {
                    continue;
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
        // Where it would go, before anything is read.
        let target = open_target(context, scope, false).map_err(refused)?;
        let draft = self.read(source).await.map_err(refused)?;
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
        let replaces = self.place(name, scope, &dest).map_err(refused)?;
        let found = found_at(target.as_ref(), name, &dest, replaces.is_some()).map_err(refused)?;
        // (A path as the user reads it.)
        let given = match &draft.origin {
            Origin::Copied(path) => with_tilde(path, context.home.as_deref()),
            Origin::Downloaded { url, .. } => url.clone(),
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
            found,
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

    /// Saves the skill of `plan`, once the user approved it, and records it.
    /// It writes the snapshot into a hidden folder beside the skill's and
    /// checks that what is at the skill's place is still what the card
    /// showed. Then, under [`LOCK_FILE`], it checks that place again, renames
    /// the new folder into place without replacing anything (an agent's
    /// skill it replaces is first renamed aside and checked), and records the
    /// skill. A folder put aside is deleted after that, once it is checked to
    /// hold what the card showed, else it is kept.
    async fn save(&self, plan: &Plan) -> Result<Saved, String> {
        let context = &self.0;
        let _writing = context.writing.lock().await;
        // (No await from here on, so the save isn't dropped halfway.)
        let target = open_target(context, plan.scope, true)?
            .ok_or_else(|| format!("Couldn't make {}.", target(context, plan.scope).display()))?;
        let changed = || {
            format!(
                "{} changed while the user decided, so Arko didn't save the skill. Call add_skill again to ask again.",
                plan.dest.display()
            )
        };

        let staging = Owned {
            folder: &target,
            name: format!(".{}.saving-{}", plan.name, uuid::Uuid::new_v4().simple()),
        };
        let made = target.make(&staging.name).and_then(|()| {
            target
                .open(&staging.name, false)
                .map_err(std::io::Error::other)?
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        });
        let folder = made.map_err(|error| {
            format!(
                "Couldn't write the skill in {} ({}).",
                target.path().display(),
                reason_of(&error)
            )
        })?;
        self.stage(plan, &folder.0)?;
        // What is there is checked whole before the lock, which every
        // session's saves wait for.
        let now = found_at(Some(&target), &plan.name, &plan.dest, true).map_err(|_| changed())?;
        if now != plan.found {
            return Err(changed());
        }
        // A skill of the name the user added elsewhere while they decided.
        self.place(&plan.name, plan.scope, &plan.dest)
            .map_err(|why| format!("Arko didn't save the skill, since the skill folders changed while the user decided. {why}"))?;

        let lock = lock_saves(&context.root)?;
        // The place again, by what is there and its identity, and the record.
        let there = target
            .entry(&plan.name)
            .map_err(|_| changed())?
            .map(|entry| (entry.kind, entry.id));
        let expected = plan.found.as_ref().map(|stamp| (Kind::Folder, stamp.id));
        let recorded = agent_added::skill_at(&context.root, &plan.dest);
        fn same_record(
            record: &Option<SkillRecord>,
        ) -> Option<(&Path, chrono::DateTime<chrono::Utc>)> {
            record
                .as_ref()
                .map(|record| (record.path.as_path(), record.added))
        }
        if there != expected || same_record(&recorded) != same_record(&plan.replaces) {
            return Err(changed());
        }
        let not_saved = |error: std::io::Error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotFound
            ) {
                changed()
            } else {
                format!(
                    "Couldn't save {} ({}).",
                    plan.dest.display(),
                    reason_of(&error)
                )
            }
        };
        let mut aside = None;
        match &plan.found {
            None => target
                .rename_new(&staging.name, &plan.name)
                .map_err(not_saved)?,
            Some(stamp) => match swap_in(&target, &plan.name, &staging.name, stamp.id, || {}) {
                Ok(old) => aside = Some(old),
                Err(NotSwapped::Changed) => return Err(changed()),
                Err(NotSwapped::Failed(error)) => return Err(not_saved(error)),
                Err(NotSwapped::Stranded { old, error }) => {
                    return Err(format!(
                        "{} The skill that was there is in {} now. Move it back to {} to restore it.",
                        not_saved(error),
                        target.path().join(old).display(),
                        plan.dest.display()
                    ));
                }
            },
        }
        let unrecorded = match owned(&target, &plan.name) {
            Ok((path, id)) => {
                let record = SkillRecord {
                    path,
                    id,
                    scope: plan.scope,
                    session: context.session.clone(),
                    session_title: (context.title)(),
                    added: chrono::Utc::now(),
                    why: plan.why.clone(),
                    source: Some(plan.source.clone()),
                };
                agent_added::record_skill(&context.root, record).err()
            }
            Err(why) => Some(why),
        };
        drop(lock);
        // The folder put aside is read whole, so after the lock.
        let kept = aside.and_then(|old| {
            let same = stamp(&target, &old).ok() == plan.found;
            let id = plan.found.as_ref().and_then(|stamp| stamp.id);
            (!same || target.remove_all_of(&old, id).is_err()).then(|| target.path().join(old))
        });
        Ok(Saved { unrecorded, kept })
    }

    /// Writes the files of `plan` into the new folder `staging`, `SKILL.md`
    /// as the card showed it.
    fn stage(&self, plan: &Plan, staging: &Folder) -> Result<(), String> {
        let skill_md = ("SKILL.md".to_string(), plan.skill_md.as_bytes().to_vec());
        let files = plan
            .draft
            .files
            .iter()
            .filter(|(path, _)| path != "SKILL.md");
        for (path, bytes) in files.chain([&skill_md]) {
            write_in(staging, path, bytes).map_err(|error| {
                format!(
                    "Couldn't write {} ({}).",
                    plan.dest.join(path).display(),
                    reason_of(&error)
                )
            })?;
        }
        Ok(())
    }

    /// The note in the transcript and the result for the agent, once saved.
    fn saved(&self, plan: &Plan, words: Option<&str>, saved: Saved) -> String {
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
        // The skill it replaced, changed as it was put aside.
        let kept = saved.kept.as_ref().map(|old| {
            format!(
                " The skill it replaced changed as Arko replaced it, so its folder is kept in {}.",
                with_tilde(old, context.home.as_deref())
            )
        });
        if let Some(kept) = &kept {
            notice.push_str(kept);
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
        if let Some(kept) = &kept {
            result.push_str(kept);
        }
        if let Some(error) = saved.unrecorded {
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

    /// What the agent reads when the skill wasn't saved. Nothing was
    /// written, so nothing is deleted.
    fn not_saved(&self, plan: &Plan, answer: &Answer) -> String {
        let reach = reach(plan.scope);
        match answer {
            Answer::Rejected { words: Some(words) } if !words.trim().is_empty() => {
                (self.0.notice)(Level::Info, "Not saved.");
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
        }
    }
}

/// What is at the skill's folder `dest`, `name` in `target`, now: nothing,
/// or the folder of the agent's skill a save replaces (with `replaces`),
/// whose stamp it gives. Refuses a link, and anything else.
fn found_at(
    target: Option<&Folder>,
    name: &str,
    dest: &Path,
    replaces: bool,
) -> Result<Option<Stamp>, String> {
    let Some(target) = target else {
        return Ok(None);
    };
    let unread = |error: std::io::Error| {
        if error.kind() == std::io::ErrorKind::FileTooLarge {
            format!(
                "{} holds more than Arko reads before it replaces a skill ({} files and folders, {}, or {} folders deep). Ask the user to remove it, or choose another name.",
                dest.display(),
                thousands(STAMP_ENTRIES),
                size_words(STAMP_BYTES),
                STAMP_DEPTH
            )
        } else {
            format!("Couldn't read {} ({}).", dest.display(), reason_of(&error))
        }
    };
    match target.entry(name).map_err(unread)? {
        None => Ok(None),
        Some(Entry {
            kind: Kind::Link, ..
        }) => Err(format!("{} Choose another name.", link_refused(dest))),
        Some(Entry {
            kind: Kind::Folder, ..
        }) if replaces => stamp(target, name).map(Some).map_err(unread),
        Some(_) => Err(format!(
            "{} is there already. Choose another name.",
            dest.display()
        )),
    }
}

/// Writes `bytes` to the new file `path` (`/` between folders) in `folder`,
/// making the folders on the way.
fn write_in(folder: &Folder, path: &str, bytes: &[u8]) -> std::io::Result<()> {
    let parts: Vec<&str> = path.split('/').collect();
    let Some((file, folders)) = parts.split_last() else {
        return Ok(());
    };
    let mut opened: Option<Folder> = None;
    for part in folders {
        let at = opened.as_ref().unwrap_or(folder);
        let inner = at
            .open(part, true)
            .map_err(std::io::Error::other)?
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        opened = Some(inner.0);
    }
    opened.as_ref().unwrap_or(folder).create(file, bytes)
}

/// What a save did besides saving.
struct Saved {
    /// Why it couldn't be recorded, if it couldn't.
    unrecorded: Option<String>,
    /// Where the folder of the skill it replaced is kept, when it changed
    /// while the save replaced it.
    kept: Option<PathBuf>,
}

/// A hidden folder a save made beside a skill's, deleted when this is
/// dropped unless it was renamed away.
struct Owned<'a> {
    folder: &'a Folder,
    name: String,
}

impl Drop for Owned<'_> {
    fn drop(&mut self) {
        let _ = self.folder.remove_all(&self.name);
    }
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
                    "A folder with a SKILL.md or a Markdown file you wrote outside the skill folders, such as in a scratch folder, or an https URL of a .md file or a .zip or .skill bundle.",
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
                return Err(refused(
                    "No one can approve a skill in this session, and the user reviews the skills agents save. Tell the user what it would hold.",
                ));
            }
            match self.0.approver.request(context, self.card(&plan)).await {
                Answer::Approved { words: said, .. } => words = said,
                answer => return Ok(self.not_saved(&plan, &answer)),
            }
        }
        let saved = self.save(&plan).await.map_err(refused)?;
        Ok(self.saved(&plan, words.as_deref(), saved))
    }
}

/// Removes the skill an agent saved in the folder `path`: deletes the
/// folder and drops its record. Refuses a skill no agent saved, which is
/// found by the folder's real path, so a link that leads to another folder
/// isn't followed. For `/skills remove` and an app's Remove (see
/// [`extensions`](crate::extensions)).
pub fn remove_saved(root: &Path, path: &Path) -> Result<String, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let _lock = lock_saves(root)?;
    let Some(record) = agent_added::skill_at(root, path) else {
        return Err(format!(
            "{name} wasn't added by an agent. Delete {} to remove it.",
            path.display()
        ));
    };
    let folder = saved_in(root, &record)?;
    let saved = record.path.file_name().unwrap_or_default();
    // Only the very folder the agent saved, by its identity.
    let id = record.id.map(<(u64, u64)>::from);
    folder.remove_all_of(saved, id).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidData {
            format!(
                "Arko didn't delete {}, since it isn't the folder an agent saved there. Delete it by hand.",
                record.path.display()
            )
        } else {
            format!(
                "Couldn't delete {} ({}).",
                record.path.display(),
                reason_of(&error)
            )
        }
    })?;
    // (A record left behind is dropped once its folder is gone.)
    let _ = agent_added::forget_skill(root, &record.path);
    Ok(format!("Removed the skill {name}."))
}

/// The real path and identity of the folder `name` in `target`, for the
/// record of a save. The path is the one `target` had when it was opened,
/// and it must still name the folder reached through `target`, so a folder
/// on the way swapped for a link since then doesn't have the record claim
/// what the link leads to.
fn owned(target: &Folder, name: &str) -> Result<(PathBuf, Option<agent_added::FileId>), String> {
    let path = target.real().join(name);
    let id = target
        .entry(name)
        .ok()
        .flatten()
        .filter(|entry| entry.kind == Kind::Folder)
        .and_then(|entry| entry.id);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let named = std::fs::symlink_metadata(&path)
            .ok()
            .map(|meta| (meta.dev(), meta.ino()));
        if id.is_none() || named != id {
            return Err(format!(
                "{} no longer leads to the folder it saved in",
                path.display()
            ));
        }
    }
    Ok((path, id.map(agent_added::FileId::from)))
}

/// The folder that holds the skill of `record`, opened as a save opens it,
/// with no link on the way from the project or Arko's folder. Refuses a
/// record whose folder isn't where saves put skills.
fn saved_in(root: &Path, record: &SkillRecord) -> Result<Folder, String> {
    let parent = record.path.parent();
    let opened = match record.scope {
        // (`<project>/.arko/skills/<name>`.)
        Scope::Project => match parent.and_then(Path::parent).and_then(Path::parent) {
            Some(workspace) => crate::arko_dir::open_in(
                workspace,
                &[crate::arko_dir::DIR, skills::SAVED_FOLDER],
                false,
            ),
            None => Ok(None),
        },
        Scope::User => crate::arko_dir::open_in(root, &[skills::SAVED_FOLDER], false),
    }
    .map_err(|error| sentence(&error))?;
    match opened {
        Some(folder) if Some(folder.real()) == parent => Ok(folder),
        _ => Err(format!(
            "Arko didn't delete {}, since it isn't in a folder agents save skills in.",
            record.path.display()
        )),
    }
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

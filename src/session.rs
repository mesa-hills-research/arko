//! Sessions: one folder per session under the user's data directory, with
//! the settings it was started with (a resumed session keeps them), the
//! workflow's checkpoints, and the terminal jobs' logs.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::agents::{
    DEFAULT_WORKER_DEPTH, ExtraRules, MAX_WORKER_DEPTH, ModelChoice, ModelSettings,
};
use crate::builtin_skills::BuiltinSkills;
use crate::project;
use crate::providers::OpenAiAuth;
use crate::shell::WindowsShell;

pub(crate) const SETTINGS_FILE: &str = "session.json";
/// The name the user gave the session, when they renamed it: apart from
/// its settings, which the session rewrites while it runs.
const TITLE_FILE: &str = "title";

/// What a session is, an agent working in a folder with its tools and
/// workers or a chat with no tools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Agent,
    Chat,
}

impl Mode {
    fn is_agent(&self) -> bool {
        *self == Mode::Agent
    }
}
/// The session's log, in its folder.
#[cfg(feature = "tui")]
pub const LOG_FILE: &str = "arko.log";

/// A branch's conversation, in its new session's folder until the session
/// first opens and saves it (`Request::Branch`).
pub const SEED_FILE: &str = "seed.json";

/// What a session was started with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub workspace: PathBuf,
    pub main: ModelSettings,
    pub worker: ModelSettings,
    /// How many workers may run at once; 0 turns them off, and
    /// `agents::NO_LIMIT` sets no limit.
    pub max_workers: u32,
    pub handover_at: usize,
    /// The input size, in tokens, at which the main agent hands over, as
    /// `/handover-at` (or `--handover-at` when resuming) last set it; `None`:
    /// as its model settings say. The main agent's runs hand over at it in
    /// place of the size their saved configuration holds (see `handover`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_handover_at: Option<usize>,
    /// The input size, in tokens, at which the workers hand over, as
    /// `/handover-at` (or `--handover-at` when resuming) last set it; `None`:
    /// each as its model settings say. Every worker run, also one started
    /// before it was set, hands over at it in place of the size its
    /// configuration holds, up to what its own model leaves room for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers_handover_at: Option<usize>,
    /// Outside access, as the session last had it (`/outside`).
    pub allow_outside_workspace: bool,
    /// Show the agents' thinking whole as they work: the line prompt
    /// prints it, and the full-screen view opens it (else it shows folded,
    /// and the line prompt leaves it out).
    pub show_thinking: bool,
    /// The agents neither ask their models for the text of their thinking
    /// nor show it. False in sessions saved before it existed: their saved
    /// agents keep asking for what they asked for, and new workers ask.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide_thinking: bool,
    pub created: DateTime<Utc>,
    /// The start of the first message, to tell sessions apart.
    #[serde(default)]
    pub title: Option<String>,
    /// An agent's session or a chat (sessions saved before chats: an
    /// agent's).
    #[serde(default, skip_serializing_if = "Mode::is_agent")]
    pub mode: Mode,
    /// A chat's instructions from the user, for its next conversations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_instructions: Option<String>,
    /// The session switched between Chat and Agent: its conversations of
    /// either mode resume, each with its own configuration until its next
    /// message, which goes on in the session's mode.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub switched_mode: bool,
    /// The session this one branched from, at one of its messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branched_from: Option<String>,
    /// The shell commands run in on Windows. `None` in sessions saved
    /// before it could be chosen, which ran PowerShell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_shell: Option<WindowsShell>,
    /// How the session reaches OpenAI models, when it has one: the ChatGPT
    /// login or an API key. `None` in sessions without OpenAI models, and
    /// in those saved before it could be chosen, which used the ChatGPT
    /// login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_auth: Option<OpenAiAuth>,
    /// The workers' model was chosen on its own (`--worker-model`, `/model
    /// worker`), so it stays when the main agent's changes. False in
    /// sessions saved before it existed, whose workers followed the main
    /// agent when their models matched.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub own_worker_model: bool,
    /// The main agent ends its turns with a reply in plain text, instead of
    /// a `complete_task` call. It answers what the user types meanwhile
    /// with `message_user`, except in a session saved before it had that
    /// (its saved configuration says), which answers in text beside its
    /// tool calls. False in sessions saved before text replies existed,
    /// whose main agent calls `complete_task`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub text_replies: bool,
    /// The agents on `cc:` models hand over at the handover size, as on the
    /// other providers, instead of having Claude Code compact their
    /// conversation. False (they compact) in sessions saved before it
    /// existed, as their saved agents were built.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cc_handover: bool,
    /// Which agents get the extra rules for small models. Off in sessions
    /// saved before they existed, as their saved agents were built.
    #[serde(default, skip_serializing_if = "ExtraRules::is_off")]
    pub extra_rules: ExtraRules,
    /// Which of the project's instruction files (AGENTS.md, CLAUDE.md) the
    /// agents get. One in sessions saved before the choice, as they gave.
    #[serde(default, skip_serializing_if = "project::Files::is_one")]
    pub project_instructions: project::Files,
    /// Workers get the project's instructions too, in their first message.
    /// False in sessions saved before workers got them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worker_project_instructions: bool,
    /// After how many seconds a shell command still running moves to the
    /// background (0: never), as `/background-after` last set it. Ten
    /// minutes in sessions saved before it.
    #[serde(default = "default_background_after")]
    pub background_after: u64,
    /// How long after the user's last message a monitor's warning before it
    /// ends still wakes the agent, as `/monitor-wakes` last set it. Two
    /// hours in sessions saved before it.
    #[serde(default)]
    pub monitor_wakes: crate::monitors::Wakes,
    /// Who picks the model of a worker (`/worker-models`). Every worker runs
    /// the session's worker model in sessions saved before it existed, as
    /// those sessions always did and their agents were told.
    #[serde(default = "default_worker_models")]
    pub worker_models: ModelChoice,
    /// How many levels of workers the session allows (`/worker-depth`).
    /// Two in sessions saved before it existed.
    #[serde(
        default = "default_worker_depth",
        skip_serializing_if = "is_default_worker_depth"
    )]
    pub worker_depth: u32,
    /// The agent that started a worker gets a copy of the user's messages to
    /// it (`/message-copies`). Always saved, off in a new session. On in
    /// sessions saved before it existed, as they always did.
    #[serde(default = "default_message_copies")]
    pub message_copies: bool,
    /// The session was made by an Arko whose `start_agent` takes `nested`.
    /// A conversation in Claude Code keeps the tools it started with, so one
    /// in a session made before has no `nested` whatever the settings are.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nested_start: bool,
    /// Which of Arko's built-in skills the agents get, all unless some are
    /// turned off. All in sessions saved before they existed.
    #[serde(default, skip_serializing_if = "BuiltinSkills::is_all")]
    pub builtin_skills: BuiltinSkills,
}

fn default_background_after() -> u64 {
    crate::shell::DEFAULT_BACKGROUND_AFTER
}

fn default_worker_models() -> ModelChoice {
    ModelChoice::Session
}

fn default_worker_depth() -> u32 {
    DEFAULT_WORKER_DEPTH
}

fn is_default_worker_depth(depth: &u32) -> bool {
    *depth == DEFAULT_WORKER_DEPTH
}

fn default_message_copies() -> bool {
    true
}

impl Settings {
    /// Settings for tests: a session in `workspace` with both agents on
    /// `model`, no limit on workers, and nothing else on (not the built-in
    /// skills either). Tests change what they test.
    #[cfg(test)]
    pub fn for_test(workspace: &Path, model: &ModelSettings) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            main: model.clone(),
            worker: model.clone(),
            max_workers: crate::agents::NO_LIMIT,
            handover_at: 200_000,
            main_handover_at: None,
            workers_handover_at: None,
            allow_outside_workspace: false,
            show_thinking: false,
            hide_thinking: false,
            created: Utc::now(),
            title: None,
            mode: Mode::Agent,
            chat_instructions: None,
            switched_mode: false,
            branched_from: None,
            windows_shell: None,
            openai_auth: None,
            own_worker_model: false,
            text_replies: false,
            cc_handover: false,
            extra_rules: Default::default(),
            project_instructions: Default::default(),
            worker_project_instructions: false,
            background_after: 0,
            monitor_wakes: Default::default(),
            worker_models: ModelChoice::default(),
            worker_depth: DEFAULT_WORKER_DEPTH,
            message_copies: true,
            nested_start: true,
            builtin_skills: BuiltinSkills {
                none: true,
                off: Vec::new(),
            },
        }
    }

    /// Whether the workers run the main agent's model, and follow it when
    /// it changes before the conversation starts.
    pub fn workers_follow_main(&self) -> bool {
        !self.own_worker_model && self.worker.model == self.main.model
    }

    /// `model`'s settings as the banner and `/session` say them, with how
    /// the session reaches it when that isn't in its name (OpenAI models).
    pub fn describe(&self, model: &ModelSettings) -> String {
        let mut text = model.describe();
        if crate::providers::resolve_model(&model.model)
            .is_ok_and(|resolved| resolved.provider == arko_ai::Provider::OpenAI)
        {
            text.push_str(&format!(
                " via {}",
                self.openai_auth.unwrap_or(OpenAiAuth::ChatGpt).describe()
            ));
        }
        text
    }
}

/// A session's folder.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub dir: PathBuf,
}

impl Session {
    /// A new, empty session folder. Two sessions made in the same second
    /// don't share one, since the folder is taken only if it is new.
    pub fn create(root: &Path) -> std::io::Result<Self> {
        Self::create_named(root, || {
            format!(
                "{}-{}",
                Local::now().format("%Y%m%d-%H%M%S"),
                &uuid::Uuid::new_v4().simple().to_string()[..4]
            )
        })
    }

    /// [`create`](Self::create) with the IDs it tries. A folder that exists
    /// already is left as it is and the next ID is tried.
    fn create_named(root: &Path, mut next_id: impl FnMut() -> String) -> std::io::Result<Self> {
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions)?;
        let mut taken = 0;
        loop {
            let id = next_id();
            let dir = sessions.join(&id);
            match std::fs::create_dir(&dir) {
                Ok(()) => return Ok(Self { id, dir }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && taken < 100 => {
                    taken += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn open(root: &Path, id: &str) -> Result<Self, String> {
        if !is_session_id(id) {
            return Err(format!("`{id}` isn't a session's ID"));
        }
        let dir = root.join("sessions").join(id);
        if !dir.join(SETTINGS_FILE).is_file() {
            return Err(format!(
                "no session `{id}` (looked in {}). `arko --sessions` lists them",
                dir.display()
            ));
        }
        Ok(Self {
            id: id.to_string(),
            dir,
        })
    }

    /// The workflow's run directory.
    pub fn run_dir(&self) -> PathBuf {
        self.dir.join("run")
    }

    /// The terminal's jobs' folder: the terminal tools' background jobs and
    /// the `shell` tool's commands. (Sessions saved before the shell's
    /// commands could move to the background kept those in `commands`.)
    pub fn jobs_dir(&self) -> PathBuf {
        self.dir.join("jobs")
    }

    /// The checkpoint to resume from: the one `latest-checkpoint` names.
    pub fn latest_checkpoint(&self) -> Option<PathBuf> {
        latest_checkpoint_in(&self.run_dir())
    }

    /// When the session was last worked in: its latest checkpoint's time.
    /// `None` for a session closed before its first message.
    pub fn last_active(&self) -> Option<std::time::SystemTime> {
        std::fs::metadata(self.latest_checkpoint()?)
            .and_then(|metadata| metadata.modified())
            .ok()
    }

    /// The name the user gave the session, if they renamed it.
    pub fn renamed(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join(TITLE_FILE))
            .ok()
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty())
    }

    /// Renames the session `title` (an empty title: back to the one it
    /// takes from its conversation). A running session may be renamed: the
    /// name isn't in the settings it rewrites.
    pub fn rename(&self, title: &str) -> Result<(), String> {
        let path = self.dir.join(TITLE_FILE);
        let title = title.trim();
        if title.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(format!("could not rename the session ({error})"))
                }
                _ => Ok(()),
            };
        }
        write_whole(&path, title.as_bytes())
            .map_err(|error| format!("could not rename the session ({error})"))
    }

    pub fn load_settings(&self) -> Result<Settings, String> {
        let path = self.dir.join(SETTINGS_FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("could not read {} ({error})", path.display()))?;
        let mut settings: Settings = serde_json::from_str(&text)
            .map_err(|error| format!("could not read {} ({error})", path.display()))?;
        // (A depth edited by hand to one the settings can't show.)
        settings.worker_depth = settings.worker_depth.clamp(1, MAX_WORKER_DEPTH);
        Ok(settings)
    }

    pub fn save_settings(&self, settings: &Settings) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(settings)?;
        write_whole(&self.dir.join(SETTINGS_FILE), text.as_bytes())
    }
}

/// Writes `bytes` to `path` whole: through a temporary file of its own
/// beside it (two writers at once don't share one), renamed into place, so
/// a reader finds the old text or the new.
pub(crate) fn write_whole(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}.{count}.tmp", std::process::id()));
    let written =
        std::fs::write(&temporary, bytes).and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Writes `bytes` to `path` whole, as [`write_whole`] does, but only if
/// nothing is at `path` yet. Of two writers at once, the second gets
/// `AlreadyExists`, and the first one's file is not replaced. A file system
/// without hard links gets [`write_whole`], which can replace.
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}.{count}.new", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    let linked = std::fs::hard_link(&temporary, path);
    let _ = std::fs::remove_file(&temporary);
    match linked {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(error),
        Err(_) => write_whole(path, bytes),
        Ok(()) => Ok(()),
    }
}

/// The checkpoint `latest-checkpoint` names in a workflow's run directory.
pub fn latest_checkpoint_in(run: &Path) -> Option<PathBuf> {
    let name = std::fs::read_to_string(run.join("latest-checkpoint")).ok()?;
    let path = run.join(name.trim());
    path.is_file().then_some(path)
}

/// The folder sessions and the input history are kept in, which is
/// `ARKO_HOME`, else `arko` in the user's data directory.
pub fn data_root() -> Result<PathBuf, String> {
    if let Some(home) = std::env::var_os("ARKO_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    data_directory()
        .map(|base| base.join("arko"))
        .ok_or_else(|| "could not find your data directory. Set ARKO_HOME".to_string())
}

/// The user's data directory, which holds Arko's folder.
fn data_directory() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
    }
}

/// Moves the data folder of fin (Arko's name before) to Arko's, for folders
/// made before the rename. Only without ARKO_HOME. arko_workflows keeps its
/// own runs in Arko's folder too, so it may be there already. Then each of
/// fin's files and folders moves in unless Arko's has one by that name, and
/// a note says what stayed.
#[cfg(feature = "tui")]
pub fn move_fin_folder() -> Option<String> {
    if std::env::var_os("ARKO_HOME").is_some_and(|value| !value.is_empty()) {
        return None;
    }
    move_fin_folder_in(&data_directory()?)
}

#[cfg(feature = "tui")]
pub(crate) fn move_fin_folder_in(data_directory: &Path) -> Option<String> {
    let (fin, arko) = (data_directory.join("fin"), data_directory.join("arko"));
    if !fin.is_dir() {
        return None;
    }
    if !arko.exists() {
        let error = std::fs::rename(&fin, &arko).err()?;
        return Some(format!(
            "could not move fin's data folder {} to {} ({error}). Arko goes on without it",
            fin.display(),
            arko.display()
        ));
    }
    let mut stayed = Vec::new();
    for entry in std::fs::read_dir(&fin).ok()?.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let to = arko.join(entry.file_name());
        if to.exists() {
            stayed.push(format!("{name} (Arko's folder has one)"));
        } else if let Err(error) = std::fs::rename(entry.path(), &to) {
            stayed.push(format!("{name} ({error})"));
        }
    }
    if stayed.is_empty() {
        let _ = std::fs::remove_dir(&fin);
        return None;
    }
    Some(format!(
        "in {}, {} stayed when the rest of fin's data moved to {}",
        fin.display(),
        stayed.join(", "),
        arko.display()
    ))
}

/// The file an open session holds locked: one Arko (in any process) has a
/// session open at a time.
pub(crate) const LOCK_FILE: &str = "lock";
/// Who holds the lock, as they wrote it once they had it.
pub(crate) const HOLDER_FILE: &str = "holder.json";
/// How many more times a lock that is held is tried, and how long apart.
const LOCK_TRIES: u32 = 3;
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(20);

/// The lock the session's terminal holds while an Arko has it open: all
/// that an Arko from before session locks holds.
pub(crate) const TERMINAL_LOCK: &str = "jobs/.lock";

/// Whether `id` can name a session: a folder's name in `sessions/`, never
/// a path (no separators, not `.` or `..`).
pub fn is_session_id(id: &str) -> bool {
    let mut parts = Path::new(id).components();
    !id.contains(['/', '\\'])
        && matches!(
            (parts.next(), parts.next()),
            (Some(std::path::Component::Normal(name)), None) if name == id
        )
}

/// Who has a session open, with the app, its process and machine, and since
/// when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// The app's identity, `arko` for the command line, or the one an app
    /// gave ([`Options::app`](crate::Options::app)).
    pub app: String,
    /// The app's name, as people read it ("Arko in a terminal"). `None` in
    /// holder files written before Arko kept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub pid: u32,
    pub host: String,
    pub since: DateTime<Utc>,
}

impl Holder {
    /// This process, as the app `app` named `name`, from now.
    fn now(app: &str, name: &str) -> Holder {
        Holder {
            app: app.to_string(),
            name: Some(name.to_string()),
            pid: std::process::id(),
            host: host_name(),
            since: Utc::now(),
        }
    }

    /// One that holds the lock but hasn't said who it is (yet).
    #[cfg_attr(not(test), allow(dead_code))] // (The session list's, to come.)
    fn unknown() -> Holder {
        Holder {
            app: String::new(),
            name: None,
            pid: 0,
            host: String::new(),
            since: Utc::now(),
        }
    }

    /// As the user reads it, such as "Arko in a terminal (process 41233),
    /// since 14:02".
    pub fn describe(&self) -> String {
        // (Holder files written before the name was kept have the app's
        // identity alone.)
        let app = match (self.name.as_deref(), self.app.as_str()) {
            (Some(name), _) if !name.trim().is_empty() => name.to_string(),
            (_, "") => return "another Arko".to_string(),
            (_, "arko") => "Arko in a terminal".to_string(),
            (_, other) => other.to_string(),
        };
        let host = if self.host.is_empty() || self.host == host_name() {
            String::new()
        } else {
            format!(" on {}", self.host)
        };
        let since = self.since.with_timezone(&Local);
        let when = if since.date_naive() == Local::now().date_naive() {
            since.format("%H:%M").to_string()
        } else {
            since.format("%b %-d, %H:%M").to_string()
        };
        format!("{app} (process {}{host}), since {when}", self.pid)
    }
}

/// This machine's name, as far as Arko can tell.
fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut name = [0u8; 256];
        // SAFETY: the buffer is as long as the length given.
        if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } == 0 {
            let end = name
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(name.len());
            return String::from_utf8_lossy(&name[..end]).into_owned();
        }
        String::new()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

/// Why a session couldn't be locked.
#[derive(Debug)]
pub enum LockError {
    /// Another Arko has it open: who, when it said so.
    Held(Option<Holder>),
    Io(std::io::Error),
}

/// An open session's lock: it is released when this is dropped.
#[derive(Debug)]
pub struct SessionLock {
    /// `None` in a folder where the lock can't be made: the terminal's
    /// lock is all that holds the session, as before session locks.
    _file: Option<std::fs::File>,
    holder: Option<PathBuf>,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        if let Some(holder) = &self.holder {
            let _ = std::fs::remove_file(holder);
        }
    }
}

impl Session {
    /// Takes the session for the app `app` (its identity: `arko` for the
    /// command line) named `name` (as people read it) in this process, until
    /// the lock is dropped, and says who has it. A session open in another
    /// Arko, also in this process, can't be taken: who has it, if they said.
    pub fn lock(&self, app: &str, name: &str) -> Result<SessionLock, LockError> {
        let path = self.dir.join(LOCK_FILE);
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            // (A lock works on a file only read too: one in a folder only
            // read, or left by another user.)
            .or_else(|error| match error.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    std::fs::File::open(&path).map_err(|_| error)
                }
                _ => Err(error),
            });
        let file = match opened {
            Ok(file) => file,
            // A folder the lock can't be made in: the session opens as
            // before session locks, which its terminal's lock guards.
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                log::warn!(
                    "session {}: no lock in its folder ({error}): its terminal's lock holds it",
                    self.id
                );
                return Ok(SessionLock {
                    _file: None,
                    holder: None,
                });
            }
            Err(error) => return Err(LockError::Io(error)),
        };
        // Someone may be looking (the session list tries the lock for a
        // moment): a few tries before it counts as held.
        let mut tries = 0;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if tries < LOCK_TRIES => {
                    tries += 1;
                    std::thread::sleep(LOCK_WAIT);
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(LockError::Held(read_holder(&self.dir)));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(LockError::Io(error)),
            }
        }
        let holder = self.dir.join(HOLDER_FILE);
        let text = serde_json::to_string_pretty(&Holder::now(app, name))
            .map_err(|error| LockError::Io(std::io::Error::other(error)))?;
        // (Who has it, whole: never half written for one who reads it.)
        let holder = match write_whole(&holder, text.as_bytes()) {
            Ok(()) => Some(holder),
            Err(error) => {
                log::warn!(
                    "session {}: could not say who has it open: {error}",
                    self.id
                );
                None
            }
        };
        Ok(SessionLock {
            _file: Some(file),
            holder,
        })
    }

    /// Takes the session's terminal's lock, when it has a terminal: to
    /// delete it, as an Arko from before session locks holds only that one
    /// while it has the session open (`Held(None)`).
    pub(crate) fn lock_terminal(&self) -> Result<Option<std::fs::File>, LockError> {
        let file = match open_lock(&self.dir.join(TERMINAL_LOCK)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(LockError::Io(error)),
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Err(LockError::Held(None)),
            Err(std::fs::TryLockError::Error(error)) => Err(LockError::Io(error)),
        }
    }
}

/// Who has the session in `dir` open, if anyone does: its lock is held,
/// or (an Arko from before session locks, which doesn't say who it is) its
/// terminal's. (A holder file left without the lock, by an Arko that ended
/// without removing it, says nothing.)
pub fn holder(dir: &Path) -> Option<Holder> {
    if held(&dir.join(LOCK_FILE)) {
        return Some(read_holder(dir).unwrap_or_else(Holder::unknown));
    }
    held(&dir.join(TERMINAL_LOCK)).then(Holder::unknown)
}

/// Whether the lock file at `path` is held (also one that can only be
/// read).
fn held(path: &Path) -> bool {
    let Ok(file) = open_lock(path) else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// The lock file at `path`, as it is: to write, else (another user's, or
/// in a folder only read) to read, which a lock works on too.
fn open_lock(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .or_else(|error| match error.kind() {
            std::io::ErrorKind::PermissionDenied => std::fs::File::open(path).map_err(|_| error),
            _ => Err(error),
        })
}

fn read_holder(dir: &Path) -> Option<Holder> {
    let text = std::fs::read_to_string(dir.join(HOLDER_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The sessions in `root`, newest first, with their settings.
pub fn list(root: &Path) -> Vec<(Session, Settings)> {
    let Ok(entries) = std::fs::read_dir(root.join("sessions")) else {
        return Vec::new();
    };
    let mut sessions: Vec<(Session, Settings)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let id = entry.file_name().to_string_lossy().into_owned();
            let session = Session::open(root, &id).ok()?;
            let settings = session.load_settings().ok()?;
            Some((session, settings))
        })
        .collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.1.created));
    sessions
}

/// The session in `workspace` worked in last. Sessions closed before their
/// first message have nothing to continue, and don't count.
pub fn latest_in(root: &Path, workspace: &Path) -> Option<Session> {
    list(root)
        .into_iter()
        .filter(|(_, settings)| settings.workspace == workspace)
        .filter_map(|(session, _)| Some((session.last_active()?, session)))
        .max_by_key(|(active, _)| *active)
        .map(|(_, session)| session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session(root: &Path) -> Session {
        let session = Session::create(root).unwrap();
        let model = ModelSettings {
            model: crate::agents::resolve_model("luna").unwrap().id,
            effort: crate::agents::Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        session
            .save_settings(&Settings::for_test(root, &model))
            .unwrap();
        session
    }

    /// A session saved before workers' models could be chosen keeps every
    /// worker on the session's worker model, as it always did, and has two
    /// levels of workers. A session made since writes who chooses, and
    /// writes the depth only when it isn't the default.
    #[test]
    fn a_session_saved_before_worker_choices_loads_as_it_always_worked() {
        let temp = tempfile::tempdir().unwrap();
        let session = test_session(temp.path());
        let file = session.dir.join(SETTINGS_FILE);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"worker_models\": \"agent\""), "{text}");
        assert!(!text.contains("worker_depth"), "{text}");
        assert_eq!(
            session.load_settings().unwrap().worker_models,
            ModelChoice::Agent
        );
        // The file of a session from before has neither.
        let mut old: serde_json::Value = serde_json::from_str(&text).unwrap();
        old.as_object_mut().unwrap().remove("worker_models");
        std::fs::write(&file, serde_json::to_string_pretty(&old).unwrap()).unwrap();
        let loaded = session.load_settings().unwrap();
        assert_eq!(loaded.worker_models, ModelChoice::Session);
        assert_eq!(loaded.worker_depth, 2);

        let mut settings = loaded;
        settings.worker_models = ModelChoice::Profiles;
        settings.worker_depth = 3;
        session.save_settings(&settings).unwrap();
        let text = std::fs::read_to_string(session.dir.join(SETTINGS_FILE)).unwrap();
        assert!(text.contains("\"worker_models\": \"profiles\""), "{text}");
        assert!(text.contains("\"worker_depth\": 3"), "{text}");
        let loaded = session.load_settings().unwrap();
        assert_eq!(loaded.worker_models, ModelChoice::Profiles);
        assert_eq!(loaded.worker_depth, 3);
    }

    /// A session saved before message copies could be turned off has them on,
    /// as it always did. A session made now always writes the setting, so a
    /// reload gives the value it had.
    #[test]
    fn a_session_saved_before_message_copies_loads_with_them_on() {
        let temp = tempfile::tempdir().unwrap();
        let session = test_session(temp.path());
        let file = session.dir.join(SETTINGS_FILE);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"message_copies\": true"), "{text}");
        // The file of a session from before has no such setting.
        let mut old: serde_json::Value = serde_json::from_str(&text).unwrap();
        old.as_object_mut().unwrap().remove("message_copies");
        std::fs::write(&file, serde_json::to_string_pretty(&old).unwrap()).unwrap();
        let mut settings = session.load_settings().unwrap();
        assert!(settings.message_copies, "on without the setting");
        settings.message_copies = false;
        session.save_settings(&settings).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"message_copies\": false"), "{text}");
        assert!(!session.load_settings().unwrap().message_copies);
    }

    /// A session made now says its `start_agent` took `nested`, and one saved
    /// before doesn't.
    #[test]
    fn a_session_says_whether_its_start_agent_took_nested() {
        let temp = tempfile::tempdir().unwrap();
        let session = test_session(temp.path());
        assert!(session.load_settings().unwrap().nested_start);
        let file = session.dir.join(SETTINGS_FILE);
        let mut old: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("nested_start");
        std::fs::write(&file, serde_json::to_string_pretty(&old).unwrap()).unwrap();
        let loaded = session.load_settings().unwrap();
        assert!(!loaded.nested_start);
        // Saving it again doesn't make it a session of today.
        session.save_settings(&loaded).unwrap();
        assert!(!session.load_settings().unwrap().nested_start);
    }

    /// A worker depth edited by hand to 0 or past 4 loads as the nearest depth
    /// a session can have.
    #[test]
    fn a_worker_depth_outside_one_to_four_loads_as_the_nearest() {
        let temp = tempfile::tempdir().unwrap();
        let session = test_session(temp.path());
        let file = session.dir.join(SETTINGS_FILE);
        let text = std::fs::read_to_string(&file).unwrap();
        for (saved, loaded) in [(0, 1), (1, 1), (4, 4), (5, 4), (99, 4)] {
            let mut edited: serde_json::Value = serde_json::from_str(&text).unwrap();
            edited["worker_depth"] = saved.into();
            std::fs::write(&file, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
            assert_eq!(
                session.load_settings().unwrap().worker_depth,
                loaded,
                "{saved}"
            );
        }
    }

    /// Two that rename a session at once (an app and the session itself)
    /// both land, each whole: they don't share a temporary file.
    #[test]
    fn writes_at_once_each_land_whole() {
        let temp = tempfile::tempdir().unwrap();
        let session = test_session(temp.path());
        std::thread::scope(|scope| {
            for writer in 0..8 {
                let session = &session;
                scope.spawn(move || {
                    for n in 0..50 {
                        session.rename(&format!("Title {writer}-{n}")).unwrap();
                    }
                });
            }
        });
        assert!(session.renamed().unwrap().starts_with("Title "));
        let leftovers = std::fs::read_dir(&session.dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    /// A look at who has a session (the session list tries its lock for a
    /// moment) doesn't fail an Arko opening it then.
    #[test]
    fn a_look_at_the_lock_doesnt_fail_an_open() {
        let temp = tempfile::tempdir().unwrap();
        let session = test_session(temp.path());
        let look = std::fs::File::create(session.dir.join(LOCK_FILE)).unwrap();
        look.try_lock().unwrap();
        let looking = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(15));
            drop(look);
        });
        let lock = session.lock("arko", "Arko in a terminal").unwrap();
        looking.join().unwrap();
        assert_eq!(holder(&session.dir).unwrap().app, "arko");
        drop(lock);
    }

    /// A session in a folder that can only be read opens, as it did before
    /// session locks: its terminal's lock holds it.
    #[cfg(unix)]
    #[test]
    fn a_session_in_a_folder_only_read_opens() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let session = test_session(temp.path());
        let mode = |mode| std::fs::Permissions::from_mode(mode);
        std::fs::set_permissions(&session.dir, mode(0o555)).unwrap();
        let locked = session.lock("arko", "Arko in a terminal");
        std::fs::set_permissions(&session.dir, mode(0o755)).unwrap();
        let lock = locked.unwrap();
        drop(lock);
    }

    /// One Arko has a session open at a time, also in one process: another
    /// is told who has it, until it is closed. A holder file left behind
    /// without the lock (an Arko that ended without removing it) holds
    /// nothing.
    #[test]
    fn a_session_open_elsewhere_says_who_has_it() {
        let root = tempfile::tempdir().unwrap();
        let session = Session::create(root.path()).unwrap();
        assert_eq!(holder(&session.dir), None);
        let lock = session.lock("arko", "Arko in a terminal").unwrap();
        let held = holder(&session.dir).unwrap();
        assert_eq!((held.app.as_str(), held.pid), ("arko", std::process::id()));
        match session.lock("notes", "Notes") {
            Err(LockError::Held(Some(by))) => assert_eq!(by, held),
            other => panic!("{other:?}"),
        }
        assert!(held.describe().starts_with(&format!(
            "Arko in a terminal (process {}), since ",
            std::process::id()
        )));
        drop(lock);
        // (A process another test forks may hold the lock's file for a
        // moment, until it runs its program.)
        let released = std::time::Instant::now();
        while holder(&session.dir).is_some()
            && released.elapsed() < std::time::Duration::from_secs(2)
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(holder(&session.dir), None);
        assert!(!session.dir.join(HOLDER_FILE).exists());

        // A holder file without its lock, as an app named itself.
        let stale = Holder {
            app: "notes".into(),
            name: Some("Notes".into()),
            pid: 1,
            host: "elsewhere".into(),
            since: Utc::now(),
        };
        assert_eq!(
            stale.describe(),
            format!(
                "Notes (process 1 on elsewhere), since {}",
                Local::now().format("%H:%M")
            )
        );
        // One written before names were kept: the identity.
        let older: Holder = serde_json::from_str(
            r#"{"app": "notes", "pid": 1, "host": "", "since": "2026-10-16T12:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(older.name, None);
        assert!(older.describe().starts_with("notes (process 1), since "));
        std::fs::write(
            session.dir.join(HOLDER_FILE),
            serde_json::to_string(&stale).unwrap(),
        )
        .unwrap();
        assert_eq!(holder(&session.dir), None);
        let _lock = session.lock("notes", "Notes").unwrap();
        assert_eq!(holder(&session.dir).unwrap().pid, std::process::id());
        assert_eq!(Holder::unknown().describe(), "another Arko");
    }
    use crate::agents::Effort;

    fn settings(workspace: &Path, created: DateTime<Utc>) -> Settings {
        let model = ModelSettings {
            model: "claude-opus-5-5".into(),
            effort: Effort::Xhigh,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        Settings {
            max_workers: 4,
            created,
            ..Settings::for_test(workspace, &model)
        }
    }

    /// Gives `session` a checkpoint last written `ago` before now.
    fn checkpoint(session: &Session, ago: std::time::Duration) {
        let run = session.run_dir();
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("latest-checkpoint"), "checkpoint-1.json").unwrap();
        let file = std::fs::File::create(run.join("checkpoint-1.json")).unwrap();
        file.set_modified(std::time::SystemTime::now() - ago)
            .unwrap();
    }

    #[test]
    fn a_file_written_new_never_replaces_one() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("a.md");
        write_new(&file, b"first").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"first");
        let second = write_new(&file, b"second").unwrap_err();
        assert_eq!(second.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&file).unwrap(), b"first");
        // Nothing else is left in the folder.
        let left: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["a.md"]);
    }

    #[test]
    fn two_sessions_made_in_the_same_second_keep_their_own_folders() {
        let temp = tempfile::tempdir().unwrap();
        let first =
            Session::create_named(temp.path(), || "20261022-101010-aaaa".to_string()).unwrap();
        std::fs::write(first.dir.join("conversation.json"), "the first one's").unwrap();
        // The same ID again, as a clock and a short random part can give,
        // moves on to the next ID and doesn't touch the first folder.
        let mut ids = [
            "20261022-101010-aaaa",
            "20261022-101010-aaaa",
            "20261022-101010-bbbb",
        ]
        .into_iter();
        let second =
            Session::create_named(temp.path(), || ids.next().unwrap().to_string()).unwrap();
        assert_eq!(second.id, "20261022-101010-bbbb");
        assert_ne!(first.dir, second.dir);
        assert_eq!(
            std::fs::read_to_string(first.dir.join("conversation.json")).unwrap(),
            "the first one's"
        );
        assert!(std::fs::read_dir(&second.dir).unwrap().next().is_none());
        // A real run of many in one second all differ.
        let made: std::collections::HashSet<_> = (0..40)
            .map(|_| Session::create(temp.path()).unwrap().id)
            .collect();
        assert_eq!(made.len(), 40);
    }

    #[test]
    fn the_session_of_a_workspace_worked_in_last_is_found_and_its_settings_kept() {
        let root = tempfile::tempdir().unwrap();
        let here = root.path().join("here");
        let there = root.path().join("there");
        let now = Utc::now();
        let hour = std::time::Duration::from_secs(3_600);
        let older = Session::create(root.path()).unwrap();
        older
            .save_settings(&settings(&here, now - chrono::Duration::hours(3)))
            .unwrap();
        let newer = Session::create(root.path()).unwrap();
        newer
            .save_settings(&settings(&here, now - chrono::Duration::hours(2)))
            .unwrap();
        let elsewhere = Session::create(root.path()).unwrap();
        elsewhere.save_settings(&settings(&there, now)).unwrap();
        assert!(latest_in(root.path(), &here).is_none());
        checkpoint(&older, 2 * hour);
        checkpoint(&newer, 3 * hour);
        checkpoint(&elsewhere, std::time::Duration::ZERO);
        // The older session was worked in since the newer one.
        assert_eq!(latest_in(root.path(), &here).unwrap().id, older.id);
        checkpoint(&newer, hour);
        assert_eq!(latest_in(root.path(), &here).unwrap().id, newer.id);
        // One opened and closed without a message doesn't count.
        let empty = Session::create(root.path()).unwrap();
        empty.save_settings(&settings(&here, now)).unwrap();
        assert_eq!(latest_in(root.path(), &here).unwrap().id, newer.id);
        assert_eq!(list(root.path()).len(), 4);
        let loaded = Session::open(root.path(), &older.id)
            .unwrap()
            .load_settings()
            .unwrap();
        assert_eq!(loaded.workspace, here);
        assert!(Session::open(root.path(), "missing").is_err());
    }

    /// The OpenAI auth is saved only when chosen; sessions from before it
    /// load without it, and their OpenAI models used the ChatGPT login.
    #[test]
    fn the_openai_auth_is_saved_with_the_session() {
        let root = tempfile::tempdir().unwrap();
        let session = Session::create(root.path()).unwrap();
        let mut settings = settings(root.path(), Utc::now());
        session.save_settings(&settings).unwrap();
        let text = std::fs::read_to_string(session.dir.join(SETTINGS_FILE)).unwrap();
        assert!(!text.contains("openai_auth"), "{text}");
        assert_eq!(session.load_settings().unwrap().openai_auth, None);

        settings.openai_auth = Some(OpenAiAuth::ApiKey);
        settings.main.model = "gpt-6.1-sol".into();
        session.save_settings(&settings).unwrap();
        let text = std::fs::read_to_string(session.dir.join(SETTINGS_FILE)).unwrap();
        assert!(text.contains(r#""openai_auth": "api-key""#), "{text}");
        let loaded = session.load_settings().unwrap();
        assert_eq!(loaded.openai_auth, Some(OpenAiAuth::ApiKey));
        assert_eq!(
            loaded.describe(&loaded.main),
            "gpt-6.1-sol (xhigh effort) via OpenAI API key"
        );
        assert_eq!(
            loaded.describe(&loaded.worker),
            "claude-opus-5-5 (xhigh effort)"
        );
        settings.openai_auth = None;
        assert_eq!(
            settings.describe(&settings.main),
            "gpt-6.1-sol (xhigh effort) via ChatGPT login"
        );
    }

    /// The workers' handover size `/handover-at` set is kept with the
    /// session, for a resumed one to set again. A session saved before it
    /// has none: its workers go by their models' own sizes.
    #[test]
    fn the_workers_handover_size_is_saved_with_the_session() {
        let root = tempfile::tempdir().unwrap();
        let session = Session::create(root.path()).unwrap();
        let mut settings = settings(root.path(), Utc::now());
        session.save_settings(&settings).unwrap();
        let text = std::fs::read_to_string(session.dir.join(SETTINGS_FILE)).unwrap();
        assert!(!text.contains("workers_handover_at"), "{text}");
        assert_eq!(session.load_settings().unwrap().workers_handover_at, None);
        settings.main_handover_at = Some(950_000);
        settings.workers_handover_at = Some(950_000);
        session.save_settings(&settings).unwrap();
        let loaded = session.load_settings().unwrap();
        assert_eq!(loaded.main_handover_at, Some(950_000));
        assert_eq!(loaded.workers_handover_at, Some(950_000));
    }

    /// fin's data folder (Arko's name before) becomes Arko's once, with its
    /// sessions, keys and indexes, and the sessions list and open from it.
    #[test]
    fn fins_data_folder_moves_to_arkos_once() {
        let data = tempfile::tempdir().unwrap();
        let (fin, arko) = (data.path().join("fin"), data.path().join("arko"));
        let session = Session::create(&fin).unwrap();
        session
            .save_settings(&settings(data.path(), Utc::now()))
            .unwrap();
        std::fs::write(fin.join(".env"), "ANTHROPIC_API_KEY=sk-ant-1\n").unwrap();
        std::fs::create_dir_all(fin.join("indexes/work-code-1")).unwrap();

        assert_eq!(move_fin_folder_in(data.path()), None);
        assert!(!fin.exists());
        let listed = list(&arko);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0.id, session.id);
        assert_eq!(listed[0].1.workspace, data.path());
        assert!(Session::open(&arko, &session.id).is_ok());
        assert_eq!(
            std::fs::read_to_string(arko.join(".env")).unwrap(),
            "ANTHROPIC_API_KEY=sk-ant-1\n"
        );
        assert!(arko.join("indexes/work-code-1").is_dir());
        // Nothing to move the next time.
        assert_eq!(move_fin_folder_in(data.path()), None);
        assert_eq!(list(&arko).len(), 1);
    }

    /// arko_workflows' own runs may have made Arko's folder first. fin's
    /// data moves in beside them.
    #[test]
    fn fins_data_moves_into_a_folder_arko_workflows_made() {
        let data = tempfile::tempdir().unwrap();
        let (fin, arko) = (data.path().join("fin"), data.path().join("arko"));
        let session = Session::create(&fin).unwrap();
        session
            .save_settings(&settings(data.path(), Utc::now()))
            .unwrap();
        std::fs::write(fin.join(".env"), "ANTHROPIC_API_KEY=sk-ant-1\n").unwrap();
        std::fs::create_dir_all(arko.join("workflows/other")).unwrap();

        assert_eq!(move_fin_folder_in(data.path()), None);
        assert!(!fin.exists());
        assert_eq!(list(&arko).len(), 1);
        assert!(arko.join(".env").is_file());
        assert!(arko.join("workflows/other").is_dir());
    }

    /// What Arko's folder has already stays in fin's, the rest moves, and a
    /// note says what stayed.
    #[test]
    fn fins_data_that_arko_has_already_stays() {
        let data = tempfile::tempdir().unwrap();
        let (fin, arko) = (data.path().join("fin"), data.path().join("arko"));
        Session::create(&fin).unwrap();
        std::fs::write(fin.join("history.txt"), "hello\n").unwrap();
        std::fs::create_dir_all(arko.join("sessions")).unwrap();
        let note = move_fin_folder_in(data.path()).unwrap();
        assert_eq!(
            note,
            format!(
                "in {}, sessions (Arko's folder has one) stayed when the rest of fin's data moved to {}",
                fin.display(),
                arko.display()
            )
        );
        assert_eq!(std::fs::read_dir(fin.join("sessions")).unwrap().count(), 1);
        assert!(list(&arko).is_empty());
        assert!(arko.join("history.txt").is_file());
        assert!(!fin.join("history.txt").exists());
    }

    /// A move that fails is said, and Arko goes on with its own folder.
    #[cfg(unix)]
    #[test]
    fn a_move_that_fails_is_said() {
        use std::os::unix::fs::PermissionsExt;
        // Root renames in a folder it can't write to.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let data = tempfile::tempdir().unwrap();
        Session::create(&data.path().join("fin")).unwrap();
        let mode = |mode| std::fs::Permissions::from_mode(mode);
        std::fs::set_permissions(data.path(), mode(0o555)).unwrap();
        let note = move_fin_folder_in(data.path());
        std::fs::set_permissions(data.path(), mode(0o755)).unwrap();
        let note = note.unwrap();
        assert!(
            note.starts_with(&format!(
                "could not move fin's data folder {} to {} (",
                data.path().join("fin").display(),
                data.path().join("arko").display()
            )),
            "{note}"
        );
        assert!(note.ends_with(". Arko goes on without it"), "{note}");
        assert!(data.path().join("fin/sessions").is_dir());
    }
}

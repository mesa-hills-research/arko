//! Sessions: one folder per session under the user's data directory, with
//! the settings it was started with (a resumed session keeps them), the
//! workflow's checkpoints, and the terminal jobs' logs.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::agents::{ExtraRules, ModelSettings};
use crate::project;
use crate::providers::OpenAiAuth;
use crate::shell::WindowsShell;

const SETTINGS_FILE: &str = "session.json";

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
    /// a `complete_task` call, and answers what the user types meanwhile
    /// in text beside its tool calls instead of with `message_user`. False
    /// in sessions saved before it existed, whose main agent calls
    /// `complete_task`, as their saved configuration says.
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
}

fn default_background_after() -> u64 {
    crate::shell::DEFAULT_BACKGROUND_AFTER
}

impl Settings {
    /// Settings for tests: a session in `workspace` with both agents on
    /// `model`, no limit on workers, and nothing else on. Tests change what
    /// they test.
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
            windows_shell: None,
            openai_auth: None,
            own_worker_model: false,
            text_replies: false,
            cc_handover: false,
            extra_rules: Default::default(),
            project_instructions: Default::default(),
            worker_project_instructions: false,
            background_after: 0,
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
    /// A new, empty session folder.
    pub fn create(root: &Path) -> std::io::Result<Self> {
        let id = format!(
            "{}-{}",
            Local::now().format("%Y%m%d-%H%M%S"),
            &uuid::Uuid::new_v4().simple().to_string()[..4]
        );
        let dir = root.join("sessions").join(&id);
        std::fs::create_dir_all(&dir)?;
        Ok(Self { id, dir })
    }

    pub fn open(root: &Path, id: &str) -> Result<Self, String> {
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

    pub fn log_file(&self) -> PathBuf {
        self.dir.join("arko.log")
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

    pub fn load_settings(&self) -> Result<Settings, String> {
        let path = self.dir.join(SETTINGS_FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|error| format!("could not read {}: {error}", path.display()))
    }

    pub fn save_settings(&self, settings: &Settings) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(settings)?;
        let temporary = self.dir.join(format!("{SETTINGS_FILE}.tmp"));
        std::fs::write(&temporary, text)?;
        std::fs::rename(temporary, self.dir.join(SETTINGS_FILE))
    }
}

/// The checkpoint `latest-checkpoint` names in a workflow's run directory.
pub fn latest_checkpoint_in(run: &Path) -> Option<PathBuf> {
    let name = std::fs::read_to_string(run.join("latest-checkpoint")).ok()?;
    let path = run.join(name.trim());
    path.is_file().then_some(path)
}

/// The folder sessions and the input history are kept in: `ARKO_HOME`, or
/// `arko` in the user's data directory.
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
pub fn move_fin_folder() -> Option<String> {
    if std::env::var_os("ARKO_HOME").is_some_and(|value| !value.is_empty()) {
        return None;
    }
    move_fin_folder_in(&data_directory()?)
}

pub(crate) fn move_fin_folder_in(data_directory: &Path) -> Option<String> {
    let (fin, arko) = (data_directory.join("fin"), data_directory.join("arko"));
    if !fin.is_dir() {
        return None;
    }
    if !arko.exists() {
        let error = std::fs::rename(&fin, &arko).err()?;
        return Some(format!(
            "could not move fin's data folder {} to {}: {error}. Arko goes on without it",
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
        "some of fin's data stayed in {} when the rest moved to {}: {}",
        fin.display(),
        arko.display(),
        stayed.join(", ")
    ))
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
    sessions.sort_by(|a, b| b.1.created.cmp(&a.1.created));
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
                "some of fin's data stayed in {} when the rest moved to {}: sessions (Arko's folder has one)",
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
                "could not move fin's data folder {} to {}: ",
                data.path().join("fin").display(),
                data.path().join("arko").display()
            )),
            "{note}"
        );
        assert!(note.ends_with(". Arko goes on without it"), "{note}");
        assert!(data.path().join("fin/sessions").is_dir());
    }
}

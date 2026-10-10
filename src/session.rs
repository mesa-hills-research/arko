//! Sessions: one folder per session under the user's data directory, with
//! the settings it was started with (a resumed session keeps them), the
//! workflow's checkpoints, and the terminal jobs' logs.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::agents::ModelSettings;
use crate::shell::WindowsShell;

const SETTINGS_FILE: &str = "session.json";

/// What a session was started with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub workspace: PathBuf,
    pub main: ModelSettings,
    pub worker: ModelSettings,
    /// How many workers may run at once; 0 turns them off.
    pub max_workers: u32,
    pub handover_at: usize,
    pub allow_outside_workspace: bool,
    /// Show the models' reasoning as they work (OpenAI models then also
    /// ask for reasoning summaries).
    pub show_thinking: bool,
    pub created: DateTime<Utc>,
    /// The start of the first message, to tell sessions apart.
    #[serde(default)]
    pub title: Option<String>,
    /// The shell commands run in on Windows. `None` in sessions saved
    /// before it could be chosen, which ran PowerShell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_shell: Option<WindowsShell>,
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
                "no session `{id}` (looked in {}); `fin --sessions` lists them",
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

    /// The background jobs' folder (the terminal tools).
    pub fn jobs_dir(&self) -> PathBuf {
        self.dir.join("jobs")
    }

    /// The `shell` tool's commands' folder.
    pub fn commands_dir(&self) -> PathBuf {
        self.dir.join("commands")
    }

    pub fn log_file(&self) -> PathBuf {
        self.dir.join("fin.log")
    }

    /// The checkpoint to resume from: the one `latest-checkpoint` names.
    pub fn latest_checkpoint(&self) -> Option<PathBuf> {
        let run = self.run_dir();
        let name = std::fs::read_to_string(run.join("latest-checkpoint")).ok()?;
        let path = run.join(name.trim());
        path.is_file().then_some(path)
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

/// The folder sessions and the input history are kept in: `FIN_HOME`, or
/// `fin` in the user's data directory.
pub fn data_root() -> Result<PathBuf, String> {
    if let Some(home) = std::env::var_os("FIN_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let base = if cfg!(windows) {
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
    };
    base.map(|base| base.join("fin"))
        .ok_or_else(|| "could not find your data directory; set FIN_HOME".to_string())
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
        };
        Settings {
            workspace: workspace.to_path_buf(),
            main: model.clone(),
            worker: model,
            max_workers: 4,
            handover_at: 200_000,
            allow_outside_workspace: false,
            show_thinking: false,
            created,
            title: None,
            windows_shell: None,
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
}

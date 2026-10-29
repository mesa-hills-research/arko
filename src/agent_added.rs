//! The record of the skills agents saved, in `agent-added.json` in Arko's
//! folder. It lives there, and not in the skill, so that a skill committed
//! to a repository can't claim an agent saved it, and an agent can't remove
//! a user's skill by marking it. The file is written under a lock, as
//! `mcp-approvals.json` is, and an entry whose folder is gone is dropped
//! when the file is read.
//!
//! (A stand-in for the module of the same name on the MCP servers' branch,
//! with the same API for skills. That one also records servers.)

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The file in Arko's folder.
const FILE: &str = "agent-added.json";

/// Where an agent saved a skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// The project's `.arko/skills`.
    Project,
    /// Arko's `skills`, for every project.
    User,
}

/// A skill an agent saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRecord {
    /// Its folder, absolute.
    pub path: PathBuf,
    pub scope: Scope,
    /// The session that saved it.
    pub session: String,
    /// That session's name, when it had one.
    #[serde(default)]
    pub session_title: Option<String>,
    pub added: DateTime<Utc>,
    /// The agent's reason, for the user.
    pub why: String,
    /// What the agent gave as its source, a URL or a path, unless it wrote
    /// the skill in place.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    skills: Vec<SkillRecord>,
    /// The rest (the servers), kept as written.
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

fn read(root: &Path) -> Record {
    std::fs::read_to_string(root.join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The skills agents saved whose folders are still there.
pub fn skills(root: &Path) -> Vec<SkillRecord> {
    read(root)
        .skills
        .into_iter()
        .filter(|skill| skill.path.is_dir())
        .collect()
}

/// The record of the skill in the folder `path`, if an agent saved it.
pub fn skill_at(root: &Path, path: &Path) -> Option<SkillRecord> {
    skills(root).into_iter().find(|skill| skill.path == path)
}

/// Records a skill an agent saved, in place of an entry for its folder.
pub fn record_skill(root: &Path, record: SkillRecord) -> Result<(), String> {
    update(root, |skills| {
        skills.retain(|skill| skill.path != record.path);
        skills.push(record);
    })
    .map(|_| ())
}

/// Drops the record of the skill in `path`. Returns whether it had one.
pub fn forget_skill(root: &Path, path: &Path) -> Result<bool, String> {
    update(root, |skills| {
        let before = skills.len();
        skills.retain(|skill| skill.path != path);
        skills.len() != before
    })
}

fn update<T>(root: &Path, change: impl FnOnce(&mut Vec<SkillRecord>) -> T) -> Result<T, String> {
    let _lock = lock(root)?;
    let mut record = read(root);
    record.skills.retain(|skill| skill.path.is_dir());
    let result = change(&mut record.skills);
    let file = root.join(FILE);
    let text = serde_json::to_string_pretty(&record).map_err(|error| error.to_string())?;
    crate::session::write_whole(&file, (text + "\n").as_bytes())
        .map_err(|error| format!("Couldn't save {} ({error})", file.display()))?;
    Ok(result)
}

fn lock(root: &Path) -> Result<std::fs::File, String> {
    let path = root.join(format!("{FILE}.lock"));
    let file = std::fs::create_dir_all(root)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
        })
        .map_err(|error| format!("Couldn't open {} ({error})", path.display()))?;
    file.lock()
        .map_err(|error| format!("Couldn't lock {} ({error})", path.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &Path) -> SkillRecord {
        SkillRecord {
            path: path.to_path_buf(),
            scope: Scope::Project,
            session: "s1".into(),
            session_title: Some("Release prep".into()),
            added: Utc::now(),
            why: "Steps for later.".into(),
            source: None,
        }
    }

    #[test]
    fn a_record_is_kept_replaced_dropped_and_forgotten_with_its_folder() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let skill = temp.path().join("skills/notes");
        assert!(skills(&root).is_empty());
        std::fs::create_dir_all(&skill).unwrap();
        record_skill(&root, record(&skill)).unwrap();
        let mut changed = record(&skill);
        changed.why = "Changed.".into();
        record_skill(&root, changed).unwrap();
        assert_eq!(skills(&root).len(), 1);
        assert_eq!(skill_at(&root, &skill).unwrap().why, "Changed.");
        // Servers others wrote stay.
        let text = std::fs::read_to_string(root.join(FILE)).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["servers"] = serde_json::json!([{ "name": "github" }]);
        std::fs::write(root.join(FILE), value.to_string()).unwrap();
        std::fs::remove_dir_all(&skill).unwrap();
        assert!(skill_at(&root, &skill).is_none());
        assert!(!forget_skill(&root, &skill).unwrap());
        let text = std::fs::read_to_string(root.join(FILE)).unwrap();
        assert!(text.contains("github"));
        std::fs::create_dir_all(&skill).unwrap();
        record_skill(&root, record(&skill)).unwrap();
        assert!(forget_skill(&root, &skill).unwrap());
        assert!(skills(&root).is_empty());
    }
}

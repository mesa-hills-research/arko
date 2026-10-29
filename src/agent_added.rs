//! The record of what agents added: the skills an agent saved and the MCP
//! servers it added, in `agent-added.json` in Arko's folder.
//!
//! The record lives in Arko's folder rather than in the skill or the
//! server's entry, so a file committed to a cloned repository can't claim an
//! agent added it, and an agent can't remove a user's skill or server by
//! marking it. Only what the record names can be removed by an agent, or by
//! the user's remove command for what agents added.
//!
//! A skill is recorded by its folder's real path and the folder's identity
//! on its device ([`FileId`]), and a server by the real path of its file and
//! the identity of the folder that holds it, as the save or the add opened
//! them. Each is found by the real path ([`real_path`]) of the folder or
//! file asked about, so one reached through a link that leads elsewhere is
//! never taken for what an agent added, and a removal checks the identity on
//! what it opens, so a folder moved into the place isn't either.
//!
//! Each write takes a lock beside the record, as `mcp-approvals.json` does,
//! so sessions open at the same time keep each other's entries. An entry
//! whose skill folder is gone, or whose server's file no longer sets the
//! server up, is left out when the record is read, and dropped from the file
//! at its next write.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The record's file, in Arko's folder.
pub const FILE: &str = "agent-added.json";

/// Where an agent added something: this project, or every project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Project,
    User,
}

/// A file or folder's identity on its device, where the system has one (its
/// device and inode on Unix).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileId {
    pub device: u64,
    pub inode: u64,
}

impl From<(u64, u64)> for FileId {
    fn from((device, inode): (u64, u64)) -> Self {
        Self { device, inode }
    }
}

impl From<FileId> for (u64, u64) {
    fn from(id: FileId) -> Self {
        (id.device, id.inode)
    }
}

/// The identity of the file or folder at `path`, without following a link
/// at its end, where the system has one.
pub fn file_id(path: &Path) -> Option<FileId> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path)
            .ok()
            .map(|meta| FileId::from((meta.dev(), meta.ino())))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

impl SkillRecord {
    /// Whether it is the record of the skill in the folder `path`: the
    /// folder's real path is its path, and the folder is the one the agent
    /// saved, by its identity (a record without one goes by the path). A
    /// folder put in its place, such as the user's skill of that name, isn't.
    pub fn is_at(&self, path: &Path) -> bool {
        self.path == real_path(path) && self.id.is_none_or(|id| file_id(path) == Some(id))
    }
}

/// A skill an agent saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRecord {
    /// The skill's folder, by its real path.
    pub path: PathBuf,
    /// The folder's identity, where the system has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<FileId>,
    pub scope: Scope,
    /// The session it was saved in, by ID.
    pub session: String,
    /// That session's name when the skill was saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    pub added: DateTime<Utc>,
    /// Why, in the agent's words.
    pub why: String,
    /// Where the agent took it from (a URL, a folder or a file), if not its
    /// own writing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl ServerRecord {
    /// Whether its entry is in the file at `file`: the file's real path is
    /// its file, and the folder that really holds it is the one the add
    /// wrote in, by its identity (a record without one goes by the path). A
    /// folder moved into the place, such as another project's `.arko`,
    /// isn't.
    pub fn is_in(&self, file: &Path) -> bool {
        let real = real_path(file);
        self.file == real
            && self
                .folder
                .is_none_or(|id| real.parent().and_then(file_id) == Some(id))
    }
}

/// An MCP server an agent added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerRecord {
    /// The file its entry is in (a project's `.arko/mcp.json`, or Arko's
    /// `mcp.json`), by its real path.
    pub file: PathBuf,
    /// The identity of the folder the file is in, where the system has one.
    /// (The file's own changes with each save, as it is written whole.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<FileId>,
    pub name: String,
    /// The session it was added in, by ID.
    pub session: String,
    /// That session's name when the server was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    pub added: DateTime<Utc>,
    /// Why, in the agent's words.
    pub why: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    skills: Vec<SkillRecord>,
    #[serde(default)]
    servers: Vec<ServerRecord>,
}

impl Record {
    /// The record without the entries whose skill or server is gone.
    fn present(mut self) -> Self {
        self.skills.retain(|skill| skill.path.is_dir());
        self.servers
            .retain(|server| crate::mcp::sets_up(&server.file, &server.name));
        self
    }
}

fn path(root: &Path) -> PathBuf {
    root.join(FILE)
}

/// The record as the file holds it, or an empty one when there is none.
fn read(root: &Path) -> Result<Record, String> {
    let path = path(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Record::default());
        }
        Err(error) => {
            return Err(format!(
                "Couldn't read {} ({}).",
                path.display(),
                crate::profiles::reason_of(&error)
            ));
        }
    };
    serde_json::from_str(&text)
        .map_err(|error| format!("Couldn't read {} ({error}).", path.display()))
}

/// The record as it is now. One that can't be read counts as empty, and
/// says so in the log.
fn current(root: &Path) -> Record {
    match read(root) {
        Ok(record) => record.present(),
        Err(error) => {
            log::warn!("{error}");
            Record::default()
        }
    }
}

/// Changes the record with `change` under the lock, writing it back only
/// when `change` says it changed something, without the entries that are
/// gone.
fn update<T>(root: &Path, change: impl FnOnce(&mut Record) -> (T, bool)) -> Result<T, String> {
    let _lock = lock_record(root)?;
    let read = read(root)?;
    let before = (read.skills.len(), read.servers.len());
    let mut record = read.present();
    let pruned = before != (record.skills.len(), record.servers.len());
    let (result, changed) = change(&mut record);
    if changed || pruned {
        let text = serde_json::to_string_pretty(&record).map_err(|error| error.to_string())?;
        let path = path(root);
        crate::session::write_whole(&path, text.as_bytes()).map_err(|error| {
            format!(
                "Couldn't save {} ({}).",
                path.display(),
                crate::profiles::reason_of(&error)
            )
        })?;
    }
    Ok(result)
}

fn lock_record(root: &Path) -> Result<std::fs::File, String> {
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
        .map_err(|error| format!("Couldn't open {} ({error}).", path.display()))?;
    file.lock()
        .map_err(|error| format!("Couldn't lock {} ({error}).", path.display()))?;
    Ok(file)
}

/// `path` as it really is: absolute, without `.` and `..`, and with every
/// link on the way resolved, as far as it exists. A record holds a skill's
/// folder by it.
pub fn real_path(path: &Path) -> PathBuf {
    let normal = arko_agents::utils::normalize_path(
        &std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
    );
    // The part that exists, resolved, then the rest as it is.
    let mut rest = Vec::new();
    let mut at = normal.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(at) {
            return rest.iter().rev().fold(real, |path, part| path.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                at = parent;
            }
            _ => return normal,
        }
    }
}

/// The skills agents saved whose folders are still there.
pub fn skills(root: &Path) -> Vec<SkillRecord> {
    current(root).skills
}

/// The record of the skill in the folder `path`, if an agent saved it.
pub fn skill_at(root: &Path, path: &Path) -> Option<SkillRecord> {
    skills(root).into_iter().find(|skill| skill.is_at(path))
}

/// The one of `records` for the skill in the folder `path`, for a list of
/// skills.
pub fn added_skill<'a>(records: &'a [SkillRecord], path: &Path) -> Option<&'a SkillRecord> {
    records.iter().find(|record| record.is_at(path))
}

/// Records a skill an agent saved, in place of an entry for the same folder.
/// Its path is the folder's real path as the save opened and checked it,
/// kept as it is rather than resolved again, since a folder on the way may
/// have been swapped for a link since.
pub fn record_skill(root: &Path, record: SkillRecord) -> Result<(), String> {
    update(root, |kept| {
        kept.skills.retain(|skill| skill.path != record.path);
        kept.skills.push(record);
        ((), true)
    })
}

/// Drops the record of the skill in the folder `path`. Says whether it had
/// one.
pub fn forget_skill(root: &Path, path: &Path) -> Result<bool, String> {
    let real = real_path(path);
    update(root, |kept| {
        let before = kept.skills.len();
        kept.skills
            .retain(|skill| skill.path != path && skill.path != real);
        let removed = kept.skills.len() != before;
        (removed, removed)
    })
}

/// The MCP servers agents added that their files still set up.
pub fn servers(root: &Path) -> Vec<ServerRecord> {
    current(root).servers
}

/// The record of the server `name` in `file`, if an agent added it there
/// ([`ServerRecord::is_in`]).
pub fn server_in(root: &Path, file: &Path, name: &str) -> Option<ServerRecord> {
    servers(root)
        .into_iter()
        .find(|server| server.name == name && server.is_in(file))
}

/// Records a server an agent added, in place of an entry for the same name
/// in the same file. Its file is the real path its add opened.
pub fn record_server(root: &Path, record: ServerRecord) -> Result<(), String> {
    update(root, |kept| {
        kept.servers
            .retain(|server| !(server.file == record.file && server.name == record.name));
        kept.servers.push(record);
        ((), true)
    })
}

/// Drops the record of the server `name` in `file`. Says whether it had one.
pub fn forget_server(root: &Path, file: &Path, name: &str) -> Result<bool, String> {
    let real = real_path(file);
    update(root, |kept| {
        let before = kept.servers.len();
        kept.servers.retain(|server| {
            !((server.file == file || server.file == real) && server.name == name)
        });
        let removed = kept.servers.len() != before;
        (removed, removed)
    })
}

#[cfg(test)]
mod tests;

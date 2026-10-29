//! The project's `.arko` folder. Everything Arko writes into a project goes
//! there (worker profiles in `.arko/profiles`, skills an agent saves in
//! `.arko/skills`, MCP servers it adds in `.arko/mcp.json`), so other tools
//! that read `.agents` never see Arko's own files.
//!
//! [`ensure`] makes the folder before a write. While "Keep agents' project
//! files out of git" is on, which is the default, the folder it makes gets a
//! `.gitignore` that ignores everything in the folder, itself included. It
//! is written only when the folder is made, so one the user deleted to share
//! the files isn't written again. [`apply_setting`] adds or removes the file
//! when the setting changes. A file the user edited is never removed.
//!
//! The setting is the `keep_out_of_git` field of
//! [`Preferences`](crate::profiles::Preferences), in `profiles.json` in
//! Arko's folder. Everything here reads and writes files, so call it off a
//! UI thread.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::profiles::{Preferences, reason_of};

/// The folder's name, in the project (the working directory).
pub const DIR: &str = ".arko";
/// The file that keeps the folder out of git.
pub const IGNORE_FILE: &str = ".gitignore";
/// What Arko writes in [`IGNORE_FILE`]. The `*` ignores the folder's files
/// and this one.
pub const IGNORE_TEXT: &str = "# Arko keeps its project files out of git. Delete this file, or turn off\n# Keep agents' project files out of git in Arko's settings, to share them.\n*\n";

/// The `.arko` folder of the project `workspace`, whether it exists or not.
pub fn dir(workspace: &Path) -> PathBuf {
    workspace.join(DIR)
}

/// Makes the project's `.arko` folder if it isn't there and returns it. Arko's
/// folder `root` holds the setting. With the setting on, a folder this makes
/// gets its `.gitignore`. With it off, Arko's own `.gitignore` is taken out
/// of a folder that has it, which is how a project other than the session's
/// follows the switch.
pub fn ensure(workspace: &Path, root: &Path) -> Result<PathBuf, String> {
    let dir = dir(workspace);
    // (The folder's own creation tells whether it was made here.)
    let made = match std::fs::create_dir(&dir) {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if !dir.is_dir() {
                return Err(format!("{} isn't a folder.", dir.display()));
            }
            false
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            std::fs::create_dir_all(&dir).map_err(|error| made_not(&dir, &error))?;
            true
        }
        Err(error) => return Err(made_not(&dir, &error)),
    };
    if keep_out_of_git(root) {
        if made {
            write_ignore(&dir)?;
        }
    } else {
        // A file that can't be taken out doesn't stop the write that asked.
        let _ = remove_ignore(&dir);
    }
    Ok(dir)
}

/// Follows the switch for the project `workspace`. On writes Arko's
/// `.gitignore` in its `.arko` folder when the folder has none, and off takes
/// it out while its text is still Arko's. A project with no `.arko` folder is
/// left as it is, since [`ensure`] reads the setting when it makes one.
pub fn apply_setting(workspace: &Path, on: bool) -> Result<(), String> {
    let dir = dir(workspace);
    if !dir.is_dir() {
        return Ok(());
    }
    if on {
        write_ignore(&dir)
    } else {
        remove_ignore(&dir)
    }
}

/// Whether the project's `.arko` folder has Arko's own `.gitignore`, which
/// the user hasn't edited.
pub(crate) fn has_own_ignore(workspace: &Path) -> bool {
    is_own_ignore(&dir(workspace))
}

fn is_own_ignore(dir: &Path) -> bool {
    std::fs::read(dir.join(IGNORE_FILE)).is_ok_and(|text| text == IGNORE_TEXT.as_bytes())
}

/// Whether "Keep agents' project files out of git" is on in Arko's folder
/// `root`.
fn keep_out_of_git(root: &Path) -> bool {
    Preferences::load(root).keep_out_of_git
}

fn made_not(dir: &Path, error: &std::io::Error) -> String {
    format!("Couldn't make {} ({})", dir.display(), reason_of(error))
}

/// Writes the `.gitignore` in `dir` unless something is there.
fn write_ignore(dir: &Path) -> Result<(), String> {
    let file = dir.join(IGNORE_FILE);
    // (A write that made the folder a moment before has written it.)
    match crate::session::write_new(&file, IGNORE_TEXT.as_bytes()) {
        Err(error) if error.kind() != ErrorKind::AlreadyExists => Err(format!(
            "Couldn't write {} ({})",
            file.display(),
            reason_of(&error)
        )),
        _ => Ok(()),
    }
}

/// Deletes the `.gitignore` in `dir` while its text is Arko's.
fn remove_ignore(dir: &Path) -> Result<(), String> {
    let file = dir.join(IGNORE_FILE);
    if is_own_ignore(dir) {
        match std::fs::remove_file(&file) {
            Err(error) if error.kind() != ErrorKind::NotFound => {
                return Err(format!(
                    "Couldn't delete {} ({})",
                    file.display(),
                    reason_of(&error)
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

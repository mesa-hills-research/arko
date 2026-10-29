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
//! Arko never writes through a link here. A `.arko` that is a link, or a
//! folder in it that is one, is refused with the reason, since it could
//! lead a write out of the project, such as into Arko's own skills. The
//! folders are opened as a [`Folder`], which writes relative to the opened
//! folder (see [`folder`]).
//!
//! The setting is the `keep_out_of_git` field of
//! [`Preferences`](crate::profiles::Preferences), in `profiles.json` in
//! Arko's folder. Everything here reads and writes files, so call it off a
//! UI thread.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::profiles::{Preferences, reason_of};

pub(crate) mod folder;

pub(crate) use folder::Folder;

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
/// follows the switch. Refuses a `.arko` that is a link or isn't a folder.
pub fn ensure(workspace: &Path, root: &Path) -> Result<PathBuf, String> {
    Ok(ensure_dir(workspace, root)?.path().to_path_buf())
}

/// Makes the folder `name` in the project's `.arko` folder, as [`ensure`]
/// makes that, and returns it. Refuses either when it is a link or isn't a
/// folder.
pub fn ensure_in(workspace: &Path, root: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = ensure_dir(workspace, root)?;
    match dir.open(name, true)? {
        Some((folder, _)) => Ok(folder.path().to_path_buf()),
        None => Err(made_not(&dir.path().join(name), "it isn't there")),
    }
}

/// [`ensure`], with the folder opened.
pub(crate) fn ensure_dir(workspace: &Path, root: &Path) -> Result<Folder, String> {
    let path = dir(workspace);
    // A workspace that isn't there yet is made with the folder.
    if let Err(error) = std::fs::create_dir_all(workspace) {
        return Err(made_not(&path, &reason_of(&error)));
    }
    let base = Folder::base(workspace).map_err(|error| made_not(&path, &reason_of(&error)))?;
    // (The folder's own creation tells whether it was made here.)
    let Some((dir, made)) = base.open(DIR, true)? else {
        return Err(made_not(&path, "it isn't there"));
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

/// The folder `names` lead to from `base`, each opened as [`Folder::open`]
/// does, so none of them may be a link. With `make` the missing ones are
/// made, else a missing one gives `None`. A `base` that isn't there gives
/// `None` too, unless `make` makes it.
pub(crate) fn open_in(base: &Path, names: &[&str], make: bool) -> Result<Option<Folder>, String> {
    if make && let Err(error) = std::fs::create_dir_all(base) {
        return Err(made_not(base, &reason_of(&error)));
    }
    let mut folder = match Folder::base(base) {
        Ok(folder) => folder,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Couldn't open {} ({}).",
                base.display(),
                reason_of(&error)
            ));
        }
    };
    for name in names {
        match folder.open(name, make)? {
            Some((inner, _)) => folder = inner,
            None => return Ok(None),
        }
    }
    Ok(Some(folder))
}

/// Follows the switch for the project `workspace`. On writes Arko's
/// `.gitignore` in its `.arko` folder when the folder has none, and off takes
/// it out while its text is still Arko's. A project with no `.arko` folder is
/// left as it is, since [`ensure`] reads the setting when it makes one. A
/// `.arko` that is a link is refused.
pub fn apply_setting(workspace: &Path, on: bool) -> Result<(), String> {
    // (A file of the folder's name is no folder to follow it.)
    if std::fs::symlink_metadata(dir(workspace))
        .is_ok_and(|meta| !meta.is_dir() && !meta.file_type().is_symlink())
    {
        return Ok(());
    }
    let Some(dir) = open_in(workspace, &[DIR], false)? else {
        return Ok(());
    };
    if on {
        write_ignore(&dir)
    } else {
        remove_ignore(&dir)
    }
}

/// Whether the project's `.arko` folder has Arko's own `.gitignore`, which
/// the user hasn't edited. A `.arko` that is a link has none.
pub(crate) fn has_own_ignore(workspace: &Path) -> bool {
    open_in(workspace, &[DIR], false)
        .ok()
        .flatten()
        .is_some_and(|dir| is_own_ignore(&dir))
}

fn is_own_ignore(dir: &Folder) -> bool {
    dir.read(IGNORE_FILE)
        .is_ok_and(|text| text == IGNORE_TEXT.as_bytes())
}

/// Whether "Keep agents' project files out of git" is on in Arko's folder
/// `root`.
fn keep_out_of_git(root: &Path) -> bool {
    Preferences::load(root).keep_out_of_git
}

fn made_not(dir: &Path, why: &str) -> String {
    format!("Couldn't make {} ({why})", dir.display())
}

/// Writes the `.gitignore` in `dir` unless something is there.
fn write_ignore(dir: &Folder) -> Result<(), String> {
    // (A write that made the folder a moment before has written it.)
    match dir.create_whole(IGNORE_FILE, IGNORE_TEXT.as_bytes()) {
        Err(error) if error.kind() != ErrorKind::AlreadyExists => Err(format!(
            "Couldn't write {} ({})",
            dir.path().join(IGNORE_FILE).display(),
            reason_of(&error)
        )),
        _ => Ok(()),
    }
}

/// Deletes the `.gitignore` in `dir` while its text is Arko's.
fn remove_ignore(dir: &Folder) -> Result<(), String> {
    if is_own_ignore(dir) {
        match dir.remove_file(IGNORE_FILE) {
            Err(error) if error.kind() != ErrorKind::NotFound => {
                return Err(format!(
                    "Couldn't delete {} ({})",
                    dir.path().join(IGNORE_FILE).display(),
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

//! The input history: what the user sent before, oldest first, shared by
//! every front end (the line prompt, the full-screen view, an app) in
//! Arko's `history.txt`, in the line editor's format.

use std::path::{Path, PathBuf};

/// The history kept, as the line editor keeps it.
const HISTORY_SIZE: usize = 1000;

/// The history file in Arko's folder `root`.
pub fn path_in(root: &Path) -> PathBuf {
    root.join("history.txt")
}

/// The messages sent before, oldest first, in the line editor's file.
pub struct History {
    path: Option<PathBuf>,
    pub(crate) entries: Vec<String>,
}

impl History {
    /// The history in `path` (none yet is an empty one).
    pub fn load(path: Option<PathBuf>) -> Self {
        let entries = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| parse(&text))
            .unwrap_or_default();
        Self { path, entries }
    }

    /// The messages, oldest first.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Adds `entry` (not a repeat of the last), and saves the history: with
    /// what other front ends added since it was read, the latest 1,000.
    pub fn add(&mut self, entry: &str) {
        if entry.trim().is_empty() || self.entries.last().is_some_and(|last| last == entry) {
            return;
        }
        let Some(path) = self.path.clone() else {
            self.entries.push(entry.to_string());
            self.keep_latest();
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // One front end at a time reads and writes the file.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("txt.lock"));
        let _held = lock.as_ref().ok().filter(|lock| lock.lock().is_ok());
        if let Ok(text) = std::fs::read_to_string(&path) {
            self.entries = parse(&text);
        }
        if self.entries.last().is_none_or(|last| last != entry) {
            self.entries.push(entry.to_string());
        }
        self.keep_latest();
        if let Err(error) = save(&path, &self.entries) {
            log::warn!("could not save the input history: {error}");
        }
    }

    fn keep_latest(&mut self) {
        let extra = self.entries.len().saturating_sub(HISTORY_SIZE);
        self.entries.drain(..extra);
    }
}

/// Saves `entries` at `path` as the line editor does (rustyline's version
/// 2 format: line breaks and backslashes escaped), whole: a new file, with
/// the old one's permissions (else readable by the user only), renamed
/// into its place, so a crash leaves the old history or the new.
fn save(path: &Path, entries: &[String]) -> std::io::Result<()> {
    let mut text = String::from("#V2\n");
    for entry in entries {
        text.push_str(&entry.replace('\\', "\\\\").replace('\n', "\\n"));
        text.push('\n');
    }
    let temporary = path.with_file_name(format!(".history.txt.{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = std::fs::metadata(path).map_or(0o600, |meta| meta.permissions().mode() & 0o777);
        options.mode(mode);
    }
    let written = options.open(&temporary).and_then(|mut file| {
        use std::io::Write;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        // (The mode asked for is masked by the umask: set it again.)
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
        Ok(())
    });
    let written = written.and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// The entries of a history file: version 2 (escaped) or the older plain
/// one.
fn parse(text: &str) -> Vec<String> {
    let mut lines = text.lines();
    let v2 = text.starts_with("#V2");
    if v2 {
        lines.next();
    }
    lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            if !v2 {
                return line.to_string();
            }
            let mut entry = String::new();
            let mut chars = line.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    match chars.next() {
                        Some('n') => entry.push('\n'),
                        Some(other) => entry.push(other),
                        None => {}
                    }
                } else {
                    entry.push(c);
                }
            }
            entry
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What is sent goes in Arko's history file, as the line editor keeps
    /// it (a repeat of the last once, lines escaped), and any front end
    /// reads it back.
    #[test]
    fn the_history_is_shared_in_the_line_editors_format() {
        let temp = tempfile::tempdir().unwrap();
        let path = path_in(temp.path());
        let mut history = History::load(Some(path.clone()));
        assert!(history.entries().is_empty());
        history.add("Count the pears.");
        history.add("Count the pears.");
        history.add("Two lines:\nthe second, with a \\ backslash.");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("#V2\n"), "{text}");
        assert_eq!(
            History::load(Some(path)).entries(),
            [
                "Count the pears.",
                "Two lines:\nthe second, with a \\ backslash."
            ]
        );
    }
    /// Two front ends that keep the history (the terminal, an app) each
    /// keep the other's entries when they add theirs.
    #[test]
    fn front_ends_keep_each_others_entries() {
        let temp = tempfile::tempdir().unwrap();
        let path = path_in(temp.path());
        let mut terminal = History::load(Some(path.clone()));
        let mut app = History::load(Some(path.clone()));
        terminal.add("one");
        app.add("two");
        terminal.add("three");
        assert_eq!(terminal.entries(), ["one", "two", "three"]);
        assert_eq!(History::load(Some(path)).entries(), ["one", "two", "three"]);
    }

    /// The history file is replaced whole (a new file renamed into place,
    /// none left behind), with the permissions it had.
    #[cfg(unix)]
    #[test]
    fn the_history_is_replaced_whole_with_its_permissions() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let path = path_in(temp.path());
        let mut history = History::load(Some(path.clone()));
        history.add("first");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "the user's only, at first");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let before = std::fs::metadata(&path).unwrap().ino();
        history.add("second");
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino(),
            before,
            "a new file"
        );
        assert_eq!(mode(&path), 0o640);
        let leftovers: Vec<String> = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert_eq!(History::load(Some(path)).entries(), ["first", "second"]);
    }
}

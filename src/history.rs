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

    /// Adds `entry` (not a repeat of the last), and saves the history.
    pub fn add(&mut self, entry: &str) {
        if entry.trim().is_empty() || self.entries.last().is_some_and(|last| last == entry) {
            return;
        }
        self.entries.push(entry.to_string());
        let extra = self.entries.len().saturating_sub(HISTORY_SIZE);
        self.entries.drain(..extra);
        self.save();
    }

    /// Saves it as the line editor does (rustyline's version 2 format:
    /// line breaks and backslashes escaped), readable by the user only.
    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut text = String::from("#V2\n");
        for entry in &self.entries {
            text.push_str(&entry.replace('\\', "\\\\").replace('\n', "\\n"));
            text.push('\n');
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(path) {
            use std::io::Write;
            let _ = file.write_all(text.as_bytes());
        }
    }
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
}

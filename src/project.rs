//! The project's instructions: `AGENTS.md` and `CLAUDE.md` in the working
//! directory, read as Arko starts. Which of them the agents get is a session
//! setting ([`Files`]), and so is whether workers get them too.
//!
//! The main agent gets them in the first message of each conversation, and
//! again when a handover restates that message. Each worker gets the same
//! text, from the lead's working directory (a worktree of its own has only
//! what was committed), in its first message as it starts: the crew gives
//! it as arko's subagent start context, which a handover restates too.
//! Neither is in an agent's instructions, so `/project-instructions` can
//! change them at any time without breaking a prompt cache: the change
//! reaches the conversations and the workers that start after it.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Conversation;
use crate::remote::{Level, Source};

/// The files, in the order the agents get them.
const NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// The most text the agents get from the files together, in bytes. One file
/// may take all of it. Of two, each may take half, and more when the other
/// leaves room.
const MAX_BYTES: usize = 64 * 1024;

/// Where the main agent is told the files are.
const MAIN_PLACE: &str = "the working directory";

/// Where a worker is told they are: it may work in a worktree of its own.
const WORKER_PLACE: &str = "your lead's working directory";

/// Which of the project's instruction files the agents get. `One` is the
/// default, and what sessions saved before the choice gave. (The variants'
/// comments are `--project-instructions`'s help.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "tui", derive(clap::ValueEnum))]
#[serde(rename_all = "lowercase")]
pub enum Files {
    /// AGENTS.md, or CLAUDE.md when there is no AGENTS.md.
    #[default]
    One,
    /// AGENTS.md and CLAUDE.md, each under its own name. A CLAUDE.md with
    /// the same text as AGENTS.md goes once.
    Both,
    /// Neither file.
    Off,
}

impl Files {
    pub const ALL: [Files; 3] = [Files::One, Files::Both, Files::Off];

    /// The name `/project-instructions` and `--project-instructions` take.
    pub fn name(self) -> &'static str {
        match self {
            Files::One => "one",
            Files::Both => "both",
            Files::Off => "off",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|files| files.name().eq_ignore_ascii_case(text.trim()))
    }

    /// The files the agents get, as Arko tells the user.
    pub fn describe(self) -> &'static str {
        match self {
            Files::One => "AGENTS.md, or CLAUDE.md when there is none",
            Files::Both => "AGENTS.md and CLAUDE.md, each under its own name",
            Files::Off => "neither AGENTS.md nor CLAUDE.md",
        }
    }

    pub fn is_one(&self) -> bool {
        *self == Files::One
    }
}

/// The instruction files the working directory had as Arko started: each
/// by name, with its text trimmed. Missing and empty files are left out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found(Vec<(&'static str, String)>);

impl Found {
    pub fn read(workspace: &Path) -> Self {
        Self(
            NAMES
                .into_iter()
                .filter_map(|name| {
                    let text = std::fs::read_to_string(workspace.join(name)).ok()?;
                    let text = text.trim();
                    (!text.is_empty()).then(|| (name, text.to_string()))
                })
                .collect(),
        )
    }

    /// The files `choice` gives the agents, each by name with its text, cut
    /// so that together they take at most [`MAX_BYTES`].
    pub fn given(&self, choice: Files) -> Vec<(&'static str, String)> {
        let mut files: Vec<&(&'static str, String)> = Vec::new();
        match choice {
            Files::One => files.extend(self.0.first()),
            Files::Both => {
                for file in &self.0 {
                    if files.iter().all(|given| given.1 != file.1) {
                        files.push(file);
                    }
                }
            }
            Files::Off => {}
        }
        let total: usize = files.iter().map(|(_, text)| text.len()).sum();
        files
            .iter()
            .map(|(name, text)| {
                let others = total - text.len();
                let room = MAX_BYTES
                    .saturating_sub(others)
                    .max(MAX_BYTES / files.len());
                (*name, cut(name, text, room))
            })
            .collect()
    }

    /// The names of the files `choice` gives.
    pub fn names(&self, choice: Files) -> Vec<&'static str> {
        self.given(choice)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// What the main agent's first message of a conversation starts with:
    /// the files `choice` gives, each under its heading. `None` without any.
    pub fn for_main(&self, choice: Files) -> Option<String> {
        self.section(choice, MAIN_PLACE)
    }

    /// What a worker's first message starts with (see [`Found::for_main`]).
    pub fn for_worker(&self, choice: Files) -> Option<String> {
        self.section(choice, WORKER_PLACE)
    }

    fn section(&self, choice: Files, place: &str) -> Option<String> {
        let parts: Vec<String> = self
            .given(choice)
            .into_iter()
            .map(|(name, text)| format!("# Project instructions ({name} in {place})\n\n{text}"))
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }
}

/// `text`, cut to `room` bytes (at a character's boundary) with a line that
/// says the rest of `name` is left out.
fn cut(name: &str, text: &str, room: usize) -> String {
    if text.len() <= room {
        return text.to_string();
    }
    let mut end = room;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[... the rest of {name} is left out ...]", &text[..end])
}

/// `names` as a list in words: `AGENTS.md and CLAUDE.md`.
fn listed(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// How `/project-instructions` is used.
const USAGE: &str = "/project-instructions takes one, both or off, or workers on or off (/project-instructions both).";

impl crate::Harness {
    /// `/project-instructions`, typed at the prompt, sent from the page or
    /// chosen in the settings: says which of the project's instruction files
    /// the agents get. With `one`, `both` or `off` it changes which, and
    /// with `workers on` or `workers off` whether workers get them, at any
    /// time: the conversations and workers that start after it get the
    /// change.
    pub(crate) fn project_command(&mut self, argument: &str, source: Source) {
        let before = (
            self.settings.project_instructions,
            self.settings.worker_project_instructions,
        );
        let words: Vec<&str> = argument.split_whitespace().collect();
        let result = match words.as_slice() {
            [] | ["workers"] => Ok(self.project_line()),
            ["workers", "on"] => self.set_project_workers(true),
            ["workers", "off"] => self.set_project_workers(false),
            [word] => match Files::parse(word) {
                Some(files) => Ok(self.set_project_files(files)),
                None => Err(USAGE.to_string()),
            },
            _ => Err(USAGE.to_string()),
        };
        let changed = before
            != (
                self.settings.project_instructions,
                self.settings.worker_project_instructions,
            );
        self.setting_reply(result, source, changed);
    }

    /// The project's instructions new workers get in their first message,
    /// as the session's settings say, to the crew.
    pub(crate) fn give_workers_project(&self) {
        if let Some(crew) = &self.crew {
            let settings = &self.settings;
            crew.set_project(
                settings
                    .worker_project_instructions
                    .then(|| self.project.for_worker(settings.project_instructions))
                    .flatten(),
            );
        }
    }

    /// Whether the session has workers (not `--workers 0`, nor the old
    /// worker tools).
    pub(crate) fn has_workers(&self) -> bool {
        self.settings.max_workers > 0 && self.crew.is_some()
    }

    /// The setting, what it gives here and who gets it, as
    /// `/project-instructions` says it.
    fn project_line(&self) -> String {
        let mut text = self.project_setting();
        text.push_str(" /project-instructions one, both or off changes which files");
        if self.has_workers() {
            text.push_str(", and /project-instructions workers on or off whether workers get them");
        }
        text.push('.');
        text
    }

    /// The setting, the files it gives from this working directory, and
    /// who gets them how often.
    fn project_setting(&self) -> String {
        let files = self.settings.project_instructions;
        let mut text = format!(
            "Project instructions are set to {}, so the agents get {}.",
            files.name(),
            files.describe()
        );
        let names = self.project.names(files);
        if files == Files::Off {
            return text;
        }
        if names.is_empty() {
            text.push_str(" The working directory has neither.");
            return text;
        }
        let it = if names.len() == 1 { "it" } else { "them" };
        text.push_str(&format!(
            " Here they get {}. The main agent gets {it} once per conversation, in its first message",
            listed(&names)
        ));
        if !self.has_workers() {
            text.push('.');
        } else if self.settings.worker_project_instructions {
            text.push_str(", and so does each worker.");
        } else {
            text.push_str(&format!(". Workers don't get {it}."));
        }
        text
    }

    /// Which files the agents get, from the next first message on: the
    /// conversation under way keeps what it got.
    fn set_project_files(&mut self, files: Files) -> String {
        if self.settings.project_instructions == files {
            return format!("The agents get {} already.", files.describe());
        }
        self.settings.project_instructions = files;
        self.project_changed();
        let mut text = self.project_setting();
        if !matches!(self.conversation, Conversation::New) {
            text.push_str(
                " The conversation under way keeps what it got, and the next one gets the change.",
            );
        }
        text
    }

    /// Whether workers started from now on get the project's instructions.
    fn set_project_workers(&mut self, on: bool) -> Result<String, String> {
        if !self.has_workers() {
            return Err("This session has no workers (it started with --workers 0, or with the old worker tools).".to_string());
        }
        let state = if on { "on" } else { "off" };
        if self.settings.worker_project_instructions == on {
            return Ok(format!(
                "Project instructions for workers are {state} already."
            ));
        }
        self.settings.worker_project_instructions = on;
        self.project_changed();
        Ok(if on {
            "Workers started from now on get the project instructions, once each, in their first message.".to_string()
        } else {
            "Workers started from now on don't get the project instructions.".to_string()
        })
    }

    /// The session keeps the change, new workers get it, and the clients
    /// show it.
    fn project_changed(&mut self) {
        if let Err(error) = self.session.save_settings(&self.settings) {
            self.say(
                Level::Error,
                &format!("Couldn't save the session's settings ({error})."),
            );
        }
        self.give_workers_project();
        self.publish_models();
    }

    /// The files the agents get from this working directory, in words (the
    /// banner's), or nothing.
    pub(crate) fn project_names(&self) -> String {
        listed(&self.project.names(self.settings.project_instructions))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A working directory with these files (a `None` text: no file).
    fn found(agents: Option<&str>, claude: Option<&str>) -> (tempfile::TempDir, Found) {
        let temp = tempfile::tempdir().unwrap();
        for (name, text) in [("AGENTS.md", agents), ("CLAUDE.md", claude)] {
            if let Some(text) = text {
                std::fs::write(temp.path().join(name), text).unwrap();
            }
        }
        let found = Found::read(temp.path());
        (temp, found)
    }

    fn given(found: &Found, choice: Files) -> Vec<(&'static str, String)> {
        found.given(choice)
    }

    fn pair(name: &'static str, text: &str) -> (&'static str, String) {
        (name, text.to_string())
    }

    /// One gives AGENTS.md, else CLAUDE.md; both gives each there is; off
    /// gives none. An empty (or blank) file counts as none.
    #[test]
    fn each_choice_gives_its_files() {
        let (_neither, neither) = found(None, None);
        let (_agents, agents) = found(Some("agent rules\n"), None);
        let (_claude, claude) = found(None, Some("claude rules"));
        let (_both, both) = found(Some("agent rules"), Some("  claude rules \n"));
        let (_empty, empty) = found(Some(" \n\n"), Some(""));
        let (_blank_agents, blank_agents) = found(Some("\n"), Some("claude rules"));
        for choice in Files::ALL {
            assert!(given(&neither, choice).is_empty(), "{choice:?}");
            assert!(given(&empty, choice).is_empty(), "{choice:?}");
        }
        let agent = pair("AGENTS.md", "agent rules");
        let claude_md = pair("CLAUDE.md", "claude rules");
        assert_eq!(given(&agents, Files::One), [agent.clone()]);
        assert_eq!(given(&agents, Files::Both), [agent.clone()]);
        assert_eq!(given(&claude, Files::One), [claude_md.clone()]);
        assert_eq!(given(&claude, Files::Both), [claude_md.clone()]);
        assert_eq!(given(&both, Files::One), [agent.clone()]);
        assert_eq!(
            given(&both, Files::Both),
            [agent.clone(), claude_md.clone()]
        );
        assert_eq!(given(&blank_agents, Files::One), [claude_md.clone()]);
        assert_eq!(given(&blank_agents, Files::Both), [claude_md]);
        for found in [&agents, &claude, &both, &blank_agents] {
            assert!(given(found, Files::Off).is_empty());
        }
    }

    /// A CLAUDE.md with AGENTS.md's text (a link to it, say) is given once.
    #[test]
    fn the_same_text_twice_is_given_once() {
        let (_temp, same) = found(Some("rules\n"), Some("rules"));
        assert_eq!(given(&same, Files::Both), [pair("AGENTS.md", "rules")]);
        assert_eq!(same.names(Files::Both), ["AGENTS.md"]);
    }

    /// One file may take all of [`MAX_BYTES`]. Of two, each may take half,
    /// and more when the other leaves room, so together they never pass
    /// it. A cut says so, and never splits a character.
    #[test]
    fn the_files_share_the_cap() {
        let cut_line = |name: &str| format!("\n[... the rest of {name} is left out ...]");
        let long = "a".repeat(MAX_BYTES + 10);
        let (_temp, alone) = found(Some(&long), None);
        let text = &given(&alone, Files::One)[0].1;
        assert_eq!(
            *text,
            format!("{}{}", &long[..MAX_BYTES], cut_line("AGENTS.md"))
        );
        let exactly = "b".repeat(MAX_BYTES);
        let (_temp, full) = found(None, Some(&exactly));
        assert_eq!(given(&full, Files::Both), [pair("CLAUDE.md", &exactly)]);

        // A short AGENTS.md leaves the rest to CLAUDE.md.
        let short = "s".repeat(1000);
        let (_temp, uneven) = found(Some(&short), Some(&long));
        let files = given(&uneven, Files::Both);
        assert_eq!(files[0], pair("AGENTS.md", &short));
        assert_eq!(
            files[1].1,
            format!("{}{}", &long[..MAX_BYTES - 1000], cut_line("CLAUDE.md"))
        );
        // Under One, the long file isn't given, and the short one is whole.
        assert_eq!(given(&uneven, Files::One), [pair("AGENTS.md", &short)]);

        // Two long files take half each.
        let other = "c".repeat(MAX_BYTES);
        let (_temp, long_both) = found(Some(&long), Some(&other));
        let files = given(&long_both, Files::Both);
        assert_eq!(
            files[0].1,
            format!("{}{}", &long[..MAX_BYTES / 2], cut_line("AGENTS.md"))
        );
        assert_eq!(
            files[1].1,
            format!("{}{}", &other[..MAX_BYTES / 2], cut_line("CLAUDE.md"))
        );
        // Two that fit together are whole.
        let half = "h".repeat(MAX_BYTES / 2);
        let (_temp, fits) = found(Some(&half), Some(&"g".repeat(MAX_BYTES / 2)));
        let files = given(&fits, Files::Both);
        assert_eq!(files[0].1.len() + files[1].1.len(), MAX_BYTES);

        // A cut that would split a character stops before it.
        let wide = format!("x{}", "é".repeat(MAX_BYTES));
        let (_temp, accents) = found(Some(&wide), None);
        let text = &given(&accents, Files::One)[0].1;
        assert_eq!(
            *text,
            format!("{}{}", &wide[..MAX_BYTES - 1], cut_line("AGENTS.md"))
        );
    }

    /// The main agent and a worker get the same files, each under a heading
    /// with its name and where it is.
    #[test]
    fn each_file_goes_under_its_own_heading() {
        let (_temp, both) = found(Some("agent rules"), Some("claude rules"));
        assert_eq!(
            both.for_main(Files::Both).unwrap(),
            "# Project instructions (AGENTS.md in the working directory)\n\nagent rules\n\n# Project instructions (CLAUDE.md in the working directory)\n\nclaude rules"
        );
        assert_eq!(
            both.for_worker(Files::One).unwrap(),
            "# Project instructions (AGENTS.md in your lead's working directory)\n\nagent rules"
        );
        assert!(both.for_main(Files::Off).is_none());
        assert!(Found::default().for_worker(Files::Both).is_none());
    }

    /// The names the command and the flag take.
    #[test]
    fn the_choices_parse_by_name() {
        for files in Files::ALL {
            assert_eq!(Files::parse(files.name()), Some(files));
        }
        assert_eq!(Files::parse(" Both "), Some(Files::Both));
        assert_eq!(Files::parse("agents"), None);
        assert_eq!(Files::default(), Files::One);
    }
}

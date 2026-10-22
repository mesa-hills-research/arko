//! The session's conversations with the main agent: `/new` starts one, and
//! `/resume` goes back to an earlier one.
//!
//! A conversation is the main agent's runs from one `/new` (or the
//! session's start) to the next. A handover goes on in the same run, with
//! a fresh context. A turn that fails leaves its run unable to go on, so
//! the next message starts another run of the same conversation. The next
//! message in a conversation continues its latest run.
//!
//! The record of them is kept in the session's folder (`conversations.json`),
//! so a resumed session lists them too, and goes on in the one it was in.
//! Sessions saved before it have none: theirs is worked out from the main
//! agent's runs (see [`Conversations::derive`]).
//!
//! What belongs to a conversation goes with it: the lines the user typed
//! during its last turn that the agent didn't read wait in its record
//! while another one is current, and a handover asked for its next turn
//! stays asked (it is asked of its run). The workers belong to the
//! session: their news goes to the conversation current when it comes.

use std::path::Path;

use arko_workflows::prelude::*;
use chrono::{DateTime, Datelike, Local, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agents::MAIN;
use crate::remote::{self, ConversationEntry, Level};
use crate::{Conversation, Harness, conversation_of, lock};

/// The record's file, in the session's folder.
const FILE: &str = "conversations.json";

/// The longest title kept, in characters.
const TITLE_CHARS: usize = 120;

/// The session's conversations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Conversations {
    /// In the order they started: the first is conversation 1.
    pub list: Vec<Record>,
    /// The one the next message goes to, by its place in `list`. `None`
    /// when the next message starts a new one.
    pub current: Option<usize>,
}

/// A conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The main agent's runs, in the order they started.
    pub runs: Vec<Uuid>,
    /// The first line of the user's first message.
    pub title: String,
    /// While another conversation is current: what the user typed during
    /// this one's last turn that the agent didn't read. It goes with the
    /// next message in it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread: Vec<String>,
}

/// What the record needs of one of the main agent's runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub id: Uuid,
    /// The first line of the user's words in its first message.
    pub title: String,
    /// It failed, so it can't go on.
    pub failed: bool,
    pub started: DateTime<Utc>,
}

/// The main agent's runs in `workflow`, in the order they started.
pub fn main_runs(workflow: &Workflow) -> Vec<Run> {
    main_runs_of(&workflow.context().agent_results.read())
}

/// The main agent's runs in the agent runs `results`, in the order they
/// started.
pub fn main_runs_of(results: &[AgentResult]) -> Vec<Run> {
    let mut runs: Vec<Run> = results
        .iter()
        .filter(|result| result.agent_name == MAIN)
        .map(|result| Run {
            id: result.agent_run_id,
            title: title_of(&result.task.initial_task_prompt),
            failed: result.status == AgentRunStatus::Failed,
            started: result.task.start_time,
        })
        .collect();
    runs.sort_by_key(|run| run.started);
    runs
}

/// When the main agent's run `run` was last used: when its last turn
/// ended, else when it started.
fn last_used(workflow: &Workflow, run: Uuid) -> Option<DateTime<Utc>> {
    workflow
        .context()
        .agent_results
        .read()
        .iter()
        .find(|result| result.agent_run_id == run)
        .map(|result| result.task.end_time.unwrap_or(result.task.start_time))
}

impl Conversations {
    /// The record in the session's folder `dir`. `None` when there is none
    /// (a session saved before it, or one that hasn't started a
    /// conversation) or it can't be read.
    pub fn load(dir: &Path) -> Option<Self> {
        let path = dir.join(FILE);
        let text = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<Self>(&text) {
            Ok(mut conversations) => {
                if conversations
                    .current
                    .is_some_and(|current| current >= conversations.list.len())
                {
                    conversations.current = None;
                }
                Some(conversations)
            }
            Err(error) => {
                log::warn!("could not read {}: {error}", path.display());
                None
            }
        }
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        let temporary = dir.join(format!("{FILE}.tmp"));
        std::fs::write(&temporary, text)?;
        std::fs::rename(temporary, dir.join(FILE))
    }

    /// The record of a session saved without one, from its main agent's
    /// `runs`: each run started a conversation (with `/new`), except a run
    /// after one that failed, which went on with its conversation. The
    /// latest is current, as such a session went on with its latest run.
    pub fn derive(runs: &[Run]) -> Self {
        let mut list: Vec<Record> = Vec::new();
        let mut after_failure = false;
        for run in runs {
            match list.last_mut() {
                Some(last) if after_failure => last.runs.push(run.id),
                _ => list.push(Record {
                    runs: vec![run.id],
                    title: run.title.clone(),
                    unread: Vec::new(),
                }),
            }
            after_failure = run.failed;
        }
        let current = list.len().checked_sub(1);
        Self { list, current }
    }

    /// Takes in the runs of `runs` the record doesn't have: each goes on
    /// with the current conversation, or starts one when the next message
    /// was to start one. Returns whether the record changed.
    pub fn take_in(&mut self, runs: &[Run]) -> bool {
        let mut changed = false;
        for run in runs {
            if self.list.iter().any(|record| record.runs.contains(&run.id)) {
                continue;
            }
            match self.current {
                Some(current) => self.list[current].runs.push(run.id),
                None => {
                    self.list.push(Record {
                        runs: vec![run.id],
                        title: run.title.clone(),
                        unread: Vec::new(),
                    });
                    self.current = Some(self.list.len() - 1);
                }
            }
            changed = true;
        }
        changed
    }

    /// The current conversation's latest run: the one the next message
    /// continues. `None` when the next message starts a conversation.
    pub fn current_run(&self) -> Option<Uuid> {
        self.list.get(self.current?)?.runs.last().copied()
    }

    pub fn current_mut(&mut self) -> Option<&mut Record> {
        self.list.get_mut(self.current?)
    }

    /// Whether there is a conversation to go back to: one that isn't
    /// current.
    pub fn has_earlier(&self) -> bool {
        (0..self.list.len()).any(|index| Some(index) != self.current)
    }

    /// The conversations as `/resume` lists them, newest first. `last_used`
    /// says when a run was last used.
    pub fn entries(
        &self,
        last_used: impl Fn(Uuid) -> Option<DateTime<Utc>>,
    ) -> Vec<ConversationEntry> {
        self.list
            .iter()
            .enumerate()
            .rev()
            .map(|(index, record)| ConversationEntry {
                number: index + 1,
                title: record.title.clone(),
                last_used: record.runs.last().and_then(|run| last_used(*run)),
                current: self.current == Some(index),
            })
            .collect()
    }
}

/// The title of a conversation whose first message was `prompt`: the first
/// line of the user's words in it (Arko's message keeps them under "# My
/// message" when it adds sections of its own, see `Harness::compose`).
pub fn title_of(prompt: &str) -> String {
    let words = prompt
        .split_once("# My message\n\n")
        .map_or(prompt, |(_, words)| words);
    let line = words
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    crate::ui::cut(line, TITLE_CHARS)
}

/// When `at` was, as the user reads it at `now`: "just now", "5 minutes
/// ago", "2 hours ago" (the same day), "yesterday 14:05", "Oct 10", or
/// "Oct 10 2025" in another year.
pub fn when(at: DateTime<Utc>, now: DateTime<Local>) -> String {
    let local = at.with_timezone(&Local);
    let minutes = (now - local).num_minutes().max(0);
    let plural =
        |count: i64, unit: &str| format!("{count} {unit}{} ago", if count == 1 { "" } else { "s" });
    if minutes < 1 {
        "just now".to_string()
    } else if minutes < 60 {
        plural(minutes, "minute")
    } else if local.date_naive() == now.date_naive() {
        plural(minutes / 60, "hour")
    } else if now.date_naive().pred_opt() == Some(local.date_naive()) {
        local.format("yesterday %H:%M").to_string()
    } else if local.year() == now.year() {
        local.format("%b %-d").to_string()
    } else {
        local.format("%b %-d %Y").to_string()
    }
}

/// The widest [`when`] text: "yesterday 14:05".
#[cfg(feature = "tui")]
pub const WHEN_WIDTH: usize = 15;

/// How long the titles in `/resume`'s list at the line prompt (and on the
/// page) may be, in characters: a row fits in 80 columns.
const LIST_TITLE: usize = 46;

/// `/resume`'s list at the line prompt and on the page: one row each,
/// newest first, then how to go back to one. A row's time follows its
/// title, without columns, so it reads well where it wraps (on a phone).
fn list_lines(entries: &[ConversationEntry], now: DateTime<Local>) -> Vec<String> {
    let mut lines = vec!["This session's conversations, newest first".to_string()];
    let digits = entries
        .iter()
        .map(|entry| entry.number.to_string().len())
        .max()
        .unwrap_or(1);
    for entry in entries {
        let title = if entry.title.chars().count() > LIST_TITLE {
            crate::ui::cut(&entry.title, LIST_TITLE - 1)
        } else {
            entry.title.clone()
        };
        let mut about: Vec<String> = entry
            .last_used
            .map(|at| when(at, now))
            .into_iter()
            .collect();
        if entry.current {
            about.push("current".to_string());
        }
        let about = if about.is_empty() {
            String::new()
        } else {
            format!("  ({})", about.join(", "))
        };
        lines.push(format!("  {:>digits$}  {title}{about}", entry.number));
    }
    let example = entries
        .iter()
        .find(|entry| !entry.current)
        .map_or(1, |entry| entry.number);
    lines.push(format!(
        "Go back to one with /resume and its number, such as /resume {example}."
    ));
    lines
}

/// What follows `/resume` in `text`, when it is that command.
pub fn resume_argument(text: &str) -> Option<&str> {
    match text.strip_prefix("/resume") {
        Some("") => Some(""),
        Some(rest) if rest.starts_with(char::is_whitespace) => Some(rest.trim()),
        _ => None,
    }
}

impl Harness {
    /// The record as the session opens: a new session's is empty, and a
    /// resumed one's is read from its folder (or worked out from its runs)
    /// and takes in the runs it lacks (a turn Arko stopped in the middle of
    /// started one).
    pub(crate) fn open_conversations(&mut self, workflow: &Workflow, resume: bool) {
        self.conversations = if resume {
            let runs = main_runs(workflow);
            let mut conversations = Conversations::load(&self.session.dir)
                .unwrap_or_else(|| Conversations::derive(&runs));
            conversations.take_in(&runs);
            conversations
        } else {
            Conversations::default()
        };
        if resume {
            self.save_conversations();
        }
    }

    /// After a turn: the record takes in the run it started, if it started
    /// one, and the clients get the list again.
    pub(crate) fn track_conversations(&mut self) {
        let runs = main_runs(self.workflow());
        if self.conversations.take_in(&runs) {
            self.save_conversations();
        }
        self.publish_conversations();
    }

    pub(crate) fn save_conversations(&self) {
        if let Err(error) = self.conversations.save(&self.session.dir) {
            self.say(
                Level::Error,
                &format!("Couldn't save the session's conversations ({error})."),
            );
        }
    }

    /// Tells the clients (the full-screen view's `/resume` picker) the
    /// conversations.
    pub(crate) fn publish_conversations(&self) {
        self.gauges.touch();
        self.publish_title();
        let workflow = self.workflow().clone();
        *lock(&self.gauges.conversations) =
            self.conversations.entries(|run| last_used(&workflow, run));
    }

    /// The current conversation is left: what the user typed during its
    /// last turn that the agent didn't read waits in its record.
    fn leave_conversation(&mut self) {
        let unread = std::mem::take(&mut self.unread);
        if let Some(record) = self.conversations.current_mut() {
            record.unread = unread;
        }
    }

    /// `/new`: the next message starts a new conversation.
    pub(crate) fn new_conversation(&mut self) {
        self.leave_conversation();
        self.switched = true;
        self.conversations.current = None;
        self.conversation = Conversation::New;
        self.handover.switch(None, None);
        {
            // Nothing of the new conversation is cached yet.
            let mut stats = lock(&self.stats);
            stats.context_tokens = None;
            stats.set_cache(None);
        }
        if !self.conversations.list.is_empty() {
            self.save_conversations();
        }
        self.publish_conversations();
        self.feed.conversation("New conversation");
        let mut text = "The next message starts a new conversation.".to_string();
        if !self.conversations.list.is_empty() {
            text.push_str(" /resume goes back to the last one.");
        }
        self.output.line(&self.output.dim(&text));
    }

    /// `/resume` lists the conversations, and `/resume N` goes back to
    /// conversation N.
    pub(crate) async fn resume_command(&mut self, argument: &str) {
        if !self.conversations.has_earlier() {
            self.mirrored(|harness| {
                harness
                    .output
                    .line("There is no earlier conversation to go back to.")
            });
            return;
        }
        if argument.is_empty() {
            let workflow = self.workflow().clone();
            let entries = self.conversations.entries(|run| last_used(&workflow, run));
            self.mirrored(|harness| {
                let out = &harness.output;
                let lines = list_lines(&entries, Local::now());
                for (n, line) in lines.iter().enumerate() {
                    // The rows follow the heading: the current one stands out.
                    let current = n
                        .checked_sub(1)
                        .and_then(|row| entries.get(row))
                        .is_some_and(|entry| entry.current);
                    out.line(&if current {
                        out.bold(line)
                    } else {
                        line.clone()
                    });
                }
            });
            return;
        }
        let index = argument
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
            .filter(|index| *index < self.conversations.list.len());
        let Some(index) = index else {
            self.mirrored(|harness| {
                harness.output.line(&harness.output.error(&format!(
                    "There is no conversation {argument}. /resume lists them."
                )))
            });
            return;
        };
        if self.conversations.current == Some(index) {
            self.mirrored(|harness| {
                harness
                    .output
                    .line(&format!("You are in conversation {} already.", index + 1))
            });
            return;
        }
        self.switch_to(index).await;
    }

    /// Goes back to the conversation at `index` in the record: the next
    /// message continues its latest run. What waited for it comes back, and
    /// the workers' news goes to it from now on. Its history shows, for
    /// the clients, and its last reply at the line prompt.
    async fn switch_to(&mut self, index: usize) {
        self.leave_conversation();
        self.switched = true;
        let record = &mut self.conversations.list[index];
        let run = record.runs.last().copied();
        let unread = std::mem::take(&mut record.unread);
        self.conversations.current = Some(index);
        self.unread = unread;
        let workflow = self.workflow().clone();
        let result = run.and_then(|run| workflow.agent_result(run));
        if let Some(result) = &result {
            // The lines typed during other conversations' turns were for
            // them (and its own unread ones come back above).
            workflow.skip_one_time_notes(result.agent_run_id);
            if let Some(crew) = &self.crew {
                crew.lead_run(result.agent_run_id);
            }
        }
        let input = result
            .as_ref()
            .and_then(|result| result.task.current_conversation_context_size());
        self.handover.switch(run, input);
        lock(&self.stats).context_tokens = input;
        // Its prompt cache, as the session's record has it.
        self.restore_cache(input.is_some());
        self.conversation = conversation_of(&workflow, run);
        self.save_conversations();
        self.publish_conversations();
        // What changed in the workflow (the notes skipped, the workers'
        // lead) is saved now, for a session resumed before the next turn.
        if let Err(error) = workflow.checkpoint().await {
            log::warn!("could not save the session after /resume: {error}");
        }
        self.show_conversation(index, result.as_ref());
    }

    /// Shows where the conversation at `index` left off, after `/resume`.
    fn show_conversation(&self, index: usize, latest: Option<&AgentResult>) {
        let out = &self.output;
        let when = latest
            .map(|result| result.task.end_time.unwrap_or(result.task.start_time))
            .map(|at| format!(", last used {}", when(at, Local::now())))
            .unwrap_or_default();
        let title = self
            .conversations
            .list
            .get(index)
            .map(|record| record.title.clone())
            .filter(|title| !title.is_empty());
        self.feed.conversation(&match title {
            Some(title) => format!("Conversation {} · {title}", index + 1),
            None => format!("Conversation {}", index + 1),
        });
        self.say(
            Level::Info,
            &format!("Back to conversation {}{when}.", index + 1),
        );
        if let Some(latest) = latest {
            self.feed.restore(remote::history(
                &latest.task.conversation.messages,
                latest.task.agent_config.tool_config.ends_task_with_text(),
                &self.images,
            ));
            self.end_restored_jobs(latest.agent_run_id);
        }
        let next = match &self.conversation {
            Conversation::Answered(result) if result.task.conversation.messages.is_empty() => {
                "Your next message starts it."
            }
            Conversation::Answered(result) => {
                out.echo(&out.dim("Its last reply is below."));
                out.echo(result.summary.trim());
                "Your next message continues it."
            }
            Conversation::Stopped(_) => {
                "Its last turn was stopped. Your next message continues it."
            }
            Conversation::New => {
                "It can't go on, so your next message starts it over, without what was said before."
            }
        };
        self.say(Level::Info, next);
        if !self.unread.is_empty() {
            self.say(
                Level::Info,
                "  (what you typed during its last turn, which the agent didn't read, goes with your next message)",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn run(id: u128, title: &str, failed: bool) -> Run {
        Run {
            id: Uuid::from_u128(id),
            title: title.into(),
            failed,
            started: Utc.timestamp_opt(id as i64 * 60, 0).unwrap(),
        }
    }

    /// A session saved before the record: each run started a conversation,
    /// but a run after a failed one went on with its conversation. The
    /// latest is current.
    #[test]
    fn an_old_sessions_runs_make_its_conversations() {
        let runs = [
            run(1, "Fix the build", false),
            run(2, "Add a README", true),
            run(3, "Add a README again", false),
            run(4, "Explain the tests", false),
        ];
        let derived = Conversations::derive(&runs);
        let shown: Vec<(&str, usize)> = derived
            .list
            .iter()
            .map(|record| (record.title.as_str(), record.runs.len()))
            .collect();
        assert_eq!(
            shown,
            [
                ("Fix the build", 1),
                ("Add a README", 2),
                ("Explain the tests", 1)
            ]
        );
        assert_eq!(derived.current, Some(2));
        assert_eq!(derived.current_run(), Some(Uuid::from_u128(4)));
        assert!(Conversations::derive(&[]).current.is_none());
    }

    /// A new run goes on with the current conversation, or starts one after
    /// `/new`. A run the record has stays where it is.
    #[test]
    fn new_runs_join_the_current_conversation_or_start_one() {
        let mut conversations = Conversations::default();
        assert!(!conversations.has_earlier());
        assert!(conversations.take_in(&[run(1, "First", false)]));
        assert_eq!(conversations.current, Some(0));
        assert!(!conversations.has_earlier());
        // After a failure, the next run is the same conversation's.
        assert!(conversations.take_in(&[run(1, "First", true), run(2, "Again", false)]));
        assert_eq!(conversations.list.len(), 1);
        assert_eq!(conversations.current_run(), Some(Uuid::from_u128(2)));
        // `/new`.
        conversations.current = None;
        assert!(conversations.has_earlier());
        assert!(!conversations.take_in(&[run(1, "First", false), run(2, "Again", false)]));
        assert!(conversations.take_in(&[run(3, "Second", false)]));
        assert_eq!(conversations.current, Some(1));
        let entries = conversations
            .entries(|run| Some(Utc.timestamp_opt(run.as_u128() as i64 * 3600, 0).unwrap()));
        let shown: Vec<(usize, &str, bool)> = entries
            .iter()
            .map(|entry| (entry.number, entry.title.as_str(), entry.current))
            .collect();
        assert_eq!(shown, [(2, "Second", true), (1, "First", false)]);
    }

    /// The record is kept in the session's folder, with what waits for a
    /// conversation that isn't current. A current one past the list is
    /// dropped.
    #[test]
    fn the_record_is_kept_in_the_sessions_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Conversations::load(dir.path()).is_none());
        let mut conversations = Conversations::derive(&[run(1, "First", false)]);
        conversations.list[0].unread = vec!["also the docs".into()];
        conversations.current = None;
        conversations.save(dir.path()).unwrap();
        assert_eq!(Conversations::load(dir.path()).unwrap(), conversations);
        std::fs::write(
            dir.path().join(FILE),
            r#"{"list": [{"runs": [], "title": "x"}], "current": 3}"#,
        )
        .unwrap();
        assert_eq!(Conversations::load(dir.path()).unwrap().current, None);
        std::fs::write(dir.path().join(FILE), "not json").unwrap();
        assert!(Conversations::load(dir.path()).is_none());
    }

    #[test]
    fn a_title_is_the_first_line_of_the_users_words() {
        assert_eq!(
            title_of("Fix the build\nIt fails on Windows."),
            "Fix the build"
        );
        assert_eq!(
            title_of(
                "# Project instructions (AGENTS.md in the working directory)\n\nBe brief.\n\n# My message\n\n\n  Add a README  \nwith a usage section"
            ),
            "Add a README"
        );
        assert_eq!(
            title_of(&"word ".repeat(100)).chars().count(),
            TITLE_CHARS + 1
        );
        assert_eq!(title_of(""), "");
    }

    #[test]
    fn times_read_as_people_say_them() {
        let now = Local.with_ymd_and_hms(2026, 10, 14, 15, 30, 0).unwrap();
        let at = |y, mo, d, h, mi| {
            Local
                .with_ymd_and_hms(y, mo, d, h, mi, 0)
                .unwrap()
                .with_timezone(&Utc)
        };
        assert_eq!(when(at(2026, 10, 14, 15, 30), now), "just now");
        assert_eq!(when(at(2026, 10, 14, 15, 29), now), "1 minute ago");
        assert_eq!(when(at(2026, 10, 14, 14, 35), now), "55 minutes ago");
        assert_eq!(when(at(2026, 10, 14, 13, 10), now), "2 hours ago");
        assert_eq!(when(at(2026, 10, 13, 22, 5), now), "yesterday 22:05");
        assert_eq!(when(at(2026, 10, 2, 9, 0), now), "Oct 2");
        assert_eq!(when(at(2025, 12, 30, 9, 0), now), "Dec 30 2025");
        // A clock set back doesn't make it later.
        assert_eq!(when(at(2026, 10, 14, 16, 0), now), "just now");
    }

    #[test]
    fn the_list_fits_80_columns_and_says_how_to_go_back() {
        let now = Local.with_ymd_and_hms(2026, 10, 14, 15, 30, 0).unwrap();
        let entries = [
            ConversationEntry {
                number: 2,
                title: "Make the login page remember the user between visits, with a cookie".into(),
                last_used: Some(now.with_timezone(&Utc)),
                current: true,
            },
            ConversationEntry {
                number: 1,
                title: "Fix the build".into(),
                last_used: Some((now - chrono::Duration::hours(26)).with_timezone(&Utc)),
                current: false,
            },
        ];
        let lines = list_lines(&entries, now);
        assert_eq!(
            lines,
            [
                "This session's conversations, newest first",
                "  2  Make the login page remember the user between…  (just now, current)",
                "  1  Fix the build  (yesterday 13:30)",
                "Go back to one with /resume and its number, such as /resume 1.",
            ]
        );
        assert!(lines.iter().all(|line| line.chars().count() <= 80));
        assert!(lines.iter().all(|line| !line.contains(';')));
    }

    #[test]
    fn resume_takes_a_number_or_nothing() {
        assert_eq!(resume_argument("/resume"), Some(""));
        assert_eq!(resume_argument("/resume 2"), Some("2"));
        assert_eq!(resume_argument("/resume  12 "), Some("12"));
        assert_eq!(resume_argument("/resumes"), None);
        assert_eq!(resume_argument("/new"), None);
    }
}

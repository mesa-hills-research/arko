//! Notes the main agent keeps during a session, in the session's folder.
//! They come back on their own where a summary or a restated first message
//! may have lost them.
//!
//! Each note says who it is for. The main agent gets every note it kept, in
//! the first message of a conversation and again after a handover or a Claude
//! Code compaction, as a one-time note that lists the current ones (a
//! handover restates the first message as it was, so a note kept since isn't
//! in it). A worker gets the notes for it in its first message, as they are
//! kept while it is active, and again after its own handover or compaction.
//! Nothing is sent on other turns. Once notes were kept, an agent whose
//! notes are all gone gets a list that says so, since its restated first
//! message may still hold them.
//!
//! The file is read for each call, so a front end that opens the same folder
//! sees the notes the session keeps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arko_agents::runtime::event::AgentEvent;
use arko_agents::{
    TaskContext, Tool, ToolDefinition, ToolError, ToolParameterDefinition, ToolParameterType,
    ToolTypeDetails,
};
use arko_workflows::notes::{Note as Posted, NoteSource};
use arko_workflows::prelude::*;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agents::MAIN;
use crate::crew::Crew;

pub(crate) const KEEP_NOTE: &str = "keep_note";
pub(crate) const FORGET_NOTE: &str = "forget_note";
/// The tools the main agent gets.
pub(crate) const TOOL_NAMES: [&str; 2] = [KEEP_NOTE, FORGET_NOTE];

/// The notes' file in the session's folder.
pub(crate) const FILE: &str = "kept-notes.json";

/// The most the notes take together in the main agent's list, in bytes. They
/// come back after each handover and compaction, so they stay small, and
/// well under the framework's limit for one note.
pub const MAX_BYTES: usize = 8 * 1024;
const _: () = assert!(MAX_BYTES < arko_workflows::notes::MAX_NOTE_BYTES);

/// What the worker is told above its notes.
const WORKER_HEADING: &str = "The main agent's notes for you. This list replaces any earlier one.";

/// What the worker is told above the removal of a note, which isn't a list.
const REMOVED_HEADING: &str = "The main agent's notes for you.";

/// The characters of a removed note's first line that its announcement quotes.
const QUOTED_CHARS: usize = 60;

/// What a list says when the notes it replaces are all gone.
const NONE_LEFT: &str = "No notes are kept now.";

/// One process writes the file at a time.
static WRITING: Mutex<()> = Mutex::new(());

/// Who a note is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    /// The main agent alone.
    Lead,
    /// These workers, by ID (`r1`).
    Workers(Vec<String>),
    /// Every worker, also those that start later.
    AllWorkers,
}

impl Audience {
    /// The audience a tool call names: `lead`, `workers` for all of them, or
    /// worker IDs such as `r1, r2`. Nothing means the lead.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let words: Vec<String> = text
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect();
        match words.as_slice() {
            [] => Ok(Self::Lead),
            [word] if word == "lead" => Ok(Self::Lead),
            [word] if word == "workers" => Ok(Self::AllWorkers),
            ids if ids.iter().all(|id| is_worker_id(id)) => Ok(Self::Workers(ids.to_vec())),
            _ => {
                Err("audience is lead, workers (every worker) or worker IDs such as r1, r2".into())
            }
        }
    }

    /// Whether worker `handle` is in the audience.
    pub fn includes(&self, handle: &str) -> bool {
        match self {
            Self::Lead => false,
            Self::AllWorkers => true,
            Self::Workers(ids) => ids.iter().any(|id| id.eq_ignore_ascii_case(handle)),
        }
    }

    /// Who the note is for, as `/notes` says it.
    pub fn describe(&self) -> String {
        match self {
            Self::Lead => "the main agent".to_string(),
            Self::AllWorkers => "every worker".to_string(),
            Self::Workers(ids) => ids.join(", "),
        }
    }
}

/// A worker's ID: `r` and a number.
fn is_worker_id(word: &str) -> bool {
    word.strip_prefix('r')
        .is_some_and(|number| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
}

/// A kept note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    /// Its number, which `forget_note` and `/notes remove` take. It isn't
    /// used again.
    pub id: u32,
    pub text: String,
    pub audience: Audience,
    pub created: DateTime<Utc>,
}

impl Note {
    /// The note as a list item. Its text is indented, so no line of it
    /// passes for a heading.
    fn item(&self, with_audience: bool) -> String {
        let for_whom = match (&self.audience, with_audience) {
            (_, false) => String::new(),
            (Audience::Lead, true) => ", for you".to_string(),
            (audience, true) => format!(", for {}", audience.describe()),
        };
        format!(
            "- Kept note {}{for_whom}, {}\n  {}",
            self.id,
            self.created.format("%Y-%m-%d %H:%M UTC"),
            self.text.replace('\n', "\n  ")
        )
    }
}

/// The main agent's list of `notes`, every one with who it is for. `None`
/// without notes.
fn lead_text(notes: &[Note]) -> Option<String> {
    (!notes.is_empty()).then(|| {
        let items: Vec<String> = notes.iter().map(|note| note.item(true)).collect();
        format!(
            "# Your kept notes\n\nYou kept these with keep_note, and forget_note removes one. This list replaces any earlier one.\n\n{}",
            items.join("\n")
        )
    })
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    /// The ID the next note takes.
    next: u32,
    notes: Vec<Note>,
}

/// A session's kept notes.
#[derive(Debug)]
pub struct KeptNotes {
    path: PathBuf,
    /// The task that follows the session's handovers and compactions.
    following: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl KeptNotes {
    /// The notes in `session_dir`, the session's folder.
    pub fn new(session_dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            path: session_dir.join(FILE),
            following: Mutex::new(None),
        })
    }

    /// The saved notes. A file that can't be read is an error, so it isn't
    /// replaced by the next note.
    fn read(&self) -> Result<Saved, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Saved::default());
            }
            Err(error) => return Err(self.unreadable(&error)),
        };
        serde_json::from_str(&text).map_err(|error| self.unreadable(&error))
    }

    fn unreadable(&self, error: &dyn std::fmt::Display) -> String {
        format!(
            "Couldn't read the kept notes in {} ({error}). Fix or delete the file.",
            self.path.display()
        )
    }

    /// The notes for delivery. A file that can't be read gives none.
    fn notes(&self) -> Vec<Note> {
        self.list().unwrap_or_else(|error| {
            log::warn!("{error}");
            Vec::new()
        })
    }

    /// Whether any note was ever kept, so that agents may hold a list that
    /// is gone now.
    fn ever_kept(&self) -> bool {
        self.read().is_ok_and(|saved| saved.next > 0)
    }

    fn write(&self, saved: &Saved) -> Result<(), String> {
        let text = serde_json::to_string_pretty(saved).map_err(|error| error.to_string())?;
        crate::session::write_whole(&self.path, text.as_bytes())
            .map_err(|error| format!("Couldn't save the note ({error})."))
    }

    /// The notes, oldest first. An error when the file can't be read.
    pub fn list(&self) -> Result<Vec<Note>, String> {
        Ok(self.read()?.notes)
    }

    /// Keeps a note. Refused when it is empty or the main agent's list would
    /// pass [`MAX_BYTES`].
    pub fn add(&self, text: &str, audience: Audience) -> Result<Note, String> {
        // As the framework reads a note: every kind of line break is one, so
        // every line is indented when the note is shown.
        let text = text
            .replace("\r\n", "\n")
            .replace(['\r', '\u{85}', '\u{2028}', '\u{2029}'], "\n");
        let text = text.trim();
        if text.is_empty() {
            return Err("The note is empty.".into());
        }
        let _writing = crate::lock(&WRITING);
        let mut saved = self.read()?;
        let id = saved
            .notes
            .iter()
            .map(|note| note.id)
            .fold(saved.next, u32::max)
            + 1;
        let note = Note {
            id,
            text: text.to_string(),
            audience,
            created: Utc::now(),
        };
        let held = lead_text(&saved.notes).map_or(0, |list| list.len());
        saved.next = id;
        saved.notes.push(note.clone());
        let after = lead_text(&saved.notes).map_or(0, |list| list.len());
        if after > MAX_BYTES {
            return Err(format!(
                "Kept notes hold {} KB at most. They take {held} bytes now and this note would add {}. Shorten it, or forget a note first.",
                MAX_BYTES / 1024,
                after - held
            ));
        }
        self.write(&saved)?;
        Ok(note)
    }

    /// Removes a note, returning it. The agents of a running session aren't
    /// told, so that session removes a note with `/notes remove N`.
    pub fn remove(&self, id: u32) -> Result<Note, String> {
        let _writing = crate::lock(&WRITING);
        let mut saved = self.read()?;
        let Some(index) = saved.notes.iter().position(|note| note.id == id) else {
            return Err(format!("There is no kept note {id}."));
        };
        let note = saved.notes.remove(index);
        self.write(&saved)?;
        Ok(note)
    }

    /// What the main agent gets: every note, with who it is for. `None`
    /// without notes.
    pub(crate) fn for_lead(&self) -> Option<String> {
        lead_text(&self.notes())
    }

    /// The notes for worker `handle`, as a list. `None` without any.
    fn worker_list(&self, handle: &str) -> Option<String> {
        let items: Vec<String> = self
            .notes()
            .iter()
            .filter(|note| note.audience.includes(handle))
            .map(|note| note.item(false))
            .collect();
        (!items.is_empty()).then(|| items.join("\n"))
    }

    /// What worker `handle`'s first message starts with. `None` without
    /// notes for it.
    pub(crate) fn for_worker(&self, handle: &str) -> Option<String> {
        self.worker_list(handle)
            .map(|list| format!("# Notes from the main agent\n\n{list}"))
    }

    /// Sends the notes to each active worker the audience includes. A worker
    /// reads them at its next step.
    pub(crate) fn tell_workers(&self, workflow: &Workflow, audience: &Audience) -> Vec<String> {
        let mut told = Vec::new();
        for record in workflow.subagents() {
            if record.state.is_active() && audience.includes(&record.handle) {
                told.extend(self.tell_worker(workflow, &record.handle));
            }
        }
        told
    }

    /// Sends worker `handle` its notes, or that none are left when notes
    /// were kept and its are gone, which a restated first message may still
    /// hold.
    fn tell_worker(&self, workflow: &Workflow, handle: &str) -> Option<String> {
        let list = self
            .worker_list(handle)
            .or_else(|| self.ever_kept().then(|| NONE_LEFT.to_string()))?;
        // No copy of it goes to the worker's lead: the main agent wrote it.
        match workflow.message_subagent(handle, &list, WORKER_HEADING, "") {
            Ok(_) => Some(handle.to_string()),
            Err(error) => {
                log::warn!("could not send {handle} its kept notes ({error})");
                None
            }
        }
    }

    /// Tells the agents that `note` is removed, by the user or (`by_user`
    /// unset) by the main agent. The main agent hears of it in a one-time
    /// note, unless it did it, and each active worker that had the note in a
    /// message. They keep what they read, and a handover or a compaction
    /// gives them the current list.
    pub(crate) fn tell_removed(&self, workflow: &Workflow, note: &Note, by_user: bool) {
        let by = if by_user {
            "The user"
        } else {
            "The main agent"
        };
        let first = note.text.lines().next().unwrap_or_default();
        let line = format!(
            "{by} removed kept note {} (\"{}\").",
            note.id,
            crate::ui::cut(first, QUOTED_CHARS)
        );
        if by_user
            && let Err(error) = workflow.post_note(
                Posted::new(&line)
                    .source(NoteSource::Host)
                    .to([MAIN])
                    .once(),
            )
        {
            log::warn!("could not tell the main agent a kept note was removed ({error})");
        }
        for record in workflow.subagents() {
            if record.state.is_active()
                && note.audience.includes(&record.handle)
                && let Err(error) =
                    workflow.message_subagent(&record.handle, &line, REMOVED_HEADING, "")
            {
                log::warn!(
                    "could not tell {} a kept note was removed ({error})",
                    record.handle
                );
            }
        }
    }

    /// Follows `workflow` (in place of the one followed before): after an
    /// agent's handover or Claude Code's compaction of its conversation, the
    /// agent gets its notes again.
    pub(crate) fn follow(self: &Arc<Self>, workflow: &Arc<Workflow>) {
        let mut events = workflow.subscribe_events_lossless();
        let (notes, workflow) = (Arc::downgrade(self), Arc::downgrade(workflow));
        let task = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if !matches!(
                    event.event,
                    AgentEvent::ConversationHandedOver { .. }
                        | AgentEvent::ConversationCompacted { .. }
                ) {
                    continue;
                }
                let (Some(notes), Some(workflow)) = (notes.upgrade(), workflow.upgrade()) else {
                    break;
                };
                notes.again(&workflow, &event.agent_name, event.agent_run_id);
            }
        });
        if let Some(old) = crate::lock(&self.following).replace(task) {
            old.abort();
        }
    }

    /// Gives run `run` of `agent` its notes again, in its fresh conversation.
    fn again(&self, workflow: &Workflow, agent: &str, run: uuid::Uuid) {
        if agent == MAIN {
            let none =
                format!("# Your kept notes\n\n{NONE_LEFT} This list replaces any earlier one.");
            if let Some(text) = self.for_lead().or_else(|| self.ever_kept().then_some(none))
                && let Err(error) =
                    workflow.post_note(Posted::new(text).source(NoteSource::Host).to([MAIN]).once())
            {
                log::warn!("could not give the main agent its kept notes again ({error})");
            }
        } else if let Some(record) = workflow.subagents().iter().find(|r| r.run_id == run) {
            self.tell_worker(workflow, &record.handle);
        }
    }
}

/// Copies the kept notes in the session folder `from` to the session folder
/// `to`, for a branch of the session, whose conversation holds the calls that
/// kept them.
pub(crate) fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::copy(from.join(FILE), to.join(FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        copied => copied.map(|_| ()),
    }
}

/// How `/notes` is used.
const USAGE: &str = "/notes lists the kept notes, and /notes remove N removes one.";

impl crate::Harness {
    /// `/notes`: lists the session's kept notes, and with `remove N` removes
    /// one.
    pub(crate) fn notes_command(&mut self, argument: &str) {
        let words: Vec<&str> = argument.split_whitespace().collect();
        let text = match words.as_slice() {
            [] => match self.kept.list() {
                Err(why) => why,
                Ok(notes) => match notes.first() {
                    None => {
                        "No notes are kept. The main agent keeps them with keep_note.".to_string()
                    }
                    Some(first) => {
                        let mut lines: Vec<String> = notes
                            .iter()
                            .map(|note| {
                                format!(
                                    "{}  for {}\n   {}",
                                    note.id,
                                    note.audience.describe(),
                                    note.text.replace('\n', "\n   ")
                                )
                            })
                            .collect();
                        lines.push(format!("/notes remove {} removes the first.", first.id));
                        lines.join("\n")
                    }
                },
            },
            ["remove", number] => match number.parse().map(|id| self.kept.remove(id)) {
                Ok(Ok(note)) => {
                    self.kept.tell_removed(self.workflow(), &note, true);
                    format!(
                        "Removed note {}. The agents that have it are told.",
                        note.id
                    )
                }
                Ok(Err(why)) => format!("{why} /notes lists them."),
                Err(_) => USAGE.to_string(),
            },
            _ => USAGE.to_string(),
        };
        self.mirrored(|harness| {
            for line in text.lines() {
                harness.output.line(line);
            }
        });
    }
}

/// What `keep_note` answers: the note's number, and the workers that have it
/// now.
fn kept_result(id: u32, told: &[String]) -> String {
    match told {
        [] => format!("Kept note {id}."),
        [one] => format!("Kept note {id}. {one} has it now."),
        several => format!("Kept note {id}. {} have it now.", crate::and_list(several)),
    }
}

/// `keep_note`, for the main agent.
pub(crate) struct KeepNote {
    pub(crate) notes: Arc<KeptNotes>,
    pub(crate) crew: Option<Arc<Crew>>,
}

impl std::fmt::Debug for KeepNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeepNote")
    }
}

/// `forget_note`, for the main agent.
pub(crate) struct ForgetNote {
    pub(crate) notes: Arc<KeptNotes>,
    pub(crate) crew: Option<Arc<Crew>>,
}

impl std::fmt::Debug for ForgetNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ForgetNote")
    }
}

fn parameter(
    name: &str,
    description: &str,
    param_type: ToolParameterType,
    required: bool,
) -> ToolParameterDefinition {
    ToolParameterDefinition {
        name: name.into(),
        description: description.into(),
        param_type,
        required,
        is_path: false,
        flag: None,
        default_value: None,
    }
}

fn definition(
    name: &str,
    description: &str,
    parameters: Vec<ToolParameterDefinition>,
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        output_based_example: None,
        parameters,
        parallel_safe: false,
        tool_type_details: ToolTypeDetails::DeveloperDefined,
        raw_json_schema_override: None,
    }
}

fn refused(why: impl Into<String>) -> ToolError {
    ToolError::ExecutionFailed(why.into())
}

#[async_trait]
impl Tool for KeepNote {
    fn definition(&self) -> ToolDefinition {
        definition(
            KEEP_NOTE,
            "Keeps a note for yourself or for workers. It comes back by itself after a handover or a compaction and in a new conversation's first message, so keep what must outlast this conversation, such as something the user asked you to do later. The notes hold 8 KB in all.",
            vec![
                parameter(
                    "text",
                    "The note, short and self-contained.",
                    ToolParameterType::String,
                    true,
                ),
                parameter(
                    "audience",
                    "lead (default), workers for every worker, or worker IDs such as r1, r2. A worker gets its notes in its first message, as they are kept while it runs, and after its own handover or compaction.",
                    ToolParameterType::String,
                    false,
                ),
            ],
        )
    }

    async fn execute(
        &self,
        _context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let text = params
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let audience = match params.get("audience") {
            None | Some(Value::Null) => Audience::Lead,
            Some(Value::String(audience)) => Audience::parse(audience).map_err(refused)?,
            Some(_) => return Err(refused("audience must be text")),
        };
        let workflow = self.crew.as_ref().and_then(|crew| crew.workflow());
        if let Audience::Workers(ids) = &audience {
            let Some(workflow) = &workflow else {
                return Err(refused(
                    "This session has no workers, so a note can only be for the lead.",
                ));
            };
            let workers = workflow.subagents();
            if let Some(unknown) = ids.iter().find(|id| {
                !workers
                    .iter()
                    .any(|worker| worker.handle.eq_ignore_ascii_case(id))
            }) {
                return Err(refused(format!("There is no worker {unknown}.")));
            }
        }
        let note = self.notes.add(text, audience).map_err(refused)?;
        let told = workflow
            .map(|workflow| self.notes.tell_workers(&workflow, &note.audience))
            .unwrap_or_default();
        Ok(kept_result(note.id, &told))
    }
}

#[async_trait]
impl Tool for ForgetNote {
    fn definition(&self) -> ToolDefinition {
        definition(
            FORGET_NOTE,
            "Removes a note you kept. The workers it was for are told, and one that already read it keeps what it read.",
            vec![parameter(
                "id",
                "The note's number.",
                ToolParameterType::Integer,
                true,
            )],
        )
    }

    async fn execute(
        &self,
        _context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let id = params
            .get("id")
            .and_then(|id| match id {
                Value::Number(number) => number.as_u64(),
                Value::String(text) => text.trim().parse().ok(),
                _ => None,
            })
            .and_then(|id| u32::try_from(id).ok())
            .ok_or_else(|| refused("id is the number of a kept note"))?;
        let note = self.notes.remove(id).map_err(refused)?;
        if let Some(workflow) = self.crew.as_ref().and_then(|crew| crew.workflow()) {
            self.notes.tell_removed(&workflow, &note, false);
        }
        Ok(format!("Forgot note {}.", note.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notes() -> (tempfile::TempDir, Arc<KeptNotes>) {
        let temp = tempfile::tempdir().unwrap();
        let notes = KeptNotes::new(temp.path());
        (temp, notes)
    }

    #[test]
    fn notes_are_kept_across_a_resume_and_removed_by_number() {
        let (temp, notes) = notes();
        let first = notes
            .add("  Tear the build box down.  ", Audience::Lead)
            .unwrap();
        let second = notes.add("Use tabs.", Audience::AllWorkers).unwrap();
        assert_eq!(
            (first.id, first.text.as_str()),
            (1, "Tear the build box down.")
        );
        // A session that opens the folder again has them.
        let resumed = KeptNotes::new(temp.path());
        assert_eq!(resumed.list().unwrap(), [first.clone(), second.clone()]);
        assert_eq!(resumed.remove(second.id).unwrap(), second);
        assert_eq!(KeptNotes::new(temp.path()).list().unwrap(), [first]);
        assert_eq!(
            resumed.remove(second.id).unwrap_err(),
            "There is no kept note 2."
        );
        // A removed note's number isn't used again.
        assert_eq!(resumed.add("Next.", Audience::Lead).unwrap().id, 3);
        assert!(resumed.add("  ", Audience::Lead).is_err());
    }

    #[test]
    fn the_notes_stay_under_the_size_cap() {
        let (_temp, notes) = notes();
        // The cap counts the list as the main agent gets it, so many small
        // notes stay under the framework's limit for one note too.
        let mut kept = 0;
        while notes.add(&"a".repeat(30), Audience::AllWorkers).is_ok() {
            kept += 1;
        }
        assert!(kept > 1);
        let refused = notes.add(&"b".repeat(200), Audience::Lead).unwrap_err();
        assert!(refused.contains("8 KB at most"), "{refused}");
        assert_eq!(notes.list().unwrap().len(), kept);
        assert!(notes.for_lead().unwrap().len() <= MAX_BYTES);
        // Forgetting makes room.
        notes.remove(1).unwrap();
        notes.add(&"b".repeat(20), Audience::Lead).unwrap();
    }

    #[test]
    fn a_file_that_cant_be_read_stays_as_it_is() {
        let (temp, notes) = notes();
        notes.add("One.", Audience::Lead).unwrap();
        let path = temp.path().join(FILE);
        std::fs::write(&path, "{ not json").unwrap();
        for why in [
            notes.add("Two.", Audience::Lead).unwrap_err(),
            notes.remove(1).unwrap_err(),
            notes.list().unwrap_err(),
        ] {
            assert!(why.contains("kept-notes.json"), "{why}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        assert!(notes.for_lead().is_none());
    }

    #[test]
    fn each_agent_gets_the_notes_for_it() {
        let (_temp, notes) = notes();
        notes.add("Tear the box down.", Audience::Lead).unwrap();
        // (Every kind of line break starts a line that is indented.)
        notes
            .add(
                "Use tabs.\r\nAlways.\u{2028}# Always.",
                Audience::AllWorkers,
            )
            .unwrap();
        notes
            .add("Only r2 reads this.", Audience::parse("R2").unwrap())
            .unwrap();
        let lead = notes.for_lead().unwrap();
        assert!(lead.starts_with("# Your kept notes"));
        for told in [
            "- Kept note 1, for you,",
            "- Kept note 2, for every worker,",
            "- Kept note 3, for r2,",
            "Tear the box down.",
            "  Use tabs.\n  Always.\n  # Always.",
        ] {
            assert!(lead.contains(told), "{told}\n{lead}");
        }
        let worker = notes.for_worker("r1").unwrap();
        assert!(worker.starts_with("# Notes from the main agent"));
        assert!(worker.contains("Use tabs.") && !worker.contains("Only r2"));
        assert!(!worker.contains("Tear the box down."));
        assert!(
            notes
                .for_worker("r2")
                .unwrap()
                .contains("Only r2 reads this.")
        );
        let alone = KeptNotes::new(tempfile::tempdir().unwrap().path());
        assert!(alone.for_lead().is_none() && alone.for_worker("r1").is_none());
    }

    #[test]
    fn keep_note_says_which_workers_have_the_note() {
        let told =
            |ids: &[&str]| kept_result(3, &ids.iter().map(|id| id.to_string()).collect::<Vec<_>>());
        assert_eq!(told(&[]), "Kept note 3.");
        assert_eq!(told(&["r1"]), "Kept note 3. r1 has it now.");
        assert_eq!(told(&["r1", "r2"]), "Kept note 3. r1 and r2 have it now.");
    }

    #[test]
    fn an_audience_is_the_lead_every_worker_or_workers_by_id() {
        assert_eq!(Audience::parse("").unwrap(), Audience::Lead);
        assert_eq!(Audience::parse("Lead").unwrap(), Audience::Lead);
        assert_eq!(Audience::parse("workers").unwrap(), Audience::AllWorkers);
        assert_eq!(
            Audience::parse("r1, r12 R3").unwrap(),
            Audience::Workers(vec!["r1".into(), "r12".into(), "r3".into()])
        );
        for wrong in ["everyone", "lead, r1", "r", "r1x"] {
            assert!(Audience::parse(wrong).is_err(), "{wrong}");
        }
    }
}

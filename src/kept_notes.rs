//! Notes the main agent keeps during a session, in the session's folder.
//! They come back on their own where a summary or a restated first message
//! may have lost them.
//!
//! Each note says who it is for. The main agent gets every note it kept, in
//! the first message of a conversation and again after a handover or a Claude
//! Code compaction, as a one-time note that lists the current ones (a
//! handover restates the first message as it was, so a note kept since isn't
//! in it). It gets the list once more when the session goes back to a
//! conversation (`/resume N`, `arko -c`), which may have missed changes. A
//! worker gets the notes for it in its first message, as they are kept while
//! it is active, and again after its own handover or compaction. Once notes
//! were kept, an agent whose notes are all gone gets a list that says so,
//! since its restated first message may still hold them. Nothing else is
//! sent on other turns, except one line for a note that is removed
//! (`/notes remove`, `forget_note`), to the main agent and to the active
//! workers the note was for.
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
/// come back after each handover and compaction, so they stay small.
pub const MAX_BYTES: usize = 16 * 1024;

/// The most characters a note or message that carries kept notes to a
/// running agent takes, its heading included, as the agent reads them
/// ([`shown_chars`]). A longer list goes in parts. With what the framework
/// wraps around a note (about 400 characters) a part stays under the 4,750
/// the framework always has room for in one of Claude Code's hooks beside
/// other context, so the hook stays under the 10,000 past which Claude Code
/// cuts it.
const PART_CHARS: usize = 4_000;
/// What the notes in a part take at most, which leaves room for its heading.
const PART_NOTES_CHARS: usize = PART_CHARS - 250;
// A part is within the framework's limit for one note, at four bytes a
// character.
const _: () = assert!(4 * PART_CHARS <= arko_workflows::notes::MAX_NOTE_BYTES);

/// What the worker is told above its notes.
const WORKER_HEADING: &str = "The main agent's notes for you. This list replaces any earlier one.";

/// What the worker is told above the removal of a note, which isn't a list.
const REMOVED_HEADING: &str = "The main agent's notes for you.";

/// The characters of a removed note's first line that its announcement quotes.
const QUOTED_CHARS: usize = 60;

/// What a list says when the notes it replaces are all gone.
const NONE_LEFT: &str = "No notes are kept now.";

/// What a call with an audience that isn't one is told.
const BAD_AUDIENCE: &str = "audience is lead, workers (every worker), worker IDs such as r1, r2, or a mix such as lead, r1";

/// One process writes the file at a time.
static WRITING: Mutex<()> = Mutex::new(());

/// Who a note is for. The main agent's list holds every note, so a note for
/// workers is also its own.
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
    /// worker IDs such as `r1, r2`, also mixed (`lead, r1`). Nothing means
    /// the lead. The lead's own list holds every note, so `lead` beside
    /// workers adds nothing and is dropped.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut every_worker = false;
        let mut ids: Vec<String> = Vec::new();
        for word in text
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
        {
            if word == "workers" {
                every_worker = true;
            } else if is_worker_id(&word) {
                if !ids.contains(&word) {
                    ids.push(word);
                }
            } else if word != "lead" {
                return Err(BAD_AUDIENCE.into());
            }
        }
        Ok(if every_worker {
            Self::AllWorkers
        } else if ids.is_empty() {
            Self::Lead
        } else {
            Self::Workers(ids)
        })
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
    /// When it was kept, or last replaced.
    pub created: DateTime<Utc>,
}

impl Note {
    /// The note as a list item. Its text is indented, so no line of it
    /// passes for a heading.
    fn item(&self, with_audience: bool) -> String {
        item(&self.head(with_audience), &self.text)
    }

    /// The first line of its list item: "- Kept note 3, for you, 2026-10-30
    /// 18:00 UTC".
    fn head(&self, with_audience: bool) -> String {
        let for_whom = match (&self.audience, with_audience) {
            (_, false) => String::new(),
            (Audience::Lead, true) => ", for you".to_string(),
            (audience, true) => format!(", for {}", audience.describe()),
        };
        format!(
            "- Kept note {}{for_whom}, {}",
            self.id,
            self.created.format("%Y-%m-%d %H:%M UTC")
        )
    }

    /// The note as list items that each fit in a part of a list: its whole
    /// item, or its text in pieces under its own line, then under "- Kept
    /// note 3, continued". Each piece ends at a line break near its end when
    /// there is one, else between two characters.
    fn pieces(&self, with_audience: bool) -> Vec<String> {
        let whole = self.item(with_audience);
        if fits_a_part(&whole) {
            return vec![whole];
        }
        let mut pieces = Vec::new();
        let mut rest = self.text.as_str();
        while !rest.is_empty() {
            let head = if pieces.is_empty() {
                self.head(with_audience)
            } else {
                format!("- Kept note {}, continued", self.id)
            };
            let end = cut(rest, |text| fits_a_part(&item(&head, text)));
            pieces.push(item(&head, &rest[..end]));
            rest = rest[end..].strip_prefix('\n').unwrap_or(&rest[end..]);
        }
        pieces
    }
}

/// A list item: `head`, then `text` with each line indented.
fn item(head: &str, text: &str) -> String {
    format!("{head}\n  {}", text.replace('\n', "\n  "))
}

/// Where to cut `text` for its start to fit (`fits`): after as much as fits,
/// at the last line break when one is in the last fifth of that, else
/// between two characters, and after one character at least.
fn cut(text: &str, fits: impl Fn(&str) -> bool) -> usize {
    let ends: Vec<usize> = text
        .char_indices()
        .map(|(at, c)| at + c.len_utf8())
        .collect();
    let end = ends[ends.partition_point(|&end| fits(&text[..end])).max(1) - 1];
    if end == text.len() || text[end..].starts_with('\n') {
        return end;
    }
    match text[..end].rfind('\n') {
        Some(at) if at > 0 && at >= end * 4 / 5 => at,
        _ => end,
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

/// The characters of `text` as an agent reads it in a note or a message,
/// which indents each of its lines by two.
fn shown_chars(text: &str) -> usize {
    text.chars().count() + 2 * text.lines().count()
}

/// Whether the notes `text` fit in one part of a list ([`PART_NOTES_CHARS`]).
fn fits_a_part(text: &str) -> bool {
    shown_chars(text) <= PART_NOTES_CHARS
}

/// `notes` as list items, joined in order into as few parts as hold them
/// ([`PART_NOTES_CHARS`]). A note too long for one part starts a part and
/// goes on in the next ([`Note::pieces`]).
fn parts(notes: &[Note], with_audience: bool) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for piece in notes.iter().flat_map(|note| note.pieces(with_audience)) {
        match parts.last_mut() {
            Some(part) if fits_a_part(&format!("{part}\n{piece}")) => {
                part.push('\n');
                part.push_str(&piece);
            }
            _ => parts.push(piece),
        }
    }
    parts
}

/// "the next note continues", "the next 2 notes continue": what follows the
/// first of `count` parts, each a `kind`.
fn rest_of(count: usize, kind: &str) -> String {
    match count - 1 {
        1 => format!("the next {kind} continues"),
        rest => format!("the next {rest} {kind}s continue"),
    }
}

/// The main agent's list of `notes` as the notes that carry it while it
/// runs: the whole list, or the list in parts when it is longer than one
/// part holds. Empty without notes.
fn lead_parts(notes: &[Note]) -> Vec<String> {
    let parts = parts(notes, true);
    let count = parts.len();
    if count <= 1 {
        return lead_text(notes).into_iter().collect();
    }
    parts
        .iter()
        .enumerate()
        .map(|(at, part)| {
            let about = if at == 0 {
                format!(
                    "You kept these with keep_note, and forget_note removes one. This list replaces any earlier one, and {} it.",
                    rest_of(count, "note")
                )
            } else {
                "This continues the list above.".to_string()
            };
            format!("# Your kept notes, {} of {count}\n\n{about}\n\n{part}", at + 1)
        })
        .collect()
}

/// A note's text as the framework reads a note: every kind of line break is
/// one, so every line is indented when the note is shown. An error when it
/// is empty.
fn note_text(text: &str) -> Result<String, String> {
    let text = text
        .replace("\r\n", "\n")
        .replace(['\r', '\u{85}', '\u{2028}', '\u{2029}'], "\n");
    let text = text.trim();
    if text.is_empty() {
        return Err("The note is empty.".into());
    }
    Ok(text.to_string())
}

/// Refuses a change that takes the main agent's list from the notes
/// `before` to the notes `after` past [`MAX_BYTES`]. The refusal lists each
/// note with its number, size and age, so the agent sees which to replace or
/// forget.
fn check_size(before: &[Note], after: &[Note]) -> Result<(), String> {
    let size = |notes: &[Note]| lead_text(notes).map_or(0, |list| list.len());
    let (held, total) = (size(before), size(after));
    if total <= MAX_BYTES {
        return Ok(());
    }
    let now = Utc::now();
    let mut refusal = format!(
        "Kept notes hold {} KB at most. They take {held} bytes now, and this would add {}. Replace a note (replaces), forget one, or shorten this one.",
        MAX_BYTES / 1024,
        total.saturating_sub(held)
    );
    for note in before {
        let age = (now - note.created).to_std().unwrap_or_default();
        refusal.push_str(&format!(
            "\n- Note {}, {} bytes, {} old",
            note.id,
            note.text.len(),
            crate::remote::duration_words(age)
        ));
    }
    Err(refusal)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    /// The ID the next note takes.
    next: u32,
    notes: Vec<Note>,
}

/// A session's kept notes.
pub struct KeptNotes {
    path: PathBuf,
    /// The task that follows the session's handovers and compactions.
    following: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Held from a change to the notes until the agents are told of it, and
    /// while a list is read and sent, so the agents get the lists in the
    /// order of the changes.
    telling: Mutex<()>,
    /// Runs once between a worker's list being read and sent, for tests that
    /// change the notes there.
    #[cfg(test)]
    pub(crate) before_sending: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// How many times a change waited for `telling` while it was held.
    #[cfg(test)]
    pub(crate) waited: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for KeptNotes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeptNotes")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl KeptNotes {
    /// The notes in `session_dir`, the session's folder.
    pub fn new(session_dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            path: session_dir.join(FILE),
            following: Mutex::new(None),
            telling: Mutex::new(()),
            #[cfg(test)]
            before_sending: Mutex::new(None),
            #[cfg(test)]
            waited: std::sync::atomic::AtomicUsize::new(0),
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
        let text = note_text(text)?;
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
            text,
            audience,
            created: Utc::now(),
        };
        let mut notes = saved.notes.clone();
        notes.push(note.clone());
        check_size(&saved.notes, &notes)?;
        saved.next = id;
        saved.notes = notes;
        self.write(&saved)?;
        Ok(note)
    }

    /// Replaces the text of note `id`, and its audience when `audience` is
    /// given. It keeps its number and place, counts toward [`MAX_BYTES`] at
    /// its new size, and dates from now. Returns the note as it was and as it
    /// is now.
    pub fn replace(
        &self,
        id: u32,
        text: &str,
        audience: Option<Audience>,
    ) -> Result<(Note, Note), String> {
        let text = note_text(text)?;
        let _writing = crate::lock(&WRITING);
        let mut saved = self.read()?;
        let Some(index) = saved.notes.iter().position(|note| note.id == id) else {
            let numbers: Vec<String> = saved.notes.iter().map(|note| note.id.to_string()).collect();
            return Err(match numbers.as_slice() {
                [] => format!("There is no kept note {id}, and no notes are kept."),
                [one] => format!("There is no kept note {id}. The only kept note is {one}."),
                _ => format!(
                    "There is no kept note {id}. The kept notes are {}.",
                    crate::and_list(&numbers)
                ),
            });
        };
        let was = saved.notes[index].clone();
        let note = Note {
            id,
            text,
            audience: audience.unwrap_or_else(|| was.audience.clone()),
            created: Utc::now(),
        };
        let mut notes = saved.notes.clone();
        notes[index] = note.clone();
        check_size(&saved.notes, &notes)?;
        saved.notes = notes;
        self.write(&saved)?;
        Ok((was, note))
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

    /// The notes for worker `handle`.
    fn worker_notes(&self, handle: &str) -> Vec<Note> {
        self.notes()
            .into_iter()
            .filter(|note| note.audience.includes(handle))
            .collect()
    }

    /// What worker `handle`'s first message starts with, its whole list.
    /// `None` without notes for it.
    pub(crate) fn for_worker(&self, handle: &str) -> Option<String> {
        let items: Vec<String> = self
            .worker_notes(handle)
            .iter()
            .map(|note| note.item(false))
            .collect();
        (!items.is_empty()).then(|| format!("# Notes from the main agent\n\n{}", items.join("\n")))
    }

    /// What worker `handle` is sent of its notes while it runs, as messages
    /// with their headings: its list, in parts when it is longer than one
    /// part holds, or that none are left when notes were kept and its are
    /// gone (its restated first message may still hold them).
    fn worker_messages(&self, handle: &str) -> Vec<(String, String)> {
        let notes = self.worker_notes(handle);
        if notes.is_empty() {
            return self
                .ever_kept()
                .then(|| (WORKER_HEADING.to_string(), NONE_LEFT.to_string()))
                .into_iter()
                .collect();
        }
        let parts = parts(&notes, false);
        let count = parts.len();
        parts
            .into_iter()
            .enumerate()
            .map(|(at, part)| {
                let heading = match (at, count) {
                    (_, 1) => WORKER_HEADING.to_string(),
                    (0, _) => format!(
                        "The main agent's notes for you, 1 of {count}. This list replaces any earlier one, and {} it.",
                        rest_of(count, "message")
                    ),
                    _ => format!(
                        "The main agent's notes for you, {} of {count}. This continues the list above.",
                        at + 1
                    ),
                };
                (heading, part)
            })
            .collect()
    }

    /// Takes `telling`, which a change and what it sends the agents hold.
    fn hold_telling(&self) -> std::sync::MutexGuard<'_, ()> {
        #[cfg(test)]
        if self.telling.try_lock().is_err() {
            self.waited
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        crate::lock(&self.telling)
    }

    /// Keeps a note and sends it to the active workers it is for, as one
    /// step among the changes to the notes. Returns the note and the workers
    /// that have it now.
    pub(crate) fn keep_and_tell(
        &self,
        text: &str,
        audience: Audience,
        workflow: Option<&Workflow>,
    ) -> Result<(Note, Vec<String>), String> {
        let _telling = self.hold_telling();
        let note = self.add(text, audience)?;
        let told = workflow
            .map(|workflow| self.tell_workers(workflow, |handle| note.audience.includes(handle)))
            .unwrap_or_default();
        Ok((note, told))
    }

    /// Replaces note `id` ([`replace`](Self::replace)) and sends their list
    /// again to the active workers it was or is for, so those that read it
    /// have the new text, and those it is no longer for have it gone. One
    /// step among the changes to the notes. Returns the note and the workers
    /// that have it now.
    pub(crate) fn replace_and_tell(
        &self,
        id: u32,
        text: &str,
        audience: Option<Audience>,
        workflow: Option<&Workflow>,
    ) -> Result<(Note, Vec<String>), String> {
        let _telling = self.hold_telling();
        let (was, note) = self.replace(id, text, audience)?;
        let told = workflow
            .map(|workflow| {
                self.tell_workers(workflow, |handle| {
                    was.audience.includes(handle) || note.audience.includes(handle)
                })
            })
            .unwrap_or_default()
            .into_iter()
            .filter(|handle| note.audience.includes(handle))
            .collect();
        Ok((note, told))
    }

    /// Removes note `id` and tells the agents ([`tell_removed`](Self::tell_removed)),
    /// as one step among the changes to the notes. Returns the note.
    pub(crate) fn remove_and_tell(
        &self,
        id: u32,
        workflow: Option<&Workflow>,
        by_user: bool,
    ) -> Result<Note, String> {
        let _telling = self.hold_telling();
        let note = self.remove(id)?;
        if let Some(workflow) = workflow {
            self.tell_removed(workflow, &note, by_user);
        }
        Ok(note)
    }

    /// Sends each active worker that `to` takes its notes. A worker reads
    /// them at its next step. The caller holds `telling`.
    fn tell_workers(&self, workflow: &Workflow, to: impl Fn(&str) -> bool) -> Vec<String> {
        let mut told = Vec::new();
        for record in workflow.subagents() {
            if record.state.is_active() && to(&record.handle) {
                told.extend(self.tell_worker(workflow, &record.handle));
            }
        }
        told
    }

    /// Sends worker `handle` its notes ([`worker_messages`](Self::worker_messages)).
    /// The caller holds `telling`.
    fn tell_worker(&self, workflow: &Workflow, handle: &str) -> Option<String> {
        let messages = self.worker_messages(handle);
        if messages.is_empty() {
            return None;
        }
        #[cfg(test)]
        if let Some(pause) = crate::lock(&self.before_sending).take() {
            pause();
        }
        for (heading, text) in messages {
            // No copy of it goes to the worker's lead: the main agent wrote it.
            if let Err(error) = workflow.message_subagent(handle, &text, &heading, "") {
                log::warn!("could not send {handle} its kept notes ({error})");
                return None;
            }
        }
        Some(handle.to_string())
    }

    /// Tells the agents that `note` is removed, by the user or (`by_user`
    /// unset) by the main agent. The main agent hears of it in a one-time
    /// note, unless it did it, and each active worker that had the note in a
    /// message. They keep what they read, and a handover or a compaction
    /// gives them the current list. The caller holds `telling`.
    fn tell_removed(&self, workflow: &Workflow, note: &Note, by_user: bool) {
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

    /// Posts the main agent's current list in one-time notes, for the
    /// conversation it continues ([`lead_parts`]). A list that says none are
    /// kept replaces one the conversation may still hold, when notes were
    /// ever kept.
    pub(crate) fn tell_lead(&self, workflow: &Workflow) {
        let _telling = self.hold_telling();
        self.post_lead_list(workflow);
    }

    /// [`tell_lead`](Self::tell_lead), for a caller that holds `telling`.
    fn post_lead_list(&self, workflow: &Workflow) {
        let mut parts = lead_parts(&self.notes());
        if parts.is_empty() && self.ever_kept() {
            parts.push(format!(
                "# Your kept notes\n\n{NONE_LEFT} This list replaces any earlier one."
            ));
        }
        for part in parts {
            if let Err(error) =
                workflow.post_note(Posted::new(part).source(NoteSource::Host).to([MAIN]).once())
            {
                log::warn!("could not give the main agent its kept notes again ({error})");
                return;
            }
        }
    }

    /// Gives run `run` of `agent` its notes again, in its fresh conversation.
    fn again(&self, workflow: &Workflow, agent: &str, run: uuid::Uuid) {
        let _telling = self.hold_telling();
        if agent == MAIN {
            self.post_lead_list(workflow);
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
            ["remove", number] => match number
                .parse()
                .map(|id| self.kept.remove_and_tell(id, Some(self.workflow()), true))
            {
                Ok(Ok(note)) => {
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

/// What `keep_note` answers: the note's number, whether it replaced that
/// note, and the workers that have it now.
fn kept_result(id: u32, replaced: bool, told: &[String]) -> String {
    let done = if replaced { "Replaced" } else { "Kept" };
    match told {
        [] => format!("{done} note {id}."),
        [one] => format!("{done} note {id}. {one} has it now."),
        several => format!(
            "{done} note {id}. {} have it now.",
            crate::and_list(several)
        ),
    }
}

/// A note's number in a tool call, as a number or as text.
fn note_number(value: &Value) -> Option<u32> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .and_then(|id| u32::try_from(id).ok())
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
        read_only: false,
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
            &format!(
                "Keeps a note for yourself or for workers. It comes back by itself after a handover or a compaction, when you go back to a conversation, and in a new conversation's first message, so keep what must outlast this conversation, such as something the user asked you to do later. The notes hold {} KB in all.",
                MAX_BYTES / 1024
            ),
            vec![
                parameter(
                    "text",
                    "The note, short and self-contained.",
                    ToolParameterType::String,
                    true,
                ),
                parameter(
                    "audience",
                    "lead (default), workers for every worker, or worker IDs such as r1, r2. A mix such as lead, r1 works too. Your own list holds every note, including those for workers, so a note for workers is also yours. A worker gets its notes in its first message, as they are kept while it runs, and after its own handover or compaction.",
                    ToolParameterType::String,
                    false,
                ),
                parameter(
                    "replaces",
                    "The number of a note to replace with this text, which keeps its number (and its audience unless you give one).",
                    ToolParameterType::Integer,
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
            None | Some(Value::Null) => None,
            Some(Value::String(audience)) => Some(Audience::parse(audience).map_err(refused)?),
            Some(_) => return Err(refused("audience must be text")),
        };
        let replaces = match params.get("replaces") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                note_number(value)
                    .ok_or_else(|| refused("replaces is the number of a kept note"))?,
            ),
        };
        let workflow = self.crew.as_ref().and_then(|crew| crew.workflow());
        if let Some(Audience::Workers(ids)) = &audience {
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
        let (note, told) = match replaces {
            None => self.notes.keep_and_tell(
                text,
                audience.unwrap_or(Audience::Lead),
                workflow.as_deref(),
            ),
            Some(id) => self
                .notes
                .replace_and_tell(id, text, audience, workflow.as_deref()),
        }
        .map_err(refused)?;
        Ok(kept_result(note.id, replaces.is_some(), &told))
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
            .and_then(note_number)
            .ok_or_else(|| refused("id is the number of a kept note"))?;
        let workflow = self.crew.as_ref().and_then(|crew| crew.workflow());
        let note = self
            .notes
            .remove_and_tell(id, workflow.as_deref(), false)
            .map_err(refused)?;
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
        assert!(refused.contains("16 KB at most"), "{refused}");
        // Each note is listed with its number, size and age, so the agent
        // sees which to replace or forget.
        assert!(
            refused.contains("Replace a note (replaces), forget one"),
            "{refused}"
        );
        assert!(refused.contains("\n- Note 1, 30 bytes, "), "{refused}");
        assert!(
            refused.lines().last().unwrap().ends_with(" s old"),
            "{refused}"
        );
        assert_eq!(refused.matches("\n- Note ").count(), kept);
        assert_eq!(notes.list().unwrap().len(), kept);
        let list = notes.for_lead().unwrap();
        assert!(list.len() <= MAX_BYTES && list.len() <= arko_workflows::notes::MAX_NOTE_BYTES);
        // Forgetting makes room.
        notes.remove(1).unwrap();
        notes.add(&"b".repeat(20), Audience::Lead).unwrap();
        // One note can take most of the 16 KB.
        let (_temp, notes) = self::notes();
        notes.add(&"c".repeat(15 * 1024), Audience::Lead).unwrap();
    }

    #[test]
    fn a_long_list_reaches_a_running_agent_in_parts() {
        let (_temp, notes) = notes();
        notes.add("Short.", Audience::AllWorkers).unwrap();
        // A short list goes whole, as the first message has it.
        assert_eq!(
            lead_parts(&notes.list().unwrap()),
            [lead_text(&notes.list().unwrap()).unwrap()]
        );
        assert_eq!(
            notes.worker_messages("r1"),
            [(
                WORKER_HEADING.to_string(),
                notes.list().unwrap()[0].item(false)
            )]
        );
        let texts: Vec<String> = (0..5)
            .map(|n| format!("{n} {}", "word ".repeat(300)).trim().to_string())
            .collect();
        for text in &texts {
            notes.add(text, Audience::AllWorkers).unwrap();
        }
        let parts = lead_parts(&notes.list().unwrap());
        assert_eq!(parts.len(), 3, "{parts:#?}");
        for (at, part) in parts.iter().enumerate() {
            assert!(shown_chars(part) <= PART_CHARS, "{}", shown_chars(part));
            assert!(part.starts_with(&format!("# Your kept notes, {} of 3\n\n", at + 1)));
        }
        assert!(
            parts[0]
                .contains("This list replaces any earlier one, and the next 2 notes continue it.")
        );
        assert!(parts[1].contains("\n\nThis continues the list above.\n\n- Kept note 4,"));
        // Every note is in one part whole, in order.
        let whole = parts.join("\n");
        let mut from = 0;
        for text in std::iter::once("Short.").chain(texts.iter().map(String::as_str)) {
            let at = whole[from..].find(text).expect(text) + from;
            assert_eq!(whole.matches(text).count(), 1);
            from = at;
        }
        // A new conversation's first message has the whole list.
        let first = notes.for_lead().unwrap();
        assert!(
            first.starts_with("# Your kept notes\n\n") && texts.iter().all(|t| first.contains(t))
        );
        // A worker gets its list in parts the same way.
        let messages = notes.worker_messages("r1");
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[0].0,
            "The main agent's notes for you, 1 of 3. This list replaces any earlier one, and the next 2 messages continue it."
        );
        assert_eq!(
            messages[2].0,
            "The main agent's notes for you, 3 of 3. This continues the list above."
        );
        assert!(
            messages
                .iter()
                .all(|(_, text)| shown_chars(text) <= PART_CHARS)
        );
        assert!(notes.for_worker("r1").unwrap().contains(&texts[4]));
        // The longest headings leave the notes their room.
        let headings = [
            format!(
                "# Your kept notes, 1 of 99\n\nYou kept these with keep_note, and forget_note removes one. This list replaces any earlier one, and {} it.\n\n",
                rest_of(99, "note")
            ),
            format!(
                "The main agent's notes for you, 1 of 99. This list replaces any earlier one, and {} it.\n\n",
                rest_of(99, "message")
            ),
        ];
        for heading in headings {
            assert!(
                shown_chars(&heading) + PART_NOTES_CHARS <= PART_CHARS,
                "{heading}"
            );
        }
    }

    /// The text of the notes in `parts` (their items' lines without the
    /// indent), and the first lines of their items.
    fn read_back(parts: &[String]) -> (Vec<String>, Vec<String>) {
        let lines = parts.iter().flat_map(|part| part.lines());
        let text = lines
            .clone()
            .filter_map(|line| line.strip_prefix("  "))
            .map(str::to_string)
            .collect();
        let heads = lines
            .filter(|line| line.starts_with("- Kept note"))
            .map(str::to_string)
            .collect();
        (text, heads)
    }

    #[test]
    fn a_long_note_goes_on_into_the_next_part() {
        let (_temp, notes) = notes();
        notes.add("Short.", Audience::AllWorkers).unwrap();
        // Lines: each part ends at a line break.
        let lines: Vec<String> = (0..40)
            .map(|n| format!("{n:02} {}", "x".repeat(96)))
            .collect();
        notes.add(&lines.join("\n"), Audience::AllWorkers).unwrap();
        // One line without breaks, of characters of two bytes.
        notes.add(&"é".repeat(5_000), Audience::AllWorkers).unwrap();
        let notes = notes.list().unwrap();
        for with_audience in [true, false] {
            let parts = parts(&notes, with_audience);
            for part in &parts {
                assert!(
                    shown_chars(part) <= PART_NOTES_CHARS,
                    "{}",
                    shown_chars(part)
                );
            }
            let (text, heads) = read_back(&parts);
            let mut expected = vec!["Short.".to_string()];
            expected.extend(lines.iter().cloned());
            assert_eq!(text[..41], expected[..], "every line whole, once, in order");
            assert_eq!(text[41..].concat(), "é".repeat(5_000));
            assert!(text.len() > 42, "the long line is cut");
            // The second note starts a part and goes on under its own line.
            assert!(parts[1].starts_with("- Kept note 2,"));
            assert!(heads.contains(&"- Kept note 2, continued".to_string()));
            assert!(heads.contains(&"- Kept note 3, continued".to_string()));
        }
        // A new conversation's first message has each note whole.
        let first = lead_text(&notes).unwrap();
        assert!(first.contains(&lines.join("\n  ")) && !first.contains("continued"));
    }

    #[test]
    fn a_saved_note_longer_than_a_part_goes_in_parts() {
        let (temp, notes) = notes();
        // Kept under the earlier 8 KB limit, it is 12,000 characters as the
        // agent reads it.
        let text = "a\n".repeat(2_000).trim().to_string();
        std::fs::write(
            temp.path().join(FILE),
            serde_json::json!({"next": 1, "notes": [
                {"id": 1, "text": text, "audience": "all_workers", "created": "2026-10-01T10:00:00Z"}
            ]})
            .to_string(),
        )
        .unwrap();
        let parts = lead_parts(&notes.list().unwrap());
        assert!(parts.len() >= 3, "{}", parts.len());
        for part in &parts {
            assert!(shown_chars(part) <= PART_CHARS, "{}", shown_chars(part));
        }
        let (lines, _) = read_back(&parts);
        assert_eq!(lines, vec!["a"; 2_000]);
        let messages = notes.worker_messages("r1");
        assert!(messages.len() >= 3);
        let texts: Vec<String> = messages.into_iter().map(|(_, text)| text).collect();
        assert_eq!(read_back(&texts).0, vec!["a"; 2_000]);
    }

    #[test]
    fn a_note_is_replaced_in_place_and_counts_at_its_new_size() {
        let (_temp, notes) = notes();
        let status = notes.add("Status: building.", Audience::Lead).unwrap();
        let shared = notes
            .add("Use tabs.", Audience::parse("r1").unwrap())
            .unwrap();
        let (was, now) = notes
            .replace(status.id, " Status: testing. ", None)
            .unwrap();
        assert_eq!(was, status);
        assert_eq!(
            (now.id, now.text.as_str(), &now.audience),
            (1, "Status: testing.", &Audience::Lead)
        );
        // Its number and place stay, and an audience given replaces its own.
        let (_, now) = notes
            .replace(shared.id, "Use spaces.", Some(Audience::AllWorkers))
            .unwrap();
        let texts: Vec<(u32, String)> = notes
            .list()
            .unwrap()
            .into_iter()
            .map(|note| (note.id, note.text))
            .collect();
        assert_eq!(
            texts,
            [(1, "Status: testing.".into()), (2, "Use spaces.".into())]
        );
        assert_eq!(now.audience, Audience::AllWorkers);
        assert!(notes.for_worker("r7").unwrap().contains("Use spaces."));
        // An unknown number says which there are.
        assert_eq!(
            notes.replace(9, "x", None).unwrap_err(),
            "There is no kept note 9. The kept notes are 1 and 2."
        );
        assert!(notes.replace(1, "  ", None).is_err());
        // The new text counts in place of the old, so a note that fills the
        // list can still be replaced by one as long.
        for _ in 0..3 {
            notes.add(&"f".repeat(5_000), Audience::Lead).unwrap();
        }
        let room = MAX_BYTES - notes.for_lead().unwrap().len();
        let long = "d".repeat(room + "Status: testing.".len());
        notes.replace(1, &long, None).unwrap();
        assert_eq!(notes.for_lead().unwrap().len(), MAX_BYTES);
        let refused = notes.replace(1, &format!("{long}e"), None).unwrap_err();
        assert!(refused.contains("this would add 1."), "{refused}");
        assert_eq!(notes.list().unwrap()[0].text, long);
        assert!(notes.add("More.", Audience::Lead).is_err());
    }

    #[test]
    fn keep_note_takes_the_number_of_a_note_to_replace() {
        let (_temp, notes) = notes();
        let keep = KeepNote { notes, crew: None }.definition();
        let replaces = keep
            .parameters
            .iter()
            .find(|parameter| parameter.name == "replaces")
            .unwrap();
        assert!(!replaces.required);
        assert!(keep.description.contains("The notes hold 16 KB in all."));
        assert_eq!(note_number(&serde_json::json!(3)), Some(3));
        assert_eq!(note_number(&serde_json::json!(" 4 ")), Some(4));
        assert_eq!(note_number(&serde_json::json!(-1)), None);
        assert_eq!(note_number(&serde_json::json!("four")), None);
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
        let told = |ids: &[&str]| {
            kept_result(
                3,
                false,
                &ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(told(&[]), "Kept note 3.");
        assert_eq!(told(&["r1"]), "Kept note 3. r1 has it now.");
        assert_eq!(told(&["r1", "r2"]), "Kept note 3. r1 and r2 have it now.");
        assert_eq!(
            kept_result(3, true, &["r1".to_string()]),
            "Replaced note 3. r1 has it now."
        );
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
        // The lead's own list holds every note, so `lead` beside workers
        // adds nothing, and a worker named twice is one.
        let r17 = || Audience::Workers(vec!["r17".into()]);
        for (mixed, parsed) in [
            ("lead, r17", r17()),
            ("r17 lead", r17()),
            ("LEAD,R17,r17", r17()),
            ("lead, workers", Audience::AllWorkers),
            ("workers, r2", Audience::AllWorkers),
            ("lead lead", Audience::Lead),
            (
                "r2, lead, r1, r2",
                Audience::Workers(vec!["r2".into(), "r1".into()]),
            ),
        ] {
            assert_eq!(Audience::parse(mixed).unwrap(), parsed, "{mixed}");
        }
        // A stray separator isn't a word.
        assert_eq!(Audience::parse("lead,").unwrap(), Audience::Lead);
        for wrong in [
            "everyone",
            "lead, bob",
            "r1, bob",
            "r",
            "r1x",
            "lead r",
            "workers, r1x",
        ] {
            let why = Audience::parse(wrong).unwrap_err();
            assert!(why.contains("a mix such as lead, r1"), "{wrong}\n{why}");
        }
    }

    #[test]
    fn keep_note_tells_how_audiences_work() {
        let (_temp, notes) = notes();
        let keep = KeepNote { notes, crew: None }.definition();
        let audience = keep
            .parameters
            .iter()
            .find(|parameter| parameter.name == "audience")
            .unwrap();
        for said in [
            "lead (default)",
            "workers for every worker",
            "worker IDs such as r1, r2",
            "A mix such as lead, r1",
            "Your own list holds every note, including those for workers",
        ] {
            assert!(audience.description.contains(said), "{said}");
        }
    }

    #[test]
    fn a_note_for_workers_is_in_the_lead_list_too() {
        let (_temp, notes) = notes();
        notes
            .add(
                "Skip the slow tests.",
                Audience::parse("lead, r17").unwrap(),
            )
            .unwrap();
        let lead = notes.for_lead().unwrap();
        assert!(lead.contains("- Kept note 1, for r17,"), "{lead}");
        assert!(lead.contains("Skip the slow tests."), "{lead}");
        assert!(notes.for_worker("r17").is_some() && notes.for_worker("r1").is_none());
    }

    #[test]
    fn notes_saved_before_a_mixed_audience_still_load() {
        let (temp, notes) = notes();
        std::fs::write(
            temp.path().join(FILE),
            r#"{"next":3,"notes":[
                {"id":1,"text":"A.","audience":"lead","created":"2026-10-01T10:00:00Z"},
                {"id":2,"text":"B.","audience":{"workers":["r1","r2"]},"created":"2026-10-01T10:00:00Z"},
                {"id":3,"text":"C.","audience":"all_workers","created":"2026-10-01T10:00:00Z"}]}"#,
        )
        .unwrap();
        let audiences: Vec<Audience> = notes
            .list()
            .unwrap()
            .into_iter()
            .map(|note| note.audience)
            .collect();
        assert_eq!(
            audiences,
            [
                Audience::Lead,
                Audience::Workers(vec!["r1".into(), "r2".into()]),
                Audience::AllWorkers
            ]
        );
    }
}

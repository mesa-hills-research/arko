//! Dictation in the full-screen view. `/dictate` and Ctrl+R start a
//! recording, and press again to stop it (Enter stops it too). The words go
//! into the box at the cursor as they settle, the tentative ones dimmed
//! after it. The text then stays to edit, and Enter sends it. Esc cancels
//! and takes the recording's words out again.
//!
//! The view keeps the recording's state. What it asks of the service and
//! the microphone comes out as [`Dictate`] effects, which the view's loop
//! carries out, and the service's events come back through the feed
//! ([`App::dictation_event`]).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, Effect};
use super::overlay::Overlay;
use crate::dictation::{
    self, DictationError, DictationEvent, DictationUpdate, Preparing, SpeechModel,
};
use crate::remote::Level;

/// What the view asks of the dictation service and the microphone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dictate {
    /// Open a dictation (`download`: the user agreed to download the
    /// model). The loop answers with [`App::dictation_opened`] or
    /// [`App::dictation_refused`].
    Open { download: bool },
    /// The model listens: start the microphone. A failure comes to
    /// [`App::dictation_failed`].
    Record,
    /// Stop the microphone, and let the last words settle.
    Finish,
    /// Stop the microphone and end the dictation, with no more words.
    Cancel,
    /// The dictation ended: let go of it and of the microphone.
    Close,
}

/// Where a recording is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Asked of the service.
    Starting,
    Preparing(Preparing),
    Listening,
    /// Stopped. The last words are coming.
    Finishing,
}

/// The view's recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    /// The dictation's ID, once the service opened it.
    pub id: Option<u64>,
    pub phase: Phase,
    /// Where the latest run of settled words starts in the box, and the
    /// words, which Esc takes out while they're still there as they were.
    start: usize,
    inserted: String,
    /// The model was downloaded first, which takes minutes: the microphone
    /// doesn't open by surprise once it's done.
    downloaded: bool,
}

impl Recording {
    /// Whether it's in a phase that takes Enter and Esc. A download runs
    /// beside the box, which works as usual meanwhile.
    fn takes_keys(&self) -> bool {
        !matches!(
            self.phase,
            Phase::Preparing(Preparing::Downloading { .. } | Preparing::Packing)
        )
    }
}

impl App {
    /// `/dictate` and Ctrl+R: starts a recording, stops the one listening,
    /// or cancels one still getting ready.
    pub fn toggle_dictation(&mut self) {
        match self.recording.as_ref().map(|recording| recording.phase) {
            None => self.start_dictation(false),
            Some(Phase::Listening) => self.stop_dictation(),
            Some(Phase::Starting | Phase::Preparing(_)) => self.cancel_dictation(),
            Some(Phase::Finishing) => {}
        }
    }

    fn start_dictation(&mut self, download: bool) {
        self.recording = Some(Recording {
            id: None,
            phase: Phase::Starting,
            start: self.editor.cursor(),
            inserted: String::new(),
            downloaded: false,
        });
        self.effects_push(Effect::Dictate(Dictate::Open { download }));
        if dictation::over_ssh() {
            self.say(dictation::OVER_SSH);
        }
    }

    /// The service opened the recording as `id`.
    pub fn dictation_opened(&mut self, id: u64) {
        if let Some(recording) = &mut self.recording {
            recording.id = Some(id);
        }
    }

    /// The service couldn't open it. A missing model asks first.
    pub fn dictation_refused(&mut self, error: DictationError) {
        self.recording = None;
        match error {
            DictationError::NeedsDownload(model) => {
                self.overlay = Some(Overlay::Download { model, choice: 0 })
            }
            DictationError::Busy | DictationError::Unavailable => {
                self.effects_push(Effect::Notice(Level::Warn, error.to_string()))
            }
            DictationError::Stopped => {
                self.effects_push(Effect::Notice(Level::Error, error.to_string()))
            }
        }
    }

    /// The microphone couldn't start, and the loop let go of the
    /// dictation: the recording ends, saying why in the transcript.
    pub fn dictation_failed(&mut self, why: String) {
        self.end_recording();
        self.effects_push(Effect::Notice(Level::Error, why));
    }

    /// A dictation's event, from the feed: the view's own recording's
    /// changes the box and the status line.
    pub fn dictation_event(&mut self, update: DictationUpdate) {
        let ours = self
            .recording
            .as_ref()
            .is_some_and(|recording| recording.id == Some(update.id));
        if !ours {
            return;
        }
        match update.event {
            DictationEvent::Preparing(step) => {
                if let Some(recording) = &mut self.recording {
                    recording.downloaded |= matches!(step, Preparing::Downloading { .. });
                }
                self.set_phase(Phase::Preparing(step));
            }
            DictationEvent::Listening if self.recording.as_ref().is_some_and(|r| r.downloaded) => {
                self.end_recording();
                self.say(MODEL_READY);
                self.effects_push(Effect::Dictate(Dictate::Cancel));
            }
            DictationEvent::Listening => {
                self.set_phase(Phase::Listening);
                self.effects_push(Effect::Dictate(Dictate::Record));
            }
            DictationEvent::Text { settled, tentative } => self.dictated(&settled, &tentative),
            DictationEvent::Done => {
                self.end_recording();
                self.effects_push(Effect::Dictate(Dictate::Close));
            }
            DictationEvent::Failed(why) => {
                self.end_recording();
                self.effects_push(Effect::Notice(Level::Error, why));
                self.effects_push(Effect::Dictate(Dictate::Close));
            }
        }
    }

    fn set_phase(&mut self, phase: Phase) {
        if let Some(recording) = &mut self.recording {
            recording.phase = phase;
        }
    }

    /// Settled words go in at the cursor, and the tentative ones show after
    /// it. Words typed elsewhere meanwhile start a new run where the cursor
    /// is.
    fn dictated(&mut self, settled: &str, tentative: &str) {
        let Some(recording) = &mut self.recording else {
            return;
        };
        let editor = &mut self.editor;
        let mut wrote = !recording.inserted.is_empty();
        if !settled.is_empty() {
            let end = recording.start + recording.inserted.chars().count();
            let goes_on = editor.cursor() == end
                && editor.slice(recording.start, end).as_deref() == Some(&recording.inserted);
            if !goes_on {
                recording.start = editor.cursor();
                recording.inserted.clear();
            }
            let text = spaced(editor.before_cursor(), wrote, settled);
            editor.insert_text(&text);
            recording.inserted.push_str(&text);
            wrote = true;
        }
        editor.set_tentative(&spaced(editor.before_cursor(), wrote, tentative));
    }

    /// Ctrl+R or Enter while listening: the microphone stops and the last
    /// words come.
    fn stop_dictation(&mut self) {
        self.set_phase(Phase::Finishing);
        self.effects_push(Effect::Dictate(Dictate::Finish));
    }

    /// Esc: the recording ends, and its words leave the box, if they're
    /// still there as they came.
    fn cancel_dictation(&mut self) {
        let Some(recording) = self.recording.take() else {
            return;
        };
        let end = recording.start + recording.inserted.chars().count();
        if !recording.inserted.is_empty()
            && self.editor.slice(recording.start, end).as_deref() == Some(&recording.inserted)
        {
            self.editor.remove(recording.start, end);
        }
        self.editor.set_tentative("");
        self.effects_push(Effect::Dictate(Dictate::Cancel));
    }

    fn end_recording(&mut self) {
        self.recording = None;
        self.editor.set_tentative("");
    }

    /// Enter and Esc while recording; false for any other key, and while a
    /// download runs beside the box.
    pub(super) fn dictation_key(&mut self, key: KeyEvent) -> bool {
        let Some(recording) = self.recording.as_ref().filter(|r| r.takes_keys()) else {
            return false;
        };
        match key.code {
            KeyCode::Esc => self.cancel_dictation(),
            // (Finishing: the words aren't all in yet.)
            KeyCode::Enter if key.modifiers.is_empty() => {
                if recording.phase == Phase::Listening {
                    self.stop_dictation();
                }
            }
            _ => return false,
        }
        true
    }

    /// The recording's state for the status line, and its keys.
    pub fn dictation_status(&self) -> Option<(String, &'static [(&'static str, &'static str)])> {
        let recording = self.recording.as_ref()?;
        Some(match recording.phase {
            Phase::Starting => (String::new(), &[("Esc", "cancel")]),
            Phase::Preparing(step @ (Preparing::Downloading { .. } | Preparing::Packing)) => {
                (lowercase(&step.label()), &[("Ctrl+R", "cancel")])
            }
            Phase::Preparing(step) => (lowercase(&step.label()), &[("Esc", "cancel")]),
            Phase::Listening => ("listening".into(), &[("Enter", "stop"), ("Esc", "cancel")]),
            Phase::Finishing => (String::new(), &[("Esc", "cancel")]),
        })
    }

    /// A key on the download question: Download starts the recording that
    /// asked, with the download.
    pub(super) fn download_key(
        &mut self,
        key: KeyEvent,
        model: &'static SpeechModel,
        choice: usize,
    ) -> Option<Overlay> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                Some(Overlay::Download {
                    model,
                    choice: 1 - choice.min(1),
                })
            }
            KeyCode::Enter if choice == 0 => {
                self.start_dictation(true);
                None
            }
            KeyCode::Enter | KeyCode::Esc => None,
            KeyCode::Char('c') if ctrl => None,
            _ => Some(Overlay::Download { model, choice }),
        }
    }
}

/// What the status line says once a download is done.
const MODEL_READY: &str = "Speech model downloaded. Press Ctrl+R to dictate.";

/// The status line's words start in lower case, as its others do.
fn lowercase(label: &str) -> String {
    let mut chars = label.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_lowercase().chain(chars).collect()
    })
}

/// `text` with a space before it, where it's the recording's first and the
/// box has a word right before the cursor.
fn spaced(before: Option<char>, wrote: bool, text: &str) -> String {
    let joins = !wrote
        && !text.starts_with(char::is_whitespace)
        && before.is_some_and(|c| !c.is_whitespace());
    if joins && !text.is_empty() {
        format!(" {text}")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
#[path = "dictation_tests.rs"]
mod tests;

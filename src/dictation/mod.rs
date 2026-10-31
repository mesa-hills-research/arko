//! Dictation: speech typed into the message box, recognized on this
//! computer. One [`DictationService`] per process runs the speech engine on
//! a thread of its own, one recording at a time. A front end opens a
//! [`Dictation`], hands it the microphone's audio (its [`Dictation::record`]
//! does that), and the text comes back as [`DictationEvent`]s to the
//! [`Sink`] it opened the dictation with. They're the recording client's
//! alone, so each recorder takes them on a channel of its own, never through
//! the session's feed.
//!
//! The engine and the microphone are behind the `dictation` feature.
//! Without it, [`DictationService::open`] answers that this build has none.
//! The model is downloaded once, after the user agrees, into
//! `models/speech/` in Arko's folder ([`models`]). Recordings are never
//! stored.

pub mod models;
pub mod settings;

#[cfg(feature = "dictation")]
mod capture;
#[cfg(feature = "dictation")]
mod download;
#[cfg(not(feature = "dictation"))]
mod no_engine;
#[cfg(feature = "dictation")]
mod service;

use std::path::PathBuf;

#[cfg(feature = "dictation")]
pub use capture::Capture;
pub use models::SpeechModel;
#[cfg(not(feature = "dictation"))]
pub use no_engine::{Capture, Dictation, DictationService};
#[cfg(feature = "dictation")]
pub use service::{Dictation, DictationService};

/// What a dictation reports, in order: preparing steps, `Listening`, text as
/// it's recognized, then `Done` (or `Failed` at any point).
#[derive(Debug, Clone, PartialEq)]
pub enum DictationEvent {
    /// The model is being made ready: downloaded, packed or loaded.
    Preparing(Preparing),
    /// The model is ready and the audio is taken from now on.
    Listening,
    /// `settled` holds the words that settled since the last event, to
    /// append. `tentative` holds the words after them that may still
    /// change, replacing those shown before.
    Text { settled: String, tentative: String },
    /// The last words have settled, after `Dictation::finish`.
    Done,
    /// The dictation ended, with the sentence to show.
    Failed(String),
}

/// A step of making the model ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preparing {
    /// Bytes of the model's files downloaded so far, of `total`.
    Downloading { done: u64, total: u64 },
    /// Checking the download and converting it to the engine's pack.
    Packing,
    /// Loading the pack into memory.
    Loading,
}

impl Preparing {
    /// The step in a few words, as a status line shows it.
    pub fn label(&self) -> String {
        match self {
            Preparing::Downloading { done, total } => format!(
                "Downloading speech model ({} of {} MB)",
                done / 1_000_000,
                models::megabytes(*total)
            ),
            Preparing::Packing => "Preparing speech model".into(),
            Preparing::Loading => "Loading speech model".into(),
        }
    }
}

/// A dictation's event, with the dictation it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct DictationUpdate {
    /// The dictation's ID, [`Dictation::id`].
    pub id: u64,
    pub event: DictationEvent,
}

/// Where a dictation's events go. Called on the service's thread.
pub type Sink = Box<dyn FnMut(DictationUpdate) + Send>;

/// What [`DictationService::open`] needs to know.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DictationOptions {
    /// The user agreed to download the model now, if it's missing. Without
    /// it a missing model is [`DictationError::NeedsDownload`], unless the
    /// settings download it without asking.
    pub download: bool,
}

/// Why a dictation couldn't start.
#[derive(Debug, Clone, PartialEq)]
pub enum DictationError {
    /// This build has no dictation (no `dictation` feature).
    Unavailable,
    /// Another dictation is running in this process.
    Busy,
    /// The model isn't downloaded yet, and the user should be asked first.
    NeedsDownload(&'static SpeechModel),
    /// The service's thread is gone.
    Stopped,
}

impl std::fmt::Display for DictationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DictationError::Unavailable => f.write_str("This build of Arko has no dictation."),
            DictationError::Busy => f.write_str("Dictation is already running in another window."),
            DictationError::NeedsDownload(model) => {
                write!(f, "{} isn't downloaded yet.", model.title)
            }
            DictationError::Stopped => {
                f.write_str("Dictation stopped working. Restart Arko to use it again.")
            }
        }
    }
}

impl std::error::Error for DictationError {}

/// Where a recording's audio comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The computer's default microphone.
    Microphone,
    /// A WAV file played in real time, then silence: for tests, and for a
    /// computer without a microphone.
    File(PathBuf),
}

/// The variable that makes the terminal record from a WAV file instead of
/// the microphone.
pub const WAV_VARIABLE: &str = "ARKO_DICTATION_WAV";

impl Source {
    /// The file `ARKO_DICTATION_WAV` names, else the microphone.
    pub fn from_env() -> Source {
        match std::env::var_os(WAV_VARIABLE) {
            Some(path) if !path.is_empty() => Source::File(path.into()),
            _ => Source::Microphone,
        }
    }
}

/// What the terminal says when Arko runs over SSH, where the microphone it
/// records from isn't the user's.
pub const OVER_SSH: &str = "Dictation records from the computer Arko runs on";

/// Whether Arko runs in an SSH session.
pub fn over_ssh() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some_and(|value| !value.is_empty())
}

/// The download question's title and buttons.
pub const DOWNLOAD_TITLE: &str = "Download a speech model?";
pub const DOWNLOAD: &str = "Download";
pub const CANCEL: &str = "Cancel";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparing_steps_read_as_the_status_line_shows_them() {
        let step = Preparing::Downloading {
            done: 412_345_678,
            total: 948_011_281,
        };
        assert_eq!(step.label(), "Downloading speech model (412 of 948 MB)");
        assert_eq!(Preparing::Packing.label(), "Preparing speech model");
        assert_eq!(Preparing::Loading.label(), "Loading speech model");
    }

    #[test]
    fn errors_say_what_happened() {
        assert_eq!(
            DictationError::Busy.to_string(),
            "Dictation is already running in another window."
        );
        assert_eq!(
            DictationError::NeedsDownload(&models::GRANITE).to_string(),
            "Granite Speech 5.0 isn't downloaded yet."
        );
    }
}

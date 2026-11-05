//! Dictation is speech typed into the message box, recognized on this
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
pub mod session_words;
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
pub use session_words::{SessionWord, favour_session_words, session_words};

/// What a dictation reports, in order, meaning preparing steps, `Listening`, text as
/// it's recognized, then `Done` (or `Failed` at any point).
#[derive(Debug, Clone, PartialEq)]
pub enum DictationEvent {
    /// The model is being made ready, by being downloaded, packed or loaded.
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

/// A step of making the models ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preparing {
    /// Bytes of `model`'s files downloaded so far, of `total`.
    Downloading {
        model: ModelKind,
        done: u64,
        total: u64,
    },
    /// Checking the speech model's download and converting it to the
    /// engine's pack.
    Packing,
    /// Loading the speech model into memory.
    Loading,
}

impl Preparing {
    /// The step in a few words, as a status line shows it.
    pub fn label(&self) -> String {
        match self {
            Preparing::Downloading { model, done, total } => format!(
                "Downloading {} ({} of {} MB)",
                model.name(),
                done / 1_000_000,
                models::megabytes(*total)
            ),
            Preparing::Packing => "Preparing speech model".into(),
            Preparing::Loading => "Loading speech model".into(),
        }
    }
}

/// Which of dictation's models a download brings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    Speech,
    /// Full cleanup's model.
    Cleanup,
}

impl ModelKind {
    /// Its name in a sentence.
    pub fn name(self) -> &'static str {
        match self {
            ModelKind::Speech => "speech model",
            ModelKind::Cleanup => "cleanup model",
        }
    }
}

/// What a recording downloads before it starts, which the user is asked
/// about first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Download {
    /// The speech model, when it isn't downloaded yet.
    pub speech: Option<&'static SpeechModel>,
    /// The bytes of full cleanup's model, when it's chosen and isn't
    /// downloaded yet.
    pub cleanup: Option<u64>,
}

impl Download {
    /// The question's title.
    pub fn title(&self) -> &'static str {
        match (self.speech, self.cleanup) {
            (Some(_), Some(_)) => "Download the speech and cleanup models?",
            (None, Some(_)) => "Download the cleanup model?",
            _ => "Download a speech model?",
        }
    }

    /// What the question says of the models.
    pub fn offer(&self) -> String {
        let cleanup = self.cleanup.map(|bytes| {
            format!(
                "Full cleanup runs on this computer with a model of its own ({} MB download).",
                models::megabytes(bytes)
            )
        });
        let speech = self.speech.map(SpeechModel::offer);
        speech
            .into_iter()
            .chain(cleanup)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The step the download starts on. Full cleanup's model comes first.
    pub fn first_step(&self) -> Preparing {
        match (self.cleanup, self.speech) {
            (Some(bytes), _) => Preparing::Downloading {
                model: ModelKind::Cleanup,
                done: 0,
                total: bytes,
            },
            (None, speech) => Preparing::Downloading {
                model: ModelKind::Speech,
                done: 0,
                total: speech.map_or(0, SpeechModel::download_bytes),
            },
        }
    }
}

/// What a front end says once a recording's downloads are done, given which
/// models came.
pub fn downloaded(speech: bool, cleanup: bool) -> &'static str {
    match (speech, cleanup) {
        (true, true) => "Speech and cleanup models downloaded.",
        (false, true) => "Cleanup model downloaded.",
        _ => "Speech model downloaded.",
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
    /// The user agreed to download the models now, if they're missing.
    /// Without it a missing model is [`DictationError::NeedsDownload`],
    /// unless the settings download without asking.
    pub download: bool,
}

/// Why a dictation couldn't start.
#[derive(Debug, Clone, PartialEq)]
pub enum DictationError {
    /// This build has no dictation (no `dictation` feature).
    Unavailable,
    /// Another dictation is running in this process.
    Busy,
    /// A model isn't downloaded yet, and the user should be asked first.
    NeedsDownload(Download),
    /// The service's thread is gone.
    Stopped,
}

impl std::fmt::Display for DictationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DictationError::Unavailable => f.write_str("This build of Arko has no dictation."),
            DictationError::Busy => f.write_str("Dictation is already running in another window."),
            DictationError::NeedsDownload(download) => match download.speech {
                Some(model) if download.cleanup.is_some() => write!(
                    f,
                    "{} and full cleanup's model aren't downloaded yet.",
                    model.title
                ),
                Some(model) => write!(f, "{} isn't downloaded yet.", model.title),
                None => f.write_str("Full cleanup's model isn't downloaded yet."),
            },
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
    /// A WAV file played in real time, then silence, for tests and for a
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

/// The download question's buttons. Its title and text are the
/// [`Download`]'s.
pub const DOWNLOAD: &str = "Download";
pub const CANCEL: &str = "Cancel";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparing_steps_read_as_the_status_line_shows_them() {
        let step = Preparing::Downloading {
            model: ModelKind::Speech,
            done: 412_345_678,
            total: 948_011_281,
        };
        assert_eq!(step.label(), "Downloading speech model (412 of 948 MB)");
        let step = Preparing::Downloading {
            model: ModelKind::Cleanup,
            done: 12_000_000,
            total: 157_673_903,
        };
        assert_eq!(step.label(), "Downloading cleanup model (12 of 158 MB)");
        assert_eq!(Preparing::Packing.label(), "Preparing speech model");
        assert_eq!(Preparing::Loading.label(), "Loading speech model");
    }

    #[test]
    fn errors_say_what_happened() {
        assert_eq!(
            DictationError::Busy.to_string(),
            "Dictation is already running in another window."
        );
        let speech = Download {
            speech: Some(&models::GRANITE),
            cleanup: None,
        };
        assert_eq!(
            DictationError::NeedsDownload(speech).to_string(),
            "Granite Speech 5.0 isn't downloaded yet."
        );
        let cleanup = Download {
            speech: None,
            cleanup: Some(157_673_903),
        };
        assert_eq!(
            DictationError::NeedsDownload(cleanup).to_string(),
            "Full cleanup's model isn't downloaded yet."
        );
        let both = Download {
            cleanup: Some(157_673_903),
            ..speech
        };
        assert_eq!(
            DictationError::NeedsDownload(both).to_string(),
            "Granite Speech 5.0 and full cleanup's model aren't downloaded yet."
        );
    }

    /// The question names each model a recording downloads, and the
    /// download starts on the cleanup model's.
    #[test]
    fn the_question_names_what_downloads() {
        let speech = Download {
            speech: Some(&models::GRANITE),
            cleanup: None,
        };
        assert_eq!(speech.title(), "Download a speech model?");
        assert_eq!(speech.offer(), models::GRANITE.offer());
        assert_eq!(
            speech.first_step(),
            Preparing::Downloading {
                model: ModelKind::Speech,
                done: 0,
                total: models::GRANITE.download_bytes()
            }
        );
        let both = Download {
            cleanup: Some(157_673_903),
            ..speech
        };
        assert_eq!(both.title(), "Download the speech and cleanup models?");
        assert_eq!(
            both.offer(),
            format!(
                "{} Full cleanup runs on this computer with a model of its own (158 MB download).",
                models::GRANITE.offer()
            )
        );
        assert_eq!(
            both.first_step(),
            Preparing::Downloading {
                model: ModelKind::Cleanup,
                done: 0,
                total: 157_673_903
            }
        );
        let cleanup = Download {
            speech: None,
            ..both
        };
        assert_eq!(cleanup.title(), "Download the cleanup model?");
        assert_eq!(downloaded(false, true), "Cleanup model downloaded.");
        assert_eq!(
            downloaded(true, true),
            "Speech and cleanup models downloaded."
        );
    }
}

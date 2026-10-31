//! The service: one thread per process that runs the speech engine, one
//! dictation at a time. It loads the model on the first recording, keeps it
//! while recordings follow, and drops it after the idle time the settings
//! give.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use speech::model::ModelType;
use speech::stream::{Event, StreamConfig, Streamer};
use speech::{Model, Options};

use super::capture::{self, Capture};
use super::download;
use super::models::{Family, SpeechModel};
use super::settings::Settings;
use super::{
    DictationError, DictationEvent, DictationOptions, DictationUpdate, Preparing, Sink, Source,
};

/// Where models download from.
const HUGGING_FACE: &str = "https://huggingface.co";
/// How often a dictation waiting for audio looks whether it was cancelled.
const POLL: Duration = Duration::from_millis(100);
/// The most audio that may wait for the engine, in seconds. More means the
/// computer can't keep up, and the dictation fails rather than fall further
/// behind.
const BACKLOG_SECONDS: usize = 15;
/// What a dictation says when its audio piles up.
const OVERLOADED: &str = "Dictation can't keep up on this computer.";

/// Runs dictations, one at a time, on a thread of its own.
pub struct DictationService {
    root: PathBuf,
    jobs: mpsc::Sender<Job>,
    /// A [`Dictation`] is open.
    busy: Arc<AtomicBool>,
    next_id: AtomicU64,
}

/// A dictation for the service's thread.
struct Job {
    id: u64,
    model: &'static SpeechModel,
    audio: mpsc::Receiver<Message>,
    intake: Arc<Intake>,
    sink: Sink,
    idle: Duration,
}

enum Message {
    Audio {
        samples: Vec<f32>,
        rate: u32,
    },
    /// More than [`BACKLOG_SECONDS`] of audio waited.
    Overloaded,
    /// The microphone stopped, for this reason.
    CaptureFailed(String),
    Finish,
}

/// What a dictation's handle, its microphone and the service's thread share.
#[derive(Default)]
struct Intake {
    cancelled: AtomicBool,
    /// Samples sent and not yet taken.
    queued: AtomicUsize,
    overloaded: AtomicBool,
}

impl Intake {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Sends audio, unless too much already waits, which overloads the
    /// dictation once.
    fn send(&self, sender: &mpsc::Sender<Message>, samples: &[f32], rate: u32) {
        if self.is_cancelled() || self.overloaded.load(Ordering::SeqCst) {
            return;
        }
        let limit = rate as usize * BACKLOG_SECONDS;
        if self.queued.fetch_add(samples.len(), Ordering::SeqCst) + samples.len() > limit {
            self.queued.fetch_sub(samples.len(), Ordering::SeqCst);
            if !self.overloaded.swap(true, Ordering::SeqCst) {
                let _ = sender.send(Message::Overloaded);
            }
            return;
        }
        let _ = sender.send(Message::Audio {
            samples: samples.to_vec(),
            rate,
        });
    }
}

/// The model in memory, and its pack.
struct Loaded {
    pack: PathBuf,
    model: Arc<Model>,
}

impl DictationService {
    /// A service for Arko's folder `root`, whose thread starts now. Arko
    /// itself uses [`shared`](Self::shared).
    pub fn new(root: &Path) -> Self {
        Self::downloading_from(root, HUGGING_FACE)
    }

    /// A service that downloads models from `base` in place of Hugging Face.
    pub(crate) fn downloading_from(root: &Path, base: &str) -> Self {
        let (jobs, received) = mpsc::channel();
        let (thread_root, base) = (root.to_path_buf(), base.to_string());
        std::thread::Builder::new()
            .name("arko-dictation".into())
            .spawn(move || serve(&thread_root, &base, received))
            .expect("a thread for dictation");
        Self {
            root: root.to_path_buf(),
            jobs,
            busy: Arc::default(),
            next_id: AtomicU64::new(0),
        }
    }

    /// The process's service, for Arko's folder `root` (the first caller's).
    pub fn shared(root: &Path) -> &'static DictationService {
        static SHARED: OnceLock<DictationService> = OnceLock::new();
        SHARED.get_or_init(|| DictationService::new(root))
    }

    /// The model the settings choose: theirs, else the one for the language
    /// (the setting's, else the system's), else Granite.
    pub fn model(&self) -> &'static SpeechModel {
        choose(&Settings::load(&self.root))
    }

    /// Starts a dictation, whose events go to `sink`. A missing model is
    /// downloaded first only when the user agreed (`options.download`) or
    /// the settings download without asking.
    pub fn open(&self, options: DictationOptions, sink: Sink) -> Result<Dictation, DictationError> {
        let settings = Settings::load(&self.root);
        let model = choose(&settings);
        if !model.is_installed(&self.root)
            && !options.download
            && !settings.downloads_without_asking()
        {
            return Err(DictationError::NeedsDownload(model));
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(DictationError::Busy);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, audio) = mpsc::channel();
        let intake = Arc::new(Intake::default());
        let job = Job {
            id,
            model,
            audio,
            intake: intake.clone(),
            sink,
            idle: settings.idle(),
        };
        let dictation = Dictation {
            id,
            sender,
            intake,
            busy: self.busy.clone(),
        };
        if self.jobs.send(job).is_err() {
            return Err(DictationError::Stopped);
        }
        Ok(dictation)
    }
}

/// The model `settings` choose.
fn choose(settings: &Settings) -> &'static SpeechModel {
    if let Some(model) = SpeechModel::named(&settings.model) {
        return model;
    }
    let family = settings
        .language()
        .and_then(|code| ModelType::for_language(&code))
        .unwrap_or(ModelType::DEFAULT);
    match family {
        ModelType::Parakeet => &super::models::PARAKEET,
        ModelType::Granite => &super::models::GRANITE,
    }
}

/// The engine's name for a model family.
pub(super) fn engine_type(family: Family) -> ModelType {
    match family {
        Family::Granite => ModelType::Granite,
        Family::Parakeet => ModelType::Parakeet,
    }
}

/// An open dictation. Keep it until [`DictationEvent::Done`] or
/// [`DictationEvent::Failed`] comes: dropping it cancels the dictation, as
/// [`cancel`](Self::cancel) does, and nothing more is sent.
pub struct Dictation {
    id: u64,
    sender: mpsc::Sender<Message>,
    intake: Arc<Intake>,
    busy: Arc<AtomicBool>,
}

impl Dictation {
    /// The ID its events carry.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Mono audio at `rate` samples a second. Audio before
    /// [`DictationEvent::Listening`] waits for the model. More than 15
    /// seconds of audio waiting fails the dictation, since the computer
    /// can't keep up.
    pub fn push(&self, samples: &[f32], rate: u32) {
        self.intake.send(&self.sender, samples, rate);
    }

    /// Records from `source` into this dictation until the [`Capture`] is
    /// dropped. A microphone that stops fails the dictation.
    pub fn record(&self, source: &Source) -> Result<Capture, String> {
        let (sender, intake) = (self.sender.clone(), self.intake.clone());
        let failed = self.sender.clone();
        capture::open(
            source,
            Box::new(move |samples: &[f32], rate: u32| intake.send(&sender, samples, rate)),
            Box::new(move |why: String| {
                let _ = failed.send(Message::CaptureFailed(why));
            }),
        )
    }

    /// Ends the audio. The last words settle, then [`DictationEvent::Done`]
    /// comes.
    pub fn finish(&mut self) {
        let _ = self.sender.send(Message::Finish);
    }

    /// Ends it at once, with no more events (a download stops, and resumes
    /// with the next dictation).
    pub fn cancel(self) {}
}

impl Drop for Dictation {
    fn drop(&mut self) {
        // (After the dictation ended, this changes nothing.)
        self.intake.cancelled.store(true, Ordering::SeqCst);
        self.busy.store(false, Ordering::SeqCst);
    }
}

/// The service's thread: each dictation in turn, the model dropped once
/// none came for the idle time.
fn serve(root: &Path, base: &str, jobs: mpsc::Receiver<Job>) {
    let mut loaded: Option<Loaded> = None;
    let mut idle = Settings::default().idle();
    loop {
        let job = if loaded.is_some() {
            match jobs.recv_timeout(idle) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => {
                    loaded = None;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match jobs.recv() {
                Ok(job) => job,
                Err(_) => return,
            }
        };
        let mut job = job;
        idle = job.idle;
        // (The full-screen view's panic hook leaves this thread's panics to
        // it, and to the engine's own threads, which it rethrows here.)
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(root, base, &mut job, &mut loaded)
        }));
        if ran.is_err() {
            loaded = None;
            if !job.intake.is_cancelled() {
                (job.sink)(DictationUpdate {
                    id: job.id,
                    event: DictationEvent::Failed(
                        "Speech recognition stopped unexpectedly.".into(),
                    ),
                });
            }
        }
    }
}

/// One dictation: the model made ready, then the audio until it ends.
/// Nothing is sent once it's cancelled.
fn run(root: &Path, base: &str, job: &mut Job, loaded: &mut Option<Loaded>) {
    let id = job.id;
    let intake = job.intake.clone();
    let sink = &mut job.sink;
    let mut send = |event: DictationEvent| {
        if !intake.is_cancelled() {
            sink(DictationUpdate { id, event });
        }
    };
    let pack = job.model.pack(root);
    if !pack.is_file() {
        // Each megabyte once.
        let mut shown = u64::MAX;
        let mut progress = |step: Preparing| {
            if let Preparing::Downloading { done, .. } = step {
                if done / 1_000_000 == shown {
                    return;
                }
                shown = done / 1_000_000;
            }
            send(DictationEvent::Preparing(step));
        };
        match download::install(job.model, root, base, &mut progress, &intake.cancelled) {
            Ok(()) => {}
            Err(download::Error::Cancelled) => return,
            Err(download::Error::Failed(message)) => {
                send(DictationEvent::Failed(message));
                return;
            }
        }
    }
    if intake.is_cancelled() {
        return;
    }
    let model = match loaded.as_ref().filter(|loaded| loaded.pack == pack) {
        Some(loaded) => loaded.model.clone(),
        None => {
            // The other model leaves memory first.
            *loaded = None;
            send(DictationEvent::Preparing(Preparing::Loading));
            match Model::load(&pack, &Options::default()) {
                Ok(model) => {
                    let model = Arc::new(model);
                    *loaded = Some(Loaded {
                        pack,
                        model: model.clone(),
                    });
                    model
                }
                Err(error) => {
                    send(DictationEvent::Failed(format!(
                        "Couldn't load the speech model ({error}). Delete {} to download it again.",
                        job.model.dir(root).display()
                    )));
                    return;
                }
            }
        }
    };
    if intake.is_cancelled() {
        return;
    }
    send(DictationEvent::Listening);
    listen(&job.audio, &intake, model, &mut send);
}

/// When to look at the audio next: at the pass that's due, else after
/// [`POLL`], since new audio wakes it anyway.
fn wait_for(next_pass: Option<Instant>, now: Instant) -> Duration {
    match next_pass {
        Some(at) if at > now => (at - now).min(POLL),
        _ => POLL,
    }
}

/// Takes the audio, running passes as Telmo's `Stream` does, until the
/// dictation finishes, fails or is cancelled.
fn listen(
    audio: &mpsc::Receiver<Message>,
    intake: &Intake,
    model: Arc<Model>,
    send: &mut dyn FnMut(DictationEvent),
) {
    let mut streamer: Option<Streamer> = None;
    let mut shown = Shown::default();
    let mut next_pass: Option<Instant> = None;
    let failed = |error: speech::Error| {
        DictationEvent::Failed(format!("Speech recognition failed ({error})."))
    };
    loop {
        let mut finishing = false;
        // Everything waiting is taken before a pass, so passes never fall
        // behind. The backlog's bound keeps that short.
        let mut message = audio.recv_timeout(wait_for(next_pass, Instant::now()));
        loop {
            if intake.is_cancelled() {
                return;
            }
            match message {
                Ok(Message::Audio { samples, rate }) => {
                    intake.queued.fetch_sub(samples.len(), Ordering::SeqCst);
                    let streamer = streamer.get_or_insert_with(|| {
                        Streamer::new(
                            model.clone(),
                            StreamConfig {
                                input_rate: rate,
                                ..StreamConfig::default()
                            },
                        )
                    });
                    // (Only voice activity's events, which change no text.)
                    streamer.push(&samples);
                }
                Ok(Message::Overloaded) => return send(DictationEvent::Failed(OVERLOADED.into())),
                Ok(Message::CaptureFailed(why)) => {
                    return send(DictationEvent::Failed(format!(
                        "The microphone stopped ({why})."
                    )));
                }
                Ok(Message::Finish) | Err(RecvTimeoutError::Disconnected) => finishing = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
            match audio.try_recv() {
                Ok(next) => message = Ok(next),
                Err(_) => break,
            }
        }
        if finishing {
            if let Some(streamer) = &mut streamer {
                match streamer.finish() {
                    Ok(events) => {
                        if let Some(text) = shown.take(events) {
                            send(text);
                        }
                    }
                    Err(error) => return send(failed(error)),
                }
            }
            return send(DictationEvent::Done);
        }
        let Some(streamer) = &mut streamer else {
            continue;
        };
        if !streamer.in_utterance() {
            next_pass = None;
            continue;
        }
        let now = Instant::now();
        let config = streamer.config().clone();
        let due = *next_pass.get_or_insert(now + config.min_interval);
        if streamer.wants_final() || (now >= due && streamer.has_new_audio()) {
            let start = Instant::now();
            match streamer.pass() {
                Ok(events) => {
                    if let Some(text) = shown.take(events) {
                        send(text);
                    }
                }
                Err(error) => return send(failed(error)),
            }
            let took = start.elapsed();
            next_pass = Some(
                start
                    + config
                        .min_interval
                        .max(took.mul_f64(config.interval_factor)),
            );
        }
    }
}

/// The tentative words shown, by utterance: an utterance's last pass can
/// come after the next one has started.
#[derive(Default)]
struct Shown {
    tentative: BTreeMap<u64, String>,
}

impl Shown {
    /// What a batch of the engine's events changes, as one event.
    fn take(&mut self, events: Vec<Event>) -> Option<DictationEvent> {
        let before = self.tentative();
        let mut settled = String::new();
        for event in events {
            match event {
                Event::Settled { text, .. } => settled.push_str(&text),
                Event::Tentative { utterance, text } => {
                    self.tentative.insert(utterance, text);
                }
                Event::UtteranceEnd { utterance, .. } => {
                    self.tentative.remove(&utterance);
                }
                Event::SpeechStart { .. } => {}
            }
        }
        let tentative = self.tentative();
        (!settled.is_empty() || tentative != before)
            .then_some(DictationEvent::Text { settled, tentative })
    }

    fn tentative(&self) -> String {
        self.tentative.values().map(String::as_str).collect()
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;

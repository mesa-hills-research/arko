//! Audio capture behind an interface of Arko's own, so the microphone crate
//! (cpal) can be replaced: [`open`] starts a [`Source`] and hands its audio,
//! mono at the source's own rate, to a callback until the [`Capture`] is
//! dropped.

use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use super::Source;

/// What takes the audio: mono samples, and their rate.
pub type OnAudio = Box<dyn FnMut(&[f32], u32) + Send>;

/// A source being recorded, until dropped.
pub struct Capture {
    /// Dropped to stop the capture's thread.
    _stop: mpsc::Sender<()>,
}

/// Starts recording `source` into `on_audio`.
pub fn open(source: &Source, on_audio: OnAudio) -> Result<Capture, String> {
    match source {
        Source::Microphone => microphone(on_audio),
        Source::File(path) => wav_file(path, on_audio),
    }
}

/// The length of the pieces a WAV file is played in.
const PIECE: Duration = Duration::from_millis(20);

/// Plays a WAV file in real time, in 20 ms pieces, then silence, as a
/// microphone would.
fn wav_file(path: &Path, mut on_audio: OnAudio) -> Result<Capture, String> {
    let audio = speech::wav::read(path).map_err(|error| match error {
        // (It names the file.)
        speech::Error::Io { .. } => format!("{error}."),
        other => format!("Couldn't read {} ({other}).", path.display()),
    })?;
    let rate = audio.sample_rate;
    let (stop, stopped) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("arko-dictation-file".into())
        .spawn(move || {
            let piece = (rate as usize / 50).max(1);
            let silence = vec![0.0; piece];
            let mut pieces = audio.samples.chunks(piece);
            let mut next = Instant::now();
            loop {
                on_audio(pieces.next().unwrap_or(&silence), rate);
                next += PIECE;
                match stopped.recv_timeout(next.saturating_duration_since(Instant::now())) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => return,
                }
            }
        })
        .map_err(|error| format!("Couldn't start recording ({error})."))?;
    Ok(Capture { _stop: stop })
}

/// Records the default microphone at its own rate and sample format, mixed
/// to mono. The stream lives on a thread of its own, since cpal's isn't
/// `Send` on every system.
fn microphone(on_audio: OnAudio) -> Result<Capture, String> {
    let (stop, stopped) = mpsc::channel::<()>();
    let (ready, started) = mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("arko-microphone".into())
        .spawn(move || {
            use cpal::traits::StreamTrait;
            let stream = match input_stream(on_audio) {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            if let Err(error) = stream.play() {
                let _ = ready.send(Err(format!("Couldn't start the microphone ({error}).")));
                return;
            }
            let _ = ready.send(Ok(()));
            // Until the capture is dropped.
            let _ = stopped.recv();
        })
        .map_err(|error| format!("Couldn't start recording ({error})."))?;
    started
        .recv()
        .unwrap_or_else(|_| Err("The microphone stopped.".into()))?;
    Ok(Capture { _stop: stop })
}

fn input_stream(on_audio: OnAudio) -> Result<cpal::Stream, String> {
    use cpal::SampleFormat as F;
    use cpal::traits::{DeviceTrait, HostTrait};
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("No microphone found.")?;
    let config = device
        .default_input_config()
        .map_err(|error| format!("Couldn't open the microphone ({error})."))?;
    let format = config.sample_format();
    let config: cpal::StreamConfig = config.into();
    let device = &device;
    let config = &config;
    match format {
        F::F32 => build::<f32>(device, config, on_audio),
        F::F64 => build::<f64>(device, config, on_audio),
        F::I8 => build::<i8>(device, config, on_audio),
        F::I16 => build::<i16>(device, config, on_audio),
        F::I32 => build::<i32>(device, config, on_audio),
        F::I64 => build::<i64>(device, config, on_audio),
        F::U8 => build::<u8>(device, config, on_audio),
        F::U16 => build::<u16>(device, config, on_audio),
        F::U32 => build::<u32>(device, config, on_audio),
        F::U64 => build::<u64>(device, config, on_audio),
        other => Err(format!(
            "The microphone's sample format ({other}) isn't supported."
        )),
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut on_audio: OnAudio,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::traits::DeviceTrait;
    let channels = config.channels.max(1) as usize;
    let rate = config.sample_rate.0;
    let mut mono = Vec::new();
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                mono.clear();
                mono.extend(data.chunks(channels).map(|frame| {
                    frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / frame.len() as f32
                }));
                on_audio(&mono, rate);
            },
            |error| log::warn!("microphone: {error}"),
            None,
        )
        .map_err(|error| format!("Couldn't open the microphone ({error})."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A WAV file of `seconds` of a tone at `rate`, 16-bit mono.
    pub(crate) fn tone_wav(path: &Path, rate: u32, seconds: f32) {
        let count = (rate as f32 * seconds) as usize;
        let mut data = Vec::with_capacity(count * 2);
        for n in 0..count {
            let value = (n as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 8000.0;
            data.extend_from_slice(&(value as i16).to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, wav).unwrap();
    }

    /// A file plays in real time, in 20 ms pieces at its own rate, then
    /// silence, and stops when the capture is dropped.
    #[test]
    fn a_wav_file_plays_like_a_microphone_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        tone_wav(&path, 48_000, 0.1);
        let got: Arc<Mutex<Vec<(usize, u32, bool)>>> = Arc::default();
        let into = got.clone();
        let start = Instant::now();
        let capture = open(
            &Source::File(path),
            Box::new(move |samples, rate| {
                let silent = samples.iter().all(|s| *s == 0.0);
                into.lock().unwrap().push((samples.len(), rate, silent));
            }),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        drop(capture);
        let pieces = got.lock().unwrap().clone();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(got.lock().unwrap().len(), pieces.len(), "stopped");
        assert!(start.elapsed() >= Duration::from_millis(200));
        // About ten pieces in 200 ms, the first five of the tone.
        assert!((7..=12).contains(&pieces.len()), "{pieces:?}");
        assert!(
            pieces
                .iter()
                .all(|(len, rate, _)| *len == 960 && *rate == 48_000)
        );
        assert!(pieces[..5].iter().all(|(_, _, silent)| !silent));
        assert!(pieces[5..].iter().all(|(_, _, silent)| *silent));
    }

    #[test]
    fn a_file_that_isnt_there_says_so() {
        let Err(error) = open(
            &Source::File("/nowhere/clip.wav".into()),
            Box::new(|_, _| {}),
        ) else {
            panic!("opened");
        };
        assert!(
            error.starts_with("Couldn't read /nowhere/clip.wav ("),
            "{error}"
        );
        assert_eq!(error.matches("/nowhere/clip.wav").count(), 1, "{error}");
    }
}

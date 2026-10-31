//! The service: asking before a download, one dictation at a time,
//! cancelling, and (with a real model) a recording from a WAV file end to
//! end.

use std::sync::mpsc::Receiver;

use super::*;
use crate::dictation::models::GRANITE;

/// Arko's folder with `dictation.json` holding `settings`.
fn root_with(settings: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(super::super::settings::FILE), settings).unwrap();
    root
}

/// A sink, and where its updates arrive.
fn sink() -> (Sink, Receiver<DictationUpdate>) {
    let (sender, received) = mpsc::channel();
    (
        Box::new(move |update| {
            let _ = sender.send(update);
        }),
        received,
    )
}

const NO_SERVER: &str = "http://127.0.0.1:1";

#[test]
fn a_missing_model_asks_before_it_downloads() {
    let root = root_with(r#"{"model": "granite"}"#);
    let service = DictationService::downloading_from(root.path(), NO_SERVER);
    let (sink, _) = sink();
    let Err(error) = service.open(DictationOptions::default(), sink) else {
        panic!("opened without the model");
    };
    assert_eq!(error, DictationError::NeedsDownload(&GRANITE));
}

#[test]
fn the_settings_choose_the_model() {
    let root = root_with(r#"{"model": "parakeet"}"#);
    let service = DictationService::new(root.path());
    assert_eq!(service.model().name, "parakeet");
    let root = root_with(r#"{"language": "pl"}"#);
    assert_eq!(DictationService::new(root.path()).model().name, "parakeet");
    let root = root_with(r#"{"language": "ja_JP"}"#);
    assert_eq!(DictationService::new(root.path()).model().name, "granite");
    // A language neither model knows takes the default.
    let root = root_with(r#"{"language": "zh"}"#);
    assert_eq!(DictationService::new(root.path()).model().name, "granite");
}

/// With the user's agreement (or the setting), a missing model downloads,
/// and a failed download says why.
#[test]
fn an_agreed_download_that_fails_says_why() {
    for (settings, download) in [
        (r#"{"model": "granite"}"#, true),
        (r#"{"model": "granite", "download": "automatic"}"#, false),
    ] {
        let root = root_with(settings);
        let service = DictationService::downloading_from(root.path(), NO_SERVER);
        let (sink, updates) = sink();
        let dictation = service.open(DictationOptions { download }, sink).unwrap();
        let update = updates.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(update.id, dictation.id());
        assert_eq!(
            update.event,
            DictationEvent::Failed("Couldn't download the speech model (couldn't reach Hugging Face). Start dictation again to resume.".into())
        );
    }
}

#[test]
fn one_dictation_at_a_time() {
    let root = root_with(r#"{"model": "granite"}"#);
    let service = DictationService::downloading_from(root.path(), NO_SERVER);
    let open = || service.open(DictationOptions { download: true }, sink().0);
    let first = open().unwrap();
    assert!(matches!(open(), Err(DictationError::Busy)));
    let first_id = first.id();
    first.cancel();
    let second = open().unwrap();
    assert_ne!(second.id(), first_id);
    // A finished one leaves the place to the next at once.
    second.finish();
    assert!(open().is_ok());
}

/// A dictation cancelled while it waits for the server sends nothing
/// more, and the service goes on to the next.
#[test]
fn a_cancelled_dictation_sends_nothing_more() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let held: Vec<std::net::TcpStream> = listener.incoming().flatten().collect();
        drop(held);
    });
    let root = root_with(r#"{"model": "granite"}"#);
    let service = DictationService::downloading_from(root.path(), &base);
    let (first_sink, first) = sink();
    service
        .open(DictationOptions { download: true }, first_sink)
        .unwrap()
        .cancel();
    let (second_sink, second) = sink();
    let next = service
        .open(DictationOptions { download: true }, second_sink)
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(first.try_recv().is_err(), "the cancelled one spoke");
    // The second waits for the server in its turn.
    assert!(second.try_recv().is_err());
    drop(next);
}

#[test]
fn a_batch_of_the_engines_events_is_one_text_event() {
    let mut shown = Shown::default();
    let tentative = |utterance, text: &str| Event::Tentative {
        utterance,
        text: text.into(),
    };
    let settled = |utterance, text: &str| Event::Settled {
        utterance,
        text: text.into(),
    };
    assert_eq!(
        shown.take(vec![Event::SpeechStart {
            utterance: 0,
            at: 0.2
        }]),
        None
    );
    assert_eq!(
        shown.take(vec![tentative(0, "I saw at")]),
        Some(DictationEvent::Text {
            settled: String::new(),
            tentative: "I saw at".into()
        })
    );
    assert_eq!(
        shown.take(vec![settled(0, "I saw"), tentative(0, " at the")]),
        Some(DictationEvent::Text {
            settled: "I saw".into(),
            tentative: " at the".into()
        })
    );
    // The next utterance starts before the last one's final pass.
    assert_eq!(
        shown.take(vec![tentative(1, " Then")]),
        Some(DictationEvent::Text {
            settled: String::new(),
            tentative: " at the Then".into()
        })
    );
    assert_eq!(
        shown.take(vec![
            settled(0, " at the museum."),
            tentative(0, ""),
            Event::UtteranceEnd {
                utterance: 0,
                at: 3.0,
                speech_end: 2.4
            }
        ]),
        Some(DictationEvent::Text {
            settled: " at the museum.".into(),
            tentative: " Then".into()
        })
    );
    assert_eq!(shown.take(vec![tentative(1, " Then")]), None);
}

/// A recording from a WAV file, through the service and Granite Speech's
/// real pack: the model loads, the words come and settle, and `finish`
/// ends it. A second recording finds the model loaded. It runs only with
/// `ARKO_TEST_SPEECH_PACK` naming Granite's int8 pack and
/// `ARKO_TEST_SPEECH_WAV` LibriSpeech's clip 260-123286-0018 (Telmo's
/// tests have both).
#[test]
fn a_wav_file_is_dictated_end_to_end() {
    let (Some(pack), Some(wav)) = (
        std::env::var_os("ARKO_TEST_SPEECH_PACK"),
        std::env::var_os("ARKO_TEST_SPEECH_WAV"),
    ) else {
        eprintln!("skipped: ARKO_TEST_SPEECH_PACK and ARKO_TEST_SPEECH_WAV aren't set");
        return;
    };
    let root = root_with(r#"{"model": "granite"}"#);
    let dir = GRANITE.dir(root.path());
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&pack, GRANITE.pack(root.path())).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(&pack, GRANITE.pack(root.path())).unwrap();
    let service = DictationService::downloading_from(root.path(), NO_SERVER);
    let source = Source::File(wav.into());

    let record = |expect_loading: bool| {
        let (sink, updates) = sink();
        let dictation = service.open(DictationOptions::default(), sink).unwrap();
        let next = || {
            updates
                .recv_timeout(Duration::from_secs(60))
                .expect("an event")
                .event
        };
        if expect_loading {
            assert_eq!(next(), DictationEvent::Preparing(Preparing::Loading));
        }
        assert_eq!(next(), DictationEvent::Listening);
        let capture = dictation.record(&source).unwrap();
        let (mut settled, mut tentatives) = (String::new(), 0);
        // The clip is 5.7 s, then silence ends the utterance.
        while !settled.contains("length") {
            match next() {
                DictationEvent::Text {
                    settled: words,
                    tentative,
                } => {
                    settled.push_str(&words);
                    tentatives += usize::from(!tentative.is_empty());
                }
                other => panic!("{other:?} while listening"),
            }
        }
        drop(capture);
        dictation.finish();
        loop {
            match next() {
                DictationEvent::Text { settled: words, .. } => settled.push_str(&words),
                DictationEvent::Done => break,
                other => panic!("{other:?} while finishing"),
            }
        }
        (settled, tentatives)
    };
    let (text, tentatives) = record(true);
    let lower = text.to_lowercase();
    for word in ["i saw", "hamburg", "skeleton", "thirty feet in length"] {
        assert!(lower.contains(word), "{text:?}");
    }
    assert!(tentatives > 0, "no tentative words before they settled");
    let (again, _) = record(false);
    assert!(again.to_lowercase().contains("hamburg"), "{again:?}");
}

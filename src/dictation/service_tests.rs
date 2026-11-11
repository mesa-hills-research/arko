//! The service, covering asking before a download, one dictation at a time,
//! cancelling, and (with a real model) a recording from a WAV file end to
//! end.

use std::sync::mpsc::Receiver;

use super::*;
use crate::dictation::models::{GRANITE, PACK_FILE};
use speech::disfluency::Punctuation;

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
    let missing = Download {
        speech: Some(&GRANITE),
        cleanup: None,
    };
    assert_eq!(error, DictationError::NeedsDownload(missing));
    assert_eq!(service.missing(), Some(missing));
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
    let mut second = open().unwrap();
    assert_ne!(second.id(), first_id);
    // A finished one keeps its place until it's let go, after Done.
    second.finish();
    assert!(matches!(open(), Err(DictationError::Busy)));
    drop(second);
    assert!(open().is_ok());
}

/// Audio that piles up past the backlog overloads the dictation once, and
/// later audio is dropped.
#[test]
fn audio_past_the_backlog_overloads_once() {
    let (sender, received) = mpsc::channel();
    let intake = Intake::default();
    let second = vec![0.0; 16_000];
    for _ in 0..BACKLOG_SECONDS {
        intake.send(&sender, &second, 16_000);
    }
    assert!(!intake.overloaded.load(Ordering::SeqCst));
    intake.send(&sender, &second, 16_000);
    intake.send(&sender, &second, 16_000);
    let messages: Vec<Message> = received.try_iter().collect();
    assert_eq!(messages.len(), BACKLOG_SECONDS + 1);
    assert!(matches!(messages.last(), Some(Message::Overloaded)));
    assert_eq!(
        intake.queued.load(Ordering::SeqCst),
        BACKLOG_SECONDS * 16_000
    );
    // Cancelled, nothing more goes.
    let intake = Intake::default();
    intake.cancelled.store(true, Ordering::SeqCst);
    intake.send(&sender, &second, 16_000);
    assert!(received.try_recv().is_err());
}

fn start(utterance: u64) -> Event {
    Event::SpeechStart { utterance, at: 0.0 }
}

fn end(utterance: u64) -> Event {
    Event::UtteranceEnd {
        utterance,
        at: 0.0,
        speech_end: 0.0,
    }
}

fn words(utterance: u64) -> Event {
    Event::Tentative {
        utterance,
        text: "words".into(),
    }
}

/// Recognition that keeps up stays about a pass behind the audio, and
/// silence is never behind.
#[test]
fn recognition_that_keeps_up_isnt_behind() {
    let mut lag = Lag::default();
    lag.took(2.0, &[]);
    assert_eq!(lag.behind(), 0.0);
    lag.took(0.5, &[start(0)]);
    assert_eq!(lag.behind(), 0.5);
    lag.passed(lag.taken, &[words(0)]);
    assert_eq!(lag.behind(), 0.0);
    // A long utterance, passes as it grows.
    for _ in 0..60 {
        lag.took(0.5, &[]);
        let heard = lag.taken;
        lag.took(0.2, &[]);
        lag.passed(heard, &[words(0)]);
        assert!(lag.behind() < 0.5, "{}", lag.behind());
    }
    // The next utterance starts as this one waits for its final pass.
    lag.took(0.5, &[start(1)]);
    let heard = lag.taken;
    lag.passed(heard, &[words(0), end(0)]);
    assert_eq!(lag.behind(), 0.0);
    lag.passed(lag.taken, &[words(1), end(1)]);
    lag.took(3.0, &[]);
    assert_eq!(lag.behind(), 0.0);
}

/// Utterances that wait for their final passes keep recognition behind,
/// however many passes run, until it's past the bound.
#[test]
fn utterances_waiting_for_final_passes_fall_behind() {
    let mut lag = Lag::default();
    lag.took(1.0, &[start(0)]);
    let mut utterance = 0;
    while lag.behind() <= LAG_SECONDS {
        utterance += 1;
        lag.took(2.0, &[start(utterance)]);
        let heard = lag.taken;
        lag.passed(heard, &[words(utterance)]);
    }
    assert!(utterance < 20);
    // A pass that began long ago leaves recognition that far behind.
    let mut lag = Lag::default();
    lag.took(1.0, &[start(0)]);
    let heard = lag.taken;
    lag.took(40.0, &[]);
    lag.passed(heard, &[words(0)]);
    assert!(lag.behind() > LAG_SECONDS);
}

/// A pass that's due without new audio waits for audio, rather than look
/// again at once.
#[test]
fn a_due_pass_without_audio_doesnt_spin() {
    let now = Instant::now();
    assert_eq!(wait_for(None, now), POLL);
    assert_eq!(wait_for(Some(now - Duration::from_secs(1)), now), POLL);
    assert_eq!(wait_for(Some(now), now), POLL);
    let soon = Duration::from_millis(30);
    assert_eq!(wait_for(Some(now + soon), now), soon);
    assert_eq!(wait_for(Some(now + Duration::from_secs(5)), now), POLL);
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

/// Words given to a dictation go to it, and nowhere once it's cancelled.
#[test]
fn words_go_to_their_dictation_while_it_lasts() {
    let (sender, received) = mpsc::channel();
    let intake = Arc::new(Intake::default());
    let words = || WordsSender {
        sender: sender.clone(),
        intake: intake.clone(),
    };
    let word = vec![SessionWord {
        text: "Telmo".into(),
        code: false,
    }];
    assert!(words().wanted());
    words().send(word.clone());
    assert!(matches!(received.try_recv(), Ok(Message::Words(given)) if given == word));
    intake.cancelled.store(true, Ordering::SeqCst);
    assert!(!words().wanted());
    words().send(word);
    assert!(received.try_recv().is_err());
}

#[test]
fn the_words_telmo_refuses_are_left_out() {
    let word = |text: &str, code| SessionWord {
        text: text.into(),
        code,
    };
    let list = special_words(
        &SpecialWords::new(),
        &[
            word("DictationOptions", true),
            // Two letters or digits are kept in code only.
            word("r5", true),
            word("ab", false),
            word("Sol", false),
            word("Sol", false),
            word("日本語", true),
            word("one two three four five six", false),
            word("Cargo.toml", true),
        ],
    );
    assert_eq!(
        list.words().collect::<Vec<_>>(),
        ["DictationOptions", "r5", "Sol", "Cargo.toml"]
    );
}

/// Telmo takes each of Arko's own names whole, and the merged list is
/// within its size.
#[test]
fn telmo_accepts_the_own_names() {
    let words = super::super::session_words::merge(Vec::new(), Vec::new());
    let list = special_words(&SpecialWords::new(), &words);
    assert_eq!(list.len(), words.len());
    assert!(list.words().any(|word| word == "Mesa Hills Research"));
    const { assert!(super::super::session_words::MAX_WORDS <= SpecialWords::MAX_WORDS) };
}

/// Telmo's cleanup is for English, so the settings' level applies to
/// English and nothing to another language.
#[test]
fn cleanup_is_for_english() {
    let settings = |text: &str| -> Settings { serde_json::from_str(text).unwrap() };
    let fillers = |language: &str| {
        cleanup(&settings(&format!(
            r#"{{"cleanup": "light", "language": "{language}"}}"#
        )))
    };
    assert_eq!(fillers("en"), Cleanup::Light);
    assert_eq!(fillers("en_GB"), Cleanup::Light);
    assert_eq!(fillers("fr"), Cleanup::Off);
    assert_eq!(fillers("pt-BR"), Cleanup::Off);
    assert_eq!(cleanup(&settings(r#"{"language": "en"}"#)), Cleanup::Off);
}

/// Each level's list leaves out what it takes out, and keeps doing so once
/// the session's words are added to it.
#[test]
fn the_list_leaves_out_what_the_cleanup_takes_out() {
    assert_eq!(cleaned(Cleanup::Off, None), SpecialWords::new());
    let base = cleaned(Cleanup::Light, None);
    assert_eq!(base.fillers(), Fillers::for_language("en"));
    let word = SessionWord {
        text: "Telmo".into(),
        code: false,
    };
    let list = special_words(&base, &[word]);
    assert_eq!(list.fillers(), Fillers::for_language("en"));
    assert_eq!(list.words().collect::<Vec<_>>(), ["Telmo"]);
    // Medium's tagger is built in.
    assert!(cleaned(Cleanup::Medium, None).disfluencies().is_some());
    // Every level keeps what the guards protect, and tidies the
    // punctuation.
    let punctuation = Punctuation::for_language("en").map(Punctuation::joining_more);
    for level in [Cleanup::Light, Cleanup::Medium, Cleanup::Full] {
        assert_eq!(cleaned(level, None).guards(), Guards::for_language("en"));
        assert_eq!(cleaned(level, None).punctuation(), punctuation);
    }
}

/// Full cleanup's missing tagger is asked about with the speech model, and
/// downloads first. A tagger that can't load fails before the speech model
/// downloads.
#[test]
fn full_cleanup_asks_for_its_tagger_and_fails_first_without_it() {
    let root = root_with(r#"{"model": "granite", "cleanup": "full", "language": "en"}"#);
    let service = DictationService::downloading_from(root.path(), NO_SERVER);
    let missing = Download {
        speech: Some(&GRANITE),
        cleanup: Some(download::TAGGER.bytes),
    };
    assert_eq!(service.missing(), Some(missing));
    let Err(error) = service.open(DictationOptions::default(), sink().0) else {
        panic!("opened without the models");
    };
    assert_eq!(error, DictationError::NeedsDownload(missing));
    // Another language needs no tagger.
    let french = root_with(r#"{"model": "granite", "cleanup": "full", "language": "fr"}"#);
    let speech_only = DictationService::downloading_from(french.path(), NO_SERVER).missing();
    assert_eq!(speech_only.map(|download| download.cleanup), Some(None));
    let failed = || {
        let (sink, updates) = sink();
        let _dictation = service
            .open(DictationOptions { download: true }, sink)
            .unwrap();
        updates.recv_timeout(Duration::from_secs(10)).unwrap().event
    };
    assert_eq!(
        failed(),
        DictationEvent::Failed("Couldn't download the cleanup model (couldn't reach GitHub). Start dictation again to resume.".into())
    );
    let dir = models::tagger_dir(root.path());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(PACK_FILE), "not a pack").unwrap();
    let DictationEvent::Failed(why) = failed() else {
        panic!("a tagger that can't load was used");
    };
    assert!(
        why.starts_with("Couldn't load the cleanup model ("),
        "{why}"
    );
}

/// Arko's folder with `dictation.json` holding `settings` and Granite
/// Speech's real pack, and the clip to dictate, when `ARKO_TEST_SPEECH_PACK`
/// names Granite's int8 pack and `ARKO_TEST_SPEECH_WAV` LibriSpeech's clip
/// 260-123286-0018 (Telmo's tests have both).
fn granite_and_clip(settings: &str) -> Option<(tempfile::TempDir, Source)> {
    let (Some(pack), Some(wav)) = (
        std::env::var_os("ARKO_TEST_SPEECH_PACK"),
        std::env::var_os("ARKO_TEST_SPEECH_WAV"),
    ) else {
        eprintln!("skipped: ARKO_TEST_SPEECH_PACK and ARKO_TEST_SPEECH_WAV aren't set");
        return None;
    };
    let root = root_with(settings);
    let dir = GRANITE.dir(root.path());
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&pack, GRANITE.pack(root.path())).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(&pack, GRANITE.pack(root.path())).unwrap();
    Some((root, Source::File(wav.into())))
}

/// The words settled when the clip is dictated through `service`, given
/// `words` to favour as a front end gives them, once the dictation opened.
fn dictated(service: &DictationService, source: &Source, words: &[SessionWord]) -> String {
    dictated_pieces(service, source, words).concat()
}

/// The words settled as [`dictated`] gives them, each event's on its own.
fn dictated_pieces(
    service: &DictationService,
    source: &Source,
    words: &[SessionWord],
) -> Vec<String> {
    let (sink, updates) = sink();
    let mut dictation = service.open(DictationOptions::default(), sink).unwrap();
    dictation.words_sender().send(words.to_vec());
    let next = || {
        updates
            .recv_timeout(Duration::from_secs(60))
            .expect("an event")
            .event
    };
    loop {
        match next() {
            DictationEvent::Preparing(_) => {}
            DictationEvent::Listening => break,
            other => panic!("{other:?} before listening"),
        }
    }
    let capture = dictation.record(source).unwrap();
    let mut pieces: Vec<String> = Vec::new();
    // The clip is 5.7 s, then silence ends the utterance.
    while !pieces.concat().contains("length") {
        match next() {
            DictationEvent::Text { settled, .. } if settled.is_empty() => {}
            DictationEvent::Text { settled, .. } => pieces.push(settled),
            other => panic!("{other:?} while listening"),
        }
    }
    drop(capture);
    dictation.finish();
    loop {
        match next() {
            DictationEvent::Text { settled, .. } if settled.is_empty() => {}
            DictationEvent::Text { settled, .. } => pieces.push(settled),
            DictationEvent::Done => return pieces,
            other => panic!("{other:?} while finishing"),
        }
    }
}

/// The clip dictated through `service`, whose one utterance settles whole
/// when it ends, with the session's words kept.
fn settles_whole(service: &DictationService, source: &Source) {
    let words = [SessionWord {
        text: "Hamburgh".into(),
        code: false,
    }];
    let pieces = dictated_pieces(service, source, &words);
    let first = pieces[0].to_lowercase();
    assert!(
        first.contains("i saw") && first.contains("thirty feet in length"),
        "{pieces:?}"
    );
    assert!(first.contains("hamburgh"), "{pieces:?}");
}

/// With medium cleanup, which needs no pack of its own, an utterance
/// settles whole when it ends. It runs with the clip and the pack
/// ([`granite_and_clip`]).
#[test]
fn medium_cleanup_settles_an_utterance_once_it_ends() {
    let settings = r#"{"model": "granite", "cleanup": "medium", "language": "en"}"#;
    let Some((root, source)) = granite_and_clip(settings) else {
        return;
    };
    settles_whole(
        &DictationService::downloading_from(root.path(), NO_SERVER),
        &source,
    );
}

/// With full cleanup, an utterance settles whole when it ends. It runs only
/// with the pack and the clip ([`granite_and_clip`]) and
/// `ARKO_TEST_CLEANUP_PACK` naming the tagger's pack.
#[test]
fn full_cleanup_settles_an_utterance_once_it_ends() {
    let settings = r#"{"model": "granite", "cleanup": "full", "language": "en"}"#;
    let Some(tagger) = std::env::var_os("ARKO_TEST_CLEANUP_PACK") else {
        eprintln!("skipped: ARKO_TEST_CLEANUP_PACK isn't set");
        return;
    };
    let Some((root, source)) = granite_and_clip(settings) else {
        return;
    };
    let dir = models::tagger_dir(root.path());
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&tagger, dir.join(PACK_FILE)).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(&tagger, dir.join(PACK_FILE)).unwrap();
    settles_whole(
        &DictationService::downloading_from(root.path(), NO_SERVER),
        &source,
    );
}

/// Full cleanup's tagger leaves memory when a recording uses another level,
/// and loads again for the next with full cleanup. It runs as
/// [`full_cleanup_settles_an_utterance_once_it_ends`] does.
#[test]
fn the_tagger_loads_again_after_another_level() {
    let full = r#"{"model": "granite", "cleanup": "full", "language": "en"}"#;
    let Some(tagger) = std::env::var_os("ARKO_TEST_CLEANUP_PACK") else {
        eprintln!("skipped: ARKO_TEST_CLEANUP_PACK isn't set");
        return;
    };
    let Some((root, source)) = granite_and_clip(full) else {
        return;
    };
    let pack = models::tagger_pack(root.path());
    std::fs::create_dir_all(pack.parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&tagger, &pack).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(&tagger, &pack).unwrap();
    let service = DictationService::downloading_from(root.path(), NO_SERVER);
    let settings = root.path().join(super::super::settings::FILE);
    settles_whole(&service, &source);
    std::fs::write(&settings, r#"{"model": "granite", "cleanup": "off"}"#).unwrap();
    let text = dictated(&service, &source, &[]).to_lowercase();
    assert!(text.contains("thirty feet in length"), "{text:?}");
    std::fs::write(&settings, full).unwrap();
    settles_whole(&service, &source);
}

/// The session's words reach the streamer. Given the clip's "Hamburg" in
/// another spelling, the words come out in it, also while fillers are left
/// out, and not once the settings turn session words off, which the service
/// checks whatever a front end sends. It runs with the clip and the pack, as
/// [`a_wav_file_is_dictated_end_to_end`] does.
#[test]
fn the_session_words_reach_the_streamer() {
    let Some((root, source)) = granite_and_clip(r#"{"model": "granite"}"#) else {
        return;
    };
    let service = DictationService::downloading_from(root.path(), NO_SERVER);
    let words = [SessionWord {
        text: "Hamburgh".into(),
        code: false,
    }];
    let text = dictated(&service, &source, &words);
    assert!(text.contains("Hamburgh"), "{text:?}");
    // Leaving out fillers keeps the words.
    std::fs::write(
        root.path().join(super::super::settings::FILE),
        r#"{"model": "granite", "cleanup": "light"}"#,
    )
    .unwrap();
    let text = dictated(&service, &source, &words);
    assert!(text.contains("Hamburgh"), "{text:?}");
    std::fs::write(
        root.path().join(super::super::settings::FILE),
        r#"{"model": "granite", "session_words": false}"#,
    )
    .unwrap();
    let text = dictated(&service, &source, &words);
    assert!(
        text.contains("Hamburg") && !text.contains("Hamburgh"),
        "{text:?}"
    );
}

/// A recording from a WAV file, through the service and Granite Speech's
/// real pack. The model loads, the words come and settle, and `finish`
/// ends it. A second recording finds the model loaded. It runs only with
/// the pack and the clip ([`granite_and_clip`]).
#[test]
fn a_wav_file_is_dictated_end_to_end() {
    let Some((root, source)) = granite_and_clip(r#"{"model": "granite"}"#) else {
        return;
    };
    let service = DictationService::downloading_from(root.path(), NO_SERVER);

    let record = |expect_loading: bool| {
        let (sink, updates) = sink();
        let mut dictation = service.open(DictationOptions::default(), sink).unwrap();
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

    // Each of these ends the dictation, and nothing follows.
    let listening = || {
        let (sink, updates) = sink();
        let dictation = service.open(DictationOptions::default(), sink).unwrap();
        let first = updates.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_eq!(first.event, DictationEvent::Listening);
        (dictation, updates)
    };
    let quiet = |updates: &Receiver<DictationUpdate>| {
        let after = updates.recv_timeout(Duration::from_secs(3));
        assert!(after.is_err(), "{after:?}");
    };
    // Cancelled while the last words come, so no Done.
    let (mut dictation, updates) = listening();
    let capture = dictation.record(&source).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    drop(capture);
    dictation.finish();
    dictation.cancel();
    let texts: Vec<DictationEvent> = updates.try_iter().map(|update| update.event).collect();
    assert!(!texts.contains(&DictationEvent::Done), "{texts:?}");
    quiet(&updates);
    // More audio at once than the computer could keep up with.
    let (dictation, updates) = listening();
    dictation.push(&vec![0.0; 16_000 * (BACKLOG_SECONDS + 1)], 16_000);
    let failed = updates.recv_timeout(Duration::from_secs(10)).unwrap().event;
    assert_eq!(failed, DictationEvent::Failed(OVERLOADED.into()));
    quiet(&updates);
    drop(dictation);
    // A microphone that stops.
    let (dictation, updates) = listening();
    dictation
        .sender
        .send(Message::CaptureFailed("the device is gone".into()))
        .unwrap();
    let failed = updates.recv_timeout(Duration::from_secs(10)).unwrap().event;
    assert_eq!(
        failed,
        DictationEvent::Failed("The microphone stopped (the device is gone).".into())
    );
    quiet(&updates);
}

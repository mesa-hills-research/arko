//! The view's dictation flows, with the service's events given by hand.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::dictation::models::GRANITE;
use crate::tui::app::tests::app;
use crate::ui::Input;

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.key(KeyEvent::new(code, modifiers));
}

fn key(app: &mut App, code: KeyCode) {
    press(app, code, KeyModifiers::NONE);
}

fn ctrl_r(app: &mut App) {
    press(app, KeyCode::Char('r'), KeyModifiers::CONTROL);
}

fn event(app: &mut App, id: u64, event: DictationEvent) {
    app.dictation_event(DictationUpdate { id, event });
}

fn text(settled: &str, tentative: &str) -> DictationEvent {
    DictationEvent::Text {
        settled: settled.into(),
        tentative: tentative.into(),
    }
}

fn dictates(app: &mut App) -> Vec<Dictate> {
    app.take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Dictate(dictate) => Some(dictate),
            _ => None,
        })
        .collect()
}

/// The screen's rows, drawn `width` columns wide.
fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| crate::tui::screen::draw(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// An app recording as dictation 7, listening, with `typed` in the box
/// before it started.
fn listening(typed: &str) -> App {
    let mut app = app();
    app.editor.set(typed);
    ctrl_r(&mut app);
    assert_eq!(dictates(&mut app), [Dictate::Open { download: false }]);
    app.dictation_opened(7);
    event(&mut app, 7, DictationEvent::Preparing(Preparing::Loading));
    event(&mut app, 7, DictationEvent::Listening);
    assert_eq!(dictates(&mut app), [Dictate::Record]);
    app
}

#[test]
fn words_go_in_at_the_cursor_and_tentative_ones_show_dimmed_after_it() {
    let mut app = listening("Fix this:");
    event(&mut app, 7, text("", "Hello wor"));
    assert_eq!(app.editor.text(), "Fix this:");
    let rows = screen(&mut app, 60, 12);
    assert!(
        rows.iter()
            .any(|row| row.ends_with("› Fix this: Hello wor")),
        "{rows:#?}"
    );
    // The tentative words are dimmed, the typed ones not.
    let spans = app
        .editor
        .lines(40, app.theme.text(), app.theme.key(), app.theme.dim());
    assert_eq!(spans[0][0].content, "Fix this:");
    assert_eq!(spans[0][1].content, " Hello wor");
    assert_eq!(spans[0][1].style, app.theme.dim());

    event(&mut app, 7, text("Hello", " world"));
    assert_eq!(app.editor.text(), "Fix this: Hello");
    assert_eq!(app.editor.cursor(), "Fix this: Hello".len());
    // Another recording's events aren't the view's.
    event(&mut app, 8, text("Nope", ""));
    assert_eq!(app.editor.text(), "Fix this: Hello");
}

#[test]
fn the_status_line_says_listening_with_the_keys_that_stop_and_cancel() {
    let mut app = app();
    ctrl_r(&mut app);
    app.dictation_opened(1);
    event(&mut app, 1, DictationEvent::Preparing(Preparing::Loading));
    let status = |app: &mut App| screen(app, 80, 8)[4].clone();
    assert!(
        status(&mut app).contains("loading speech model"),
        "{}",
        status(&mut app)
    );
    event(&mut app, 1, DictationEvent::Listening);
    let row = status(&mut app);
    assert!(row.starts_with(" listening "), "{row}");
    assert!(row.ends_with("Enter  stop   Esc  cancel"), "{row}");
}

#[test]
fn enter_stops_the_words_stay_to_edit_and_enter_again_sends() {
    let mut app = listening("");
    event(&mut app, 7, text("Hello", " world"));
    key(&mut app, KeyCode::Enter);
    assert_eq!(dictates(&mut app), [Dictate::Finish]);
    // While the last words come, Enter waits for them.
    key(&mut app, KeyCode::Enter);
    assert!(app.take_effects().is_empty());
    event(&mut app, 7, text(" world.", ""));
    event(&mut app, 7, DictationEvent::Done);
    assert_eq!(dictates(&mut app), [Dictate::Close]);
    assert!(app.recording.is_none() && !app.editor.has_tentative());
    assert_eq!(app.editor.text(), "Hello world.");
    key(&mut app, KeyCode::Backspace);
    key(&mut app, KeyCode::Char('!'));
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        app.take_effects(),
        [Effect::Input(Input::Line("Hello world!".into()))]
    );
}

#[test]
fn ctrl_r_stops_as_enter_does() {
    let mut app = listening("");
    ctrl_r(&mut app);
    assert_eq!(dictates(&mut app), [Dictate::Finish]);
    // Again while the last words come: nothing.
    ctrl_r(&mut app);
    assert!(app.take_effects().is_empty());
}

#[test]
fn esc_cancels_and_takes_the_recordings_words_out() {
    let mut app = listening("Note: ");
    event(&mut app, 7, text("one two", " three"));
    assert_eq!(app.editor.text(), "Note: one two");
    key(&mut app, KeyCode::Esc);
    assert_eq!(dictates(&mut app), [Dictate::Cancel]);
    assert_eq!(app.editor.text(), "Note: ");
    assert!(app.recording.is_none() && !app.editor.has_tentative());
    // Later events of the cancelled recording change nothing.
    event(&mut app, 7, text(" four", ""));
    assert_eq!(app.editor.text(), "Note: ");
}

/// Esc takes out every word the recording put in, wherever the cursor took
/// them, and none of the user's, even typed alike.
#[test]
fn esc_takes_out_only_the_recordings_words() {
    let mut app = listening("");
    event(&mut app, 7, text("alpha", ""));
    key(&mut app, KeyCode::Home);
    key(&mut app, KeyCode::Char('X'));
    event(&mut app, 7, text(" beta", ""));
    assert_eq!(app.editor.text(), "X betaalpha");
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.editor.text(), "X");

    let mut app = listening("");
    event(&mut app, 7, text("cat", ""));
    key(&mut app, KeyCode::Home);
    for c in "cat dog ".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.editor.text(), "cat dog ");
}

/// Once the recording is done, its words are the user's: a later Esc
/// leaves them.
#[test]
fn a_finished_recordings_words_stay() {
    let mut app = listening("");
    event(&mut app, 7, text("kept", ""));
    key(&mut app, KeyCode::Enter);
    event(&mut app, 7, DictationEvent::Done);
    ctrl_r(&mut app);
    app.dictation_opened(8);
    event(&mut app, 8, DictationEvent::Listening);
    event(&mut app, 8, text(" more", ""));
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.editor.text(), "kept");
}

#[test]
fn a_missing_model_asks_before_it_downloads() {
    let mut app = app();
    ctrl_r(&mut app);
    dictates(&mut app);
    app.dictation_refused(DictationError::NeedsDownload(&GRANITE));
    assert!(app.recording.is_none());
    let rows = screen(&mut app, 80, 24).join("\n");
    assert!(rows.contains("Download a speech model?"), "{rows}");
    assert!(rows.contains("with Granite Speech 5.0"), "{rows}");
    assert!(
        rows.contains("by IBM (948 MB download, 477 MB on disk, Apache 2.0)."),
        "{rows}"
    );
    assert!(rows.contains(" Download    Cancel "), "{rows}");
    // Cancel closes it, with nothing asked of the service.
    key(&mut app, KeyCode::Right);
    key(&mut app, KeyCode::Enter);
    assert!(app.overlay.is_none() && app.take_effects().is_empty());
    // Download starts the recording again, with the download.
    app.dictation_refused(DictationError::NeedsDownload(&GRANITE));
    key(&mut app, KeyCode::Enter);
    assert!(app.overlay.is_none());
    assert_eq!(dictates(&mut app), [Dictate::Open { download: true }]);
    assert_eq!(
        app.recording.as_ref().map(|r| r.phase),
        Some(Phase::Starting)
    );
}

/// A download takes minutes, so the box works as usual meanwhile, and
/// Ctrl+R cancels it.
#[test]
fn a_download_runs_beside_the_box() {
    let mut app = app();
    ctrl_r(&mut app);
    app.dictation_opened(3);
    dictates(&mut app);
    let step = Preparing::Downloading {
        done: 412_000_000,
        total: 948_011_281,
    };
    event(&mut app, 3, DictationEvent::Preparing(step));
    let status = screen(&mut app, 100, 8)[4].clone();
    assert!(
        status.contains("downloading speech model (412 of 948 MB)"),
        "{status}"
    );
    assert!(status.ends_with("Ctrl+R  cancel"), "{status}");
    for c in "hi".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        app.take_effects(),
        [Effect::Input(Input::Line("hi".into()))]
    );
    ctrl_r(&mut app);
    assert_eq!(dictates(&mut app), [Dictate::Cancel]);
    assert!(app.recording.is_none());
}

/// Once a download is done, minutes after it was asked for, the microphone
/// waits for Ctrl+R.
#[test]
fn a_finished_download_waits_for_ctrl_r() {
    let mut app = app();
    ctrl_r(&mut app);
    app.dictation_opened(4);
    dictates(&mut app);
    let step = Preparing::Downloading {
        done: 0,
        total: 948_011_281,
    };
    for event_ in [
        DictationEvent::Preparing(step),
        DictationEvent::Preparing(Preparing::Packing),
        DictationEvent::Preparing(Preparing::Loading),
        DictationEvent::Listening,
    ] {
        event(&mut app, 4, event_);
    }
    assert_eq!(dictates(&mut app), [Dictate::Cancel]);
    assert!(app.recording.is_none());
    assert_eq!(app.hint(), Some(MODEL_READY));
}

/// Failures are notices in the transcript, which outlast the status line,
/// and end the recording.
#[test]
fn failures_are_noticed_and_end_the_recording() {
    use crate::remote::Level;
    let mut app = app();
    ctrl_r(&mut app);
    dictates(&mut app);
    app.dictation_refused(DictationError::Busy);
    assert_eq!(
        app.take_effects(),
        [Effect::Notice(
            Level::Warn,
            "Dictation is already running in another window.".into()
        )]
    );
    assert!(app.recording.is_none());

    let mut app = listening("");
    event(&mut app, 7, text("", "maybe"));
    event(
        &mut app,
        7,
        DictationEvent::Failed("Speech recognition failed (no).".into()),
    );
    assert_eq!(
        app.take_effects(),
        [
            Effect::Notice(Level::Error, "Speech recognition failed (no).".into()),
            Effect::Dictate(Dictate::Close)
        ]
    );
    assert!(app.recording.is_none() && !app.editor.has_tentative());

    let mut app = listening("");
    app.dictation_failed("No microphone found.".into());
    assert_eq!(
        app.take_effects(),
        [Effect::Notice(Level::Error, "No microphone found.".into())]
    );
    assert!(app.recording.is_none());
}

#[test]
fn the_dictate_command_toggles_recording() {
    let mut app = app();
    app.editor.set("/dictate");
    key(&mut app, KeyCode::Enter);
    assert_eq!(dictates(&mut app), [Dictate::Open { download: false }]);
    assert!(app.editor.is_empty());
    app.dictation_opened(2);
    event(&mut app, 2, DictationEvent::Listening);
    dictates(&mut app);
    event(&mut app, 2, text("Hello", ""));
    // (Typed after the words, it would be part of the message.)
    ctrl_r(&mut app);
    assert_eq!(dictates(&mut app), [Dictate::Finish]);
}

#[test]
fn the_first_words_get_a_space_after_a_word() {
    assert_eq!(spaced(Some(':'), false, "Hello"), " Hello");
    assert_eq!(spaced(Some(' '), false, "Hello"), "Hello");
    assert_eq!(spaced(None, false, "Hello"), "Hello");
    assert_eq!(spaced(Some('o'), true, ", then"), ", then");
    assert_eq!(spaced(Some('o'), false, ""), "");
    assert_eq!(lowercase("Loading speech model"), "loading speech model");
}

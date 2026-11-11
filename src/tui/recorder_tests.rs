//! Frame sequences of the full-screen view for measuring a renderer.
//! `record_frames` checks nothing. It plays five scenes against the view and
//! writes every frame of each, in the format of [`golden`](super::golden),
//! one frame after another, to `SCENE.styled` in the folder
//! `ARKO_FRAME_RECORDINGS` names.
//!
//! ```bash
//! mkdir -p /tmp/frames
//! ARKO_FRAME_RECORDINGS=/tmp/frames cargo test --lib tui::recorder -- --ignored
//! ```
//!
//! | Scene | Size | Frames | What happens |
//! |---|---|---|---|
//! | `idle` | 200 by 60 | 100 | A turn under way. The spinner and the highlight of "Working…" move, and the clock goes on 100 ms a frame |
//! | `streaming` | 120 by 40 | 300 | A reply grows by a few words a frame and the view follows the bottom of a long transcript |
//! | `scrolling` | 120 by 40 | 100 | Page Up from the bottom of a long transcript |
//! | `typing` | 120 by 40 | 100 | One character a frame into the message box |
//! | `redraw` | 300 by 100 | 6 | The views Full, Quiet and Messages twice over, which changes nearly every cell |
//!
//! The clocks stand still between frames and the working time counts the
//! recording's own time, so a second recording of a scene is the same text.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::app::App;
use super::golden;
use super::inventory_tests::{
    Frozen, Items, Look, act, call, command, content, figures, fresh, key, now_ms, user,
};
use super::motion;
use super::screen;
use super::transcript::View;
use crate::glyphs::{Symbols, tests::choose_symbols};
use crate::remote::{ActKind, Activity, CallStatus, ContentKind, Item, Kind, Level, Update};

/// A terminal that keeps its screen between frames, as the view's does, and
/// the frames drawn on it.
struct Recording {
    terminal: Terminal<TestBackend>,
    text: String,
}

impl Recording {
    fn new(width: u16, height: u16) -> Self {
        Self {
            terminal: Terminal::new(TestBackend::new(width, height)).unwrap(),
            text: String::new(),
        }
    }

    /// Draws a frame at `ms` on the animations' clock and keeps it.
    fn frame(&mut self, app: &mut App, ms: u64) {
        motion::CHOSEN.set(Some(ms));
        self.terminal
            .draw(|frame| screen::draw(frame, app))
            .unwrap();
        self.text.push_str(&golden::of(self.terminal.backend()));
    }

    fn save(self, dir: &std::path::Path, scene: &str, frames: usize) {
        assert_eq!(self.text.matches("\nsize ").count() + 1, frames);
        std::fs::write(dir.join(format!("{scene}.styled")), self.text).unwrap();
    }
}

/// `turns` finished turns of the shape the animation benchmark uses, each
/// with thinking, text, a command with output, a file read, a notice, a
/// reply and the turn's line. Their times are long past.
fn turns(items: &mut Items, count: u64) {
    let lines = |count: usize| vec!["test parser::tests::reads ... ok"; count].join("\n");
    for turn in 0..count {
        items.turn = turn + 1;
        items.add(user(&format!("Turn {turn}: fix the parser's tests")));
        items.add(Item::new(
            Kind::Thinking,
            "**Planning**\nRead the tests first.\nThen the parser.",
        ));
        items.add(Item::new(
            Kind::Text,
            "Reading the **parser** first, then `cargo test`:\n\n```rust\nfn main() {}\n```",
        ));
        let test = items.add(command(
            "$ cargo test --lib parser",
            CallStatus::Ok,
            Some("exit 0 · 12 passed"),
            "12 passed",
        ));
        let mut output = content(test, ContentKind::Plain, &lines(5));
        output.more = Some("… 35 more lines".into());
        output.whole = Some(lines(40).into());
        items.add(output);
        items.add(call(
            "read src/parser.rs",
            CallStatus::Ok,
            None,
            act(ActKind::Read, "src/parser.rs"),
        ));
        items.add(Item::new(Kind::Notice, "retrying in 2 s").level(Level::Warn));
        items.add(Item::new(
            Kind::Text,
            "The parser skipped blank lines at the end, so the last test failed.",
        ));
        items.add(Item::new(
            Kind::Reply,
            "Fixed.\n\n- The parser keeps blank lines.\n- All **12** tests pass.",
        ));
        items.add(Item::new(Kind::Turn, "3 s · $0.010"));
    }
}

/// An app with 300 finished turns, and then the user's next message and a
/// file read, so the agent has work to do. Returns when the turn began.
fn long_transcript(app: &mut App, with_next_turn: bool) -> Option<i64> {
    let now = now_ms();
    let mut items = Items::new(now);
    turns(&mut items, 300);
    if with_next_turn {
        items.turn = 301;
        items.add(user("And the docs?"));
        items.add(call(
            "read README.md",
            CallStatus::Ok,
            None,
            act(ActKind::Read, "README.md"),
        ));
    }
    // Times in order and long ago, so nothing on the screen counts from now.
    for item in &mut items.list {
        item.at = now - 3_000_000 + 100 * item.id as i64;
    }
    let since = items
        .list
        .iter()
        .rev()
        .find(|item| item.kind == Kind::User)
        .map(|item| item.at);
    items.play(app);
    app.set_view(View::Full);
    figures(app);
    since
}

/// The agent works, since a minute and `ms` milliseconds before this frame.
fn working_for(app: &mut App, since: i64, ms: u64) {
    app.set_activity(Activity {
        reporting: false,
        working: true,
        since: Some(since),
        question: None,
    });
    app.working_since = Some(Instant::now() - Duration::from_millis(60_300 + ms));
}

fn idle(dir: &std::path::Path) {
    let mut app = fresh(Look::Dark);
    let since = long_transcript(&mut app, true).unwrap();
    let mut recording = Recording::new(200, 60);
    for n in 0..100 {
        working_for(&mut app, since, 100 * n);
        recording.frame(&mut app, 100 * n);
    }
    recording.save(dir, "idle", 100);
}

fn streaming(dir: &std::path::Path) {
    const WORDS: &str = "The parser reads its input one line at a time and splits each line into tokens. \
        Quoted words stay whole, so **a quoted phrase** is one token, and `parse` hands them to the \
        lexer in order. A blank line ends a paragraph, which is why the blank line at the end of a \
        file matters: without it the last paragraph never closes.\n\n\
        The fix is in the loop. It stops at the end of the input, not at the first newline, and it \
        counts the blank lines it passes.";
    let mut app = fresh(Look::Dark);
    let mut items = Items::new(now_ms());
    turns(&mut items, 300);
    items.turn = 301;
    items.add(user("How does the parser work?"));
    let words: Vec<&str> = WORDS.split(' ').cycle().take(4 * 300 + 4).collect();
    let mut item = Item::new(Kind::Text, words[..4].join(" "));
    item.at = 0;
    let id = items.add(item);
    let now = now_ms();
    for item in &mut items.list {
        item.at = now - 3_000_000 + 100 * item.id as i64;
    }
    let since = items.list[items.list.len() - 2].at;
    items.play(&mut app);
    figures(&mut app);
    let mut recording = Recording::new(120, 40);
    for n in 0..300u64 {
        if n > 0 {
            let at = 4 * n as usize;
            app.update(
                Update::Append {
                    id,
                    text: format!(" {}", words[at..at + 4].join(" ")),
                },
                Activity::default,
            );
        }
        // A frame every 33 ms, as the view draws while text streams.
        working_for(&mut app, since, 33 * n);
        recording.frame(&mut app, 33 * n);
    }
    recording.save(dir, "streaming", 300);
}

fn scrolling(dir: &std::path::Path) {
    let mut app = fresh(Look::Dark);
    long_transcript(&mut app, false);
    app.last_turn = Some(Duration::from_secs(3));
    let mut recording = Recording::new(120, 40);
    for n in 0..100u64 {
        if n > 0 {
            key(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        }
        recording.frame(&mut app, 100 * n);
    }
    recording.save(dir, "scrolling", 100);
}

fn typing(dir: &std::path::Path) {
    const TEXT: &str = "Please fix the parser so that blank lines at the end of a file are kept, then run the whole test suite.";
    let mut app = fresh(Look::Dark);
    long_transcript(&mut app, false);
    app.last_turn = Some(Duration::from_secs(3));
    let mut recording = Recording::new(120, 40);
    for (n, c) in TEXT.chars().take(100).enumerate() {
        key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        recording.frame(&mut app, 100 * n as u64);
    }
    recording.save(dir, "typing", 100);
}

fn redraw(dir: &std::path::Path) {
    let mut app = fresh(Look::Dark);
    long_transcript(&mut app, false);
    app.last_turn = Some(Duration::from_secs(3));
    let mut recording = Recording::new(300, 100);
    for (n, view) in [View::Full, View::Quiet, View::Messages]
        .into_iter()
        .cycle()
        .take(6)
        .enumerate()
    {
        app.set_view(view);
        recording.frame(&mut app, 100 * n as u64);
    }
    recording.save(dir, "redraw", 6);
}

/// Not a test. Writes the frame recordings the folder `ARKO_FRAME_RECORDINGS`
/// names.
#[test]
#[ignore]
fn record_frames() {
    let Some(dir) = std::env::var_os("ARKO_FRAME_RECORDINGS") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _symbols = choose_symbols(Symbols::Full);
    let _frozen = Frozen::new();
    idle(&dir);
    streaming(&dir);
    scrolling(&dir);
    typing(&dir);
    redraw(&dir);
}

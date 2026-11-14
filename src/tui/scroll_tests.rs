//! The transcript holds still on termelon's emulator through a scripted
//! agent session. Scrolled up, the text at the top stays where it is as
//! replies stream, results and thinking open elsewhere, "Working…" comes and
//! goes and the width changes. Following, no row moves down when the end
//! gets shorter, by any number of rows, and a scroll down at the end moves
//! no row either. The screens are the emulator's, so they are what a
//! terminal shows after the bytes Arko wrote, scrolled regions and all.

use termelon::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use termelon::testing::TestTerminal;

use crate::glyphs::{Symbols, tests::choose_symbols};
use crate::remote::{
    Act, ActKind, Activity, CallStatus, ContentKind, Delivery, Item, Kind, Update,
};
use crate::tui::app::{App, Info};
use crate::tui::input::{Editor, History};
use crate::tui::screen::draw;
use crate::tui::theme::{Catalog, Depth};
use crate::tui::transcript::View;

/// Numbers from a seed, so a session plays the same each time.
pub(super) struct Rng(pub(super) u64);

impl Rng {
    pub(super) fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n.max(1)
    }
}

/// What a step of the session did, for the messages of a failing check.
pub(super) type Each<'a> = dyn FnMut(&mut App, &str) + 'a;

/// An agent's session, played into an app a change at a time. Every word of
/// its text is a word no other has, so each line on the screen can be found
/// again in the next frame.
pub(super) struct Session {
    pub(super) app: App,
    pub(super) rng: Rng,
    id: u64,
    at: i64,
    word: u64,
    /// A command that runs on while turns go by, and its words.
    pub(super) long: Option<(u64, String)>,
}

impl Session {
    pub(super) fn new(seed: u64, view: View) -> Self {
        let mut app = App::new(
            crate::remote::Snapshot::default(),
            Editor::new(History::load(None)),
            Info::default(),
            Catalog::load(None),
            Depth::Rgb,
        );
        app.view = view;
        Self {
            app,
            rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
            id: 0,
            at: chrono::Utc::now().timestamp_millis(),
            word: 0,
            long: None,
        }
    }

    /// `n` words no other text has.
    pub(super) fn words(&mut self, n: u64) -> String {
        let words: Vec<String> = (0..n)
            .map(|_| {
                self.word += 1;
                format!("w{}", self.word)
            })
            .collect();
        words.join(" ")
    }

    pub(super) fn item(&mut self, kind: Kind, text: &str) -> Item {
        self.id += 1;
        self.at += 10;
        let mut item = Item::new(kind, text);
        item.id = self.id;
        item.at = self.at;
        item.turn = Some(1);
        item
    }

    pub(super) fn add(&mut self, item: Item) -> u64 {
        let id = item.id;
        self.app.update(Update::Item(item), Activity::default);
        id
    }

    pub(super) fn append(&mut self, id: u64, text: &str) {
        let text = text.to_string();
        self.app
            .update(Update::Append { id, text }, Activity::default);
    }

    pub(super) fn works(&mut self, working: bool) {
        self.app.set_activity(Activity {
            reporting: false,
            working,
            since: working.then_some(self.at),
            question: None,
        });
    }

    /// A command, running.
    pub(super) fn command(&mut self, words: &str) -> Item {
        let mut call = self.item(Kind::Call, &format!("$ cargo test {words}"));
        call.status = Some(CallStatus::Running);
        call.act = Some(Act {
            kind: ActKind::Command,
            target: format!("cargo test {words}"),
            alone: true,
            ..Act::default()
        });
        call
    }

    /// The command `call` ends, `lines` long, with its content.
    pub(super) fn ends(&mut self, call: u64, words: &str, lines: u64) {
        let text: Vec<String> = (0..lines.max(1))
            .map(|line| format!("test {words} {line} ... ok"))
            .collect();
        let mut result = self.item(Kind::Result, &text.join("\n"));
        result.call = Some(call);
        result.content = Some(ContentKind::Plain);
        self.add(result);
        let mut ended = self.command(words);
        ended.id = call;
        self.id -= 1;
        ended.status = Some(CallStatus::Ok);
        ended.outcome = Some("exit 0".into());
        self.app.update(Update::Change(ended), Activity::default);
    }

    /// A turn, from the user's message to its end, calling `each` after each
    /// change the screen could show.
    pub(super) fn turn(&mut self, each: &mut Each) {
        let mut user = self.item(Kind::User, "");
        let n = 4 + self.rng.below(10);
        user.text = format!("Please {}", self.words(n));
        user.delivery = Some(Delivery::Turn);
        self.add(user);
        self.works(true);
        each(&mut self.app, "the turn began, Working…");
        // A thought streams.
        let words = self.words(3);
        let thinking = self.item(Kind::Thinking, &words);
        let thinking = self.add(thinking);
        each(&mut self.app, "a thought began");
        for _ in 0..1 + self.rng.below(3) {
            let n = 2 + self.rng.below(6);
            let more = format!(" {}", self.words(n));
            self.append(thinking, &more);
            each(&mut self.app, "the thought grew");
        }
        // Calls, each with output.
        for _ in 0..1 + self.rng.below(3) {
            let words = self.words(2);
            let call = self.command(&words);
            let call = self.add(call);
            each(&mut self.app, "a command runs");
            let lines = 1 + self.rng.below(5);
            self.ends(call, &words, lines);
            each(&mut self.app, "the command ended, Working…");
        }
        // Once, a command that runs on.
        if self.long.is_none() {
            let words = self.words(2);
            let call = self.command(&words);
            let call = self.add(call);
            self.long = Some((call, words));
            each(&mut self.app, "a long command runs");
        }
        // The reply streams, a few words at a time, its paragraphs and bold
        // words coming in pieces.
        let text = self.item(Kind::Text, "");
        let text = self.add(text);
        let mut bold = false;
        for _ in 0..12 + self.rng.below(30) {
            let n = 1 + self.rng.below(6);
            let words = self.words(n);
            let more = match self.rng.below(10) {
                0 => format!(" {words}\n\n"),
                1 | 2 if bold => format!(" {words}**"),
                1 | 2 => format!(" **{words}"),
                _ => format!(" {words}"),
            };
            bold ^= more.contains("**");
            self.append(text, &more);
            each(&mut self.app, "the reply grew");
        }
        if bold {
            self.append(text, "**");
            each(&mut self.app, "the reply's bold closed");
        }
        let end = self.item(Kind::Turn, "12 s · $0.01");
        self.add(end);
        self.works(false);
        each(&mut self.app, "the turn ended");
    }
}

/// The transcript's rows on the emulator's screen, without their trailing
/// spaces.
pub(super) fn transcript_rows(terminal: &TestTerminal, app: &App) -> Vec<String> {
    let area = app.transcript.drawn.area;
    let rows = terminal.screen().rows();
    rows[area.y as usize..area.bottom() as usize].to_vec()
}

/// Draws a frame on `terminal`, and checks that the emulator shows what the
/// frame drew.
pub(super) fn frame(terminal: &mut TestTerminal, app: &mut App) -> Vec<String> {
    let drawn = terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = drawn.buffer().clone();
    let area = buffer.area();
    let expected: Vec<String> = (0..area.height)
        .map(|y| {
            let row: String = (0..area.width)
                .map(|x| buffer.symbol((x, y)).to_string())
                .collect();
            row.trim_end().to_string()
        })
        .collect();
    assert_eq!(terminal.screen().rows(), expected, "the emulator's screen");
    transcript_rows(terminal, app)
}

/// Whether `row` is the "Working…" line or the thought that streams, whose
/// spinner turns.
fn moves_by_itself(row: &str) -> bool {
    row.contains("Working…")
}

/// The rows with text that no other row has, with where they are.
pub(super) fn unique(rows: &[String]) -> Vec<(&str, usize)> {
    let mut seen: std::collections::HashMap<&str, (usize, usize)> = Default::default();
    for (y, row) in rows.iter().enumerate() {
        let text = row.trim();
        if text.is_empty() || moves_by_itself(row) {
            continue;
        }
        seen.entry(text).or_insert((y, 0)).1 += 1;
    }
    seen.into_iter()
        .filter(|(_, (_, count))| *count == 1)
        .map(|(text, (y, _))| (text, y))
        .collect()
}

/// The first word of `row`'s text, past its marker.
pub(super) fn first_word(row: &str) -> Option<&str> {
    row.split_whitespace().find(|word| word.starts_with('w'))
}

/// Following a session, in each view, no row of the screen moves down from
/// one frame to the next, and blank rows below the end never fill the
/// screen.
#[test]
fn following_a_session_never_moves_a_row_down() {
    let _full = choose_symbols(Symbols::Full);
    for (seed, view) in [
        (1, View::Full),
        (2, View::Quiet),
        (3, View::Messages),
        (4, View::Full),
    ] {
        let mut session = Session::new(seed, view);
        let mut terminal = crate::tui::golden::terminal(70, 24);
        let mut before: Vec<String> = Vec::new();
        let mut frames = 0;
        let mut check = |app: &mut App, what: &str| {
            let rows = frame(&mut terminal, app);
            frames += 1;
            assert!(app.transcript.following(), "{view:?} {what}: following");
            if !before.is_empty() {
                let now = unique(&rows);
                for (text, was) in unique(&before) {
                    if let Some((_, y)) = now.iter().find(|(row, _)| *row == text) {
                        assert!(
                            *y <= was,
                            "{view:?}, {what}: {text:?} went from row {was} to {y}\n{}\n----\n{}",
                            before.join("\n"),
                            rows.join("\n")
                        );
                    }
                }
            }
            let gap = app.transcript.gap() as usize;
            assert!(
                gap < rows.len(),
                "{view:?}, {what}: {gap} blank rows below the end\n{}",
                rows.join("\n")
            );
            before = rows;
        };
        for _ in 0..6 {
            session.turn(&mut check);
        }
        assert!(frames > 100, "{frames} frames");
    }
}

/// Scrolled up, in the Full and Quiet views, the screen stays as it is while
/// turns stream below it, a command above it ends and opens to its output,
/// and "Working…" comes and goes. The top row stays while every thought
/// opens and closes, and through other widths it keeps its first word.
#[test]
fn scrolled_up_the_text_at_the_top_holds() {
    let _full = choose_symbols(Symbols::Full);
    for (seed, view) in [(11, View::Full), (12, View::Quiet)] {
        let mut session = Session::new(seed, view);
        let mut terminal = crate::tui::golden::terminal(80, 30);
        let mut follow = |app: &mut App, _: &str| {
            frame(&mut terminal, app);
        };
        for _ in 0..6 {
            session.turn(&mut follow);
        }
        // Up past the long command, which still runs, to a row with words.
        session.app.transcript.scroll(-40);
        let mut held = frame(&mut terminal, &mut session.app);
        while first_word(&held[0]).is_none() {
            session.app.transcript.scroll(-1);
            held = frame(&mut terminal, &mut session.app);
        }
        assert!(!session.app.transcript.following());
        let last = held.len() - 1;
        let mut still = |app: &mut App, what: &str| {
            let rows = frame(&mut terminal, app);
            // (The marker of new lines is on the last row.)
            assert_eq!(
                rows[..last],
                held[..last],
                "{view:?}, {what}\n{}\n----\n{}",
                held.join("\n"),
                rows.join("\n")
            );
        };
        for _ in 0..3 {
            session.turn(&mut still);
        }
        // The long command ends above the screen, and its output opens there.
        let (call, words) = session.long.clone().unwrap();
        session.ends(call, &words, 12);
        still(&mut session.app, "the long command ended");
        assert!(!session.app.transcript.following());
        // Every thought opens, and closes again. (The top row may be one.)
        let top = held[0].clone();
        let word = first_word(&top).unwrap_or_else(|| panic!("a word in {top:?}"));
        for open in [true, false] {
            session.app.transcript.set_open_thinking(open);
            let rows = frame(&mut terminal, &mut session.app);
            assert_eq!(
                first_word(&rows[0]),
                Some(word),
                "{view:?}, thoughts open: {open}"
            );
        }
        // Other widths keep the top row's first word on it.
        for width in [100, 61, 47, 120, 80] {
            terminal.resize(width, 30);
            let rows = frame(&mut terminal, &mut session.app);
            assert!(
                rows[0].split_whitespace().any(|w| w == word),
                "{view:?} at {width}: {word:?} isn't on the top row\n{}",
                rows.join("\n")
            );
        }
        assert!(!session.app.transcript.following());
    }
}

/// A click, pressed and let go, on the transcript's row `y`.
pub(super) fn click(app: &mut App, y: usize) {
    let area = app.transcript.drawn.area;
    let y = area.y + u16::try_from(y).unwrap();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.mouse(MouseEvent::new(kind, area.x + 4, y, KeyModifiers::NONE));
    }
}

/// A notch of the wheel over the transcript.
pub(super) fn wheel(app: &mut App, kind: MouseEventKind) {
    let area = app.transcript.drawn.area;
    app.mouse(MouseEvent::new(
        kind,
        area.x + 4,
        area.y + 1,
        KeyModifiers::NONE,
    ));
}

/// No line of `after` is lower than in `before`.
pub(super) fn assert_none_lower(before: &[String], after: &[String], what: &str) {
    let now = unique(after);
    for (text, was) in unique(before) {
        if let Some((_, y)) = now.iter().find(|(row, _)| *row == text) {
            assert!(
                *y <= was,
                "{what}: {text:?} went from row {was} to {y}\n{}\n----\n{}",
                before.join("\n"),
                after.join("\n")
            );
        }
    }
}

/// Following in the Full view, the reader opens a thought of `lines` lines
/// near the end, scrolls down by up to `notches` of the wheel, and closes it.
/// Returns the session, its terminal and the screen it closed to, unless
/// the thought's first line left the screen.
fn closed_near_the_end(lines: u64, notches: usize) -> Option<(Session, TestTerminal, Vec<String>)> {
    let mut s = Session::new(31, View::Full);
    let mut terminal = crate::tui::golden::terminal(70, 24);
    let mut quiet = |_: &mut App, _: &str| {};
    s.turn(&mut quiet);
    s.turn(&mut quiet);
    let text: Vec<String> = (0..lines).map(|_| s.words(4)).collect();
    let first = text[0].clone();
    let thought = s.item(Kind::Thinking, &text.join("\n"));
    s.add(thought);
    let words = s.words(3);
    let reply = s.item(Kind::Text, &words);
    s.add(reply);
    let rows = frame(&mut terminal, &mut s.app);
    assert!(s.app.transcript.following());
    let at = |rows: &[String]| rows.iter().position(|row| row.contains(&first));
    // Open, and scroll down.
    click(&mut s.app, at(&rows).unwrap());
    let mut rows = frame(&mut terminal, &mut s.app);
    for _ in 0..notches {
        if s.app.transcript.following() {
            break;
        }
        wheel(&mut s.app, MouseEventKind::ScrollDown);
        rows = frame(&mut terminal, &mut s.app);
    }
    // Close. The line clicked stays, and no line moves down.
    let y = at(&rows)?;
    click(&mut s.app, y);
    let closed = frame(&mut terminal, &mut s.app);
    let what = format!("{lines} lines, {notches} notches");
    assert!(closed[y].contains(&first), "{what}\n{}", closed.join("\n"));
    assert_none_lower(&rows, &closed, &what);
    Some((s, terminal, closed))
}

/// Following in the Full view, the reader opens a thought near the end,
/// scrolls down (to the end, or part of the way), and closes it. The line
/// clicked stays, blank rows stand below the end where its lines were, and
/// the view follows on. A scroll down then moves no line, by the wheel's 3
/// rows or by 1, with 0 to 3 blank rows and more, and End closes them.
#[test]
fn a_scroll_down_after_a_fold_closed_near_the_end_moves_no_line() {
    let _full = choose_symbols(Symbols::Full);
    let mut gaps = std::collections::BTreeSet::new();
    for lines in [1, 2, 3, 4, 5, 8, 12] {
        for notches in [40, 1] {
            for step in [1, 3] {
                let what = format!("{lines} lines, {notches} notches, a scroll of {step}");
                let Some((mut s, mut terminal, closed)) = closed_near_the_end(lines, notches)
                else {
                    continue;
                };
                assert!(s.app.transcript.following(), "{what}");
                gaps.insert(s.app.transcript.gap());
                for _ in 0..2 {
                    if step == 3 {
                        wheel(&mut s.app, MouseEventKind::ScrollDown);
                    } else {
                        s.app.transcript.scroll(1);
                    }
                    let after = frame(&mut terminal, &mut s.app);
                    assert_eq!(after, closed, "{what}");
                    assert!(s.app.transcript.following(), "{what}");
                }
                // End closes the blank rows.
                s.app.transcript.follow();
                frame(&mut terminal, &mut s.app);
                assert_eq!(s.app.transcript.gap(), 0, "{what}");
            }
        }
    }
    for gap in [0, 1, 2, 3] {
        assert!(
            gaps.contains(&gap),
            "no case left {gap} blank rows: {gaps:?}"
        );
    }
    assert!(gaps.iter().any(|gap| *gap > 3), "{gaps:?}");
}

/// Scrolled up into a reply as it streams, the screen stays as it is as
/// more of it comes, a few words at a time and now and then several lines
/// in one frame, its bold words close and new paragraphs start.
#[test]
fn scrolled_up_inside_a_streaming_reply_the_screen_holds() {
    let _full = choose_symbols(Symbols::Full);
    let mut session = Session::new(21, View::Full);
    let mut terminal = crate::tui::golden::terminal(60, 20);
    let mut follow = |app: &mut App, _: &str| {
        frame(&mut terminal, app);
    };
    session.turn(&mut follow);
    let text = session.item(Kind::Text, "");
    let text = session.add(text);
    session.works(true);
    // Several screens of it, then up a few rows, inside it.
    for _ in 0..160 {
        let more = format!(" {}", session.words(5));
        session.append(text, &more);
        frame(&mut terminal, &mut session.app);
    }
    session.app.transcript.scroll(-4);
    let held = frame(&mut terminal, &mut session.app);
    assert!(held[0].contains('w'), "{held:?}");
    let last = held.len() - 1;
    for step in 0..60 {
        let n = if step % 9 == 4 { 30 } else { 1 + step % 5 };
        let mut more = format!(" {}", session.words(n));
        match step % 7 {
            0 => more.push_str(" **"),
            3 => more.push_str("**"),
            5 => more.push_str("\n\n"),
            _ => {}
        }
        session.append(text, &more);
        let rows = frame(&mut terminal, &mut session.app);
        assert_eq!(
            rows[..last],
            held[..last],
            "step {step}\n{}\n----\n{}",
            held.join("\n"),
            rows.join("\n")
        );
    }
}

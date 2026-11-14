//! What a reader sees as they scroll, click, select and switch views while
//! the session goes on, and a reader at random over sessions at random,
//! whose every frame is checked against the blocks as they are.

use super::*;
use crate::remote::Act;

fn item(id: u64, kind: Kind, text: &str) -> Item {
    let mut item = Item::new(kind, text);
    item.id = id;
    item.at = chrono::Utc::now().timestamp_millis();
    item
}

fn screen(t: &mut Transcript, filter: &Filter, width: u16, height: u16) -> Vec<String> {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    t.draw(&mut buffer, area, filter, &Theme::terminal());
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer.symbol((x, y)).to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn call(id: u64, kind: ActKind, target: &str, text: &str) -> Item {
    let mut call = item(id, Kind::Call, text);
    call.turn = Some(1);
    call.status = Some(CallStatus::Ok);
    call.act = Some(Act {
        kind,
        target: target.into(),
        ..Act::default()
    });
    call
}

/// In Messages, a turn that starts with nothing Messages shows (one for
/// news) and calls at once has its count after a turn that ended in a
/// thought, which leads to no call of another turn.
#[test]
fn a_count_shows_after_a_thought_of_the_turn_before() {
    let mut t = Transcript::new();
    let filter = Filter::Main(View::Messages);
    let mut user = item(1, Kind::User, "Go");
    user.turn = Some(1);
    t.apply(Update::Item(user));
    // The first turn ends in a thought (it was stopped).
    let mut thought = item(2, Kind::Thinking, "Where to start?");
    thought.turn = Some(1);
    t.apply(Update::Item(thought));
    // The next turn starts without a message (a worker's news) and calls.
    let mut read = call(3, ActKind::Read, "src/a.rs", "read src/a.rs");
    read.turn = Some(2);
    t.apply(Update::Item(read));
    let rows = screen(&mut t, &filter, 40, 6);
    assert!(
        rows.iter().any(|row| row.contains("1 tool call")),
        "{rows:?}"
    );
}

/// In Quiet, a row whose line cut its words opens to all of them.
#[test]
fn a_cut_row_opens_to_all_its_words() {
    let filter = Filter::Main(View::Quiet);
    let theme = Theme::terminal();
    let mut missing = Vec::new();
    for fixture in crate::remote::quiet::fixtures() {
        let mut t = Transcript::new();
        let mut call = item(1, Kind::Call, fixture.full);
        call.turn = Some(1);
        call.status = Some(fixture.status);
        call.outcome = Some(fixture.outcome.to_string()).filter(|outcome| !outcome.is_empty());
        call.act = Some(fixture.act.clone());
        call.detail = fixture.full_detail.map(str::to_string);
        call.job = fixture.job.then(|| "4d2c1a9b".to_string());
        call.ended = fixture.ended;
        t.apply(Update::Item(call));
        if let Some((kind, preview)) = fixture.content {
            let mut result = item(2, Kind::Result, preview);
            result.content = Some(kind);
            result.more = fixture.more.map(str::to_string);
            result.call = Some(1);
            result.turn = Some(1);
            t.apply(Update::Item(result));
        }
        let width = 50;
        let closed = screen(&mut t, &filter, width, 40);
        let blocks = t.blocks(&filter, &theme, width as usize);
        let Some(words) = blocks.iter().find_map(|block| match &block.shown {
            Shown::Quiet(row) => Some(row.words.text.clone()),
            _ => None,
        }) else {
            continue;
        };
        if !quiet_cut(
            &quiet::Words {
                text: words.clone(),
                ..quiet::Words::default()
            },
            width as usize,
        ) && !closed[0].contains('…')
        {
            continue;
        }
        if !closed[0].contains('…') {
            continue;
        }
        t.toggle(1, 0, &filter, &theme);
        let open = screen(&mut t, &filter, width, 40);
        let shown: String = open[1..]
            .iter()
            .flat_map(|row| row.chars())
            .filter(|c| !c.is_whitespace() && *c != '│')
            .collect();
        // (The last word the server didn't cut itself.)
        let last = words
            .split_whitespace()
            .rev()
            .find(|word| !word.contains('…'))
            .unwrap_or_default();
        if !shown.contains(last) {
            missing.push(format!(
                "{}: the line {:?} opens to {:?}, without {last:?}",
                fixture.name,
                closed[0],
                &open[1..open
                    .iter()
                    .rposition(|row| !row.is_empty())
                    .map_or(1, |at| at + 1)]
            ));
        }
    }
    assert!(missing.is_empty(), "{missing:#?}");
}

/// The marker counts the agent's output that comes above a note that
/// waits at the end.
#[test]
fn the_marker_counts_output_above_a_waiting_note() {
    let mut t = Transcript::new();
    let filter = Filter::Main(View::Full);
    for id in 1..=20 {
        t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
    }
    let mut note = item(21, Kind::User, "Also check the docs");
    note.pending = true;
    note.delivery = Some(Delivery::Note);
    t.apply(Update::Item(note));
    screen(&mut t, &filter, 40, 8);
    t.scroll(-6);
    let up = screen(&mut t, &filter, 40, 8);
    assert!(!t.following());
    for id in 22..=26 {
        t.apply(Update::Item(item(id, Kind::Text, &format!("text {id}"))));
    }
    let after = screen(&mut t, &filter, 40, 8);
    assert_eq!(after[..7], up[..7]);
    assert!(after[7].contains("new line"), "{after:?}");
}

/// In Quiet, the marker still counts what came after a thought the
/// reader stopped at went into the row above.
#[test]
fn the_marker_counts_on_when_a_thought_goes_into_its_row() {
    let mut t = Transcript::new();
    let filter = Filter::Main(View::Quiet);
    let mut user = item(1, Kind::User, "Go");
    user.turn = Some(1);
    t.apply(Update::Item(user));
    for id in 2..=30 {
        let mut text = item(id, Kind::Text, &format!("text {id}"));
        text.turn = Some(1);
        t.apply(Update::Item(text));
    }
    t.apply(Update::Item(call(
        31,
        ActKind::Read,
        "src/a.rs",
        "read src/a.rs",
    )));
    let mut thought = item(32, Kind::Thinking, "Now the other file.");
    thought.turn = Some(1);
    t.apply(Update::Item(thought));
    screen(&mut t, &filter, 40, 10);
    t.scroll(-6);
    let up = screen(&mut t, &filter, 40, 10);
    assert!(!t.following());
    // The next read joins the row, and the thought goes inside it.
    t.apply(Update::Item(call(
        33,
        ActKind::Read,
        "src/b.rs",
        "read src/b.rs",
    )));
    let tucked = screen(&mut t, &filter, 40, 10);
    assert_eq!(tucked[..9], up[..9]);
    // A command, a row of its own, one more line below the screen.
    t.apply(Update::Item(call(34, ActKind::Command, "ls", "$ ls")));
    let more = screen(&mut t, &filter, 40, 10);
    assert_eq!(more[..9], up[..9]);
    assert!(more[9].contains("new line"), "{:?}", more[9]);
}

/// What the last block grew by after following stopped still counts once
/// the width changes.
#[test]
fn the_marker_keeps_what_the_last_block_grew_by_at_another_width() {
    let mut t = Transcript::new();
    let filter = Filter::Main(View::Full);
    for id in 1..=20 {
        t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
    }
    t.apply(Update::Item(item(21, Kind::Reply, "The reply starts.")));
    screen(&mut t, &filter, 40, 8);
    t.scroll(-6);
    screen(&mut t, &filter, 40, 8);
    assert!(!t.following());
    for n in 0..5 {
        t.apply(Update::Append {
            id: 21,
            text: format!("\n\nParagraph {n} of the reply."),
        });
    }
    let new = |rows: &[String]| {
        let words: Vec<&str> = rows[7].split_whitespace().collect();
        let at = words.iter().position(|word| *word == "new")?;
        words.get(at.checked_sub(1)?)?.parse::<usize>().ok()
    };
    let up = screen(&mut t, &filter, 40, 8);
    let count = new(&up);
    assert!(count.is_some_and(|count| count >= 10), "{up:?}");
    let wider = screen(&mut t, &filter, 50, 8);
    assert_eq!(new(&wider), count, "{wider:?}");
}

/// A worker's heading wraps again when its time takes another column.
#[test]
fn a_heading_wraps_again_as_its_time_grows() {
    let now = chrono::Utc::now().timestamp_millis();
    let worker = WorkerSummary {
        // "r1 " + title + " · running · 9s" fills the 38 columns after the
        // margin exactly, so "10s" wraps.
        title: "t".repeat(38 - 3 - " · running · 9s".chars().count()),
        state: WorkerState::Running,
        started_at: Some(now - 9_400),
        running_since: Some(now - 9_400),
        ..WorkerSummary::new("r1")
    };
    let filter = Filter::Worker("r1".into(), View::Full);
    let mut t = Transcript::new();
    t.apply(Update::Worker(worker.clone()));
    t.apply(Update::Item(item(1, Kind::Text, "Its text").agent("r1")));
    let before = screen(&mut t, &filter, 40, 5);
    assert!(before[0].ends_with("9s"), "{before:?}");
    std::thread::sleep(Duration::from_millis(700));
    let after = screen(&mut t, &filter, 40, 5);
    // What a transcript that starts now draws.
    let mut fresh = Transcript::new();
    fresh.apply(Update::Worker(worker));
    fresh.apply(Update::Item(item(1, Kind::Text, "Its text").agent("r1")));
    let expected = screen(&mut fresh, &filter, 40, 5);
    assert_eq!(after, expected);
}

/// Every block has a row the reader can be on, also one with nothing to
/// show (a message with no text, first, with no blank line before it).
#[test]
fn a_block_with_nothing_to_show_has_a_row() {
    let mut t = Transcript::new();
    let filter = Filter::Main(View::Full);
    let theme = Theme::terminal();
    t.apply(Update::Item(item(1, Kind::Message, "")));
    t.apply(Update::Item(item(2, Kind::Text, "After it.")));
    screen(&mut t, &filter, 40, 6);
    let blocks = t.blocks(&filter, &theme, 40);
    assert_eq!(blocks.len(), 2);
    for block in &blocks {
        assert!(
            t.height(block, &theme, 40, false) > 0,
            "block {} has no row",
            block.key
        );
    }
}

/// A selection in a wrapped line keeps its words at another width.
#[test]
fn a_selection_keeps_its_words_at_another_width() {
    let mut t = Transcript::new();
    t.apply(Update::Item(item(
        1,
        Kind::Reply,
        "A long line that the screen wraps into two rows of text",
    )));
    let filter = Filter::Main(View::Full);
    let rows = screen(&mut t, &filter, 30, 4);
    assert_eq!(rows[0], "  A long line that the screen");
    assert_eq!(rows[1], "  wraps into two rows of text");
    // "wraps", on the second row.
    let (from, to) = (t.point_at(2, 1).unwrap(), t.point_at(6, 1).unwrap());
    t.select(from, to, &filter);
    assert_eq!(
        t.selected_text(&filter, &Theme::terminal()).as_deref(),
        Some("wraps")
    );
    let narrow = screen(&mut t, &filter, 26, 4);
    let copied = t.selected_text(&filter, &Theme::terminal());
    assert_eq!(copied.as_deref(), Some("wraps"), "{narrow:?}");
}

/// A closed thought that leaves some out ends with "…", also where its
/// first line fills the row.
#[test]
fn a_closed_thought_that_fills_its_row_ends_with_an_ellipsis() {
    let first = "Thinking about the first line here";
    for width in 34..42 {
        let mut t = Transcript::new();
        t.apply(Update::Item(item(
            1,
            Kind::Thinking,
            &format!("{first}\nAnd more."),
        )));
        let rows = screen(&mut t, &Filter::Main(View::Full), width, 3);
        assert!(rows[0].ends_with('…'), "{width}: {rows:?}");
    }
}

/// A frame with nothing on screen changing reads no thought's text, so
/// it takes as long with thoughts of 2 KB as with thoughts of a line.
#[test]
fn a_frame_reads_a_thoughts_text_only_when_it_changes() {
    let filter = Filter::Main(View::Full);
    let theme = Theme::terminal();
    let time = |text: &str| {
        let mut t = Transcript::new();
        for id in 1..=KEPT as u64 {
            t.apply(Update::Item(item(id, Kind::Thinking, text)));
        }
        screen(&mut t, &filter, 100, 40);
        // (The quickest of several, as the machine does other work.)
        (0..10)
            .map(|_| {
                let start = Instant::now();
                let _ = t.blocks(&filter, &theme, 100);
                start.elapsed()
            })
            .min()
            .unwrap_or_default()
    };
    let short = time("A short thought.");
    let long = time(&"A thought that goes on for a while, as they do. ".repeat(42));
    assert!(long < short * 3, "{short:?} against {long:?}");
}

/// A click (no drag) at `x`, `y`, as the app makes it of a press and a
/// release.
fn click(t: &mut Transcript, x: u16, y: u16, filter: &Filter) {
    let theme = Theme::terminal();
    let following = t.following();
    t.hold();
    t.release();
    if following {
        t.follow_on();
    }
    if let Some(key) = t.drawn.key_at(x, y) {
        if t.is_counter(key) {
            t.toggle_count(key, y);
        } else if let Some(fold) = t.fold_at(x, y, filter, &theme) {
            t.toggle(fold.item, y, filter, &theme);
        }
    }
}

/// A reader who follows, opens a fold near the end and closes it again
/// still follows, since the end is in sight as it was.
#[test]
fn a_fold_opened_and_closed_at_the_end_follows_on() {
    let mut t = Transcript::new();
    let filter = Filter::Main(View::Full);
    for id in 1..=20 {
        t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
    }
    let lines: Vec<String> = (1..=30).map(|n| format!("Thought line {n}.")).collect();
    t.apply(Update::Item(item(21, Kind::Thinking, &lines.join("\n"))));
    t.apply(Update::Item(item(22, Kind::Text, "Done.")));
    let before = screen(&mut t, &filter, 40, 12);
    assert!(t.following());
    let row = before
        .iter()
        .position(|row| row.contains("Thought line 1."))
        .unwrap() as u16;
    click(&mut t, 4, row, &filter);
    let open = screen(&mut t, &filter, 40, 12);
    assert!(open[row as usize].contains("Thought line 1."), "{open:?}");
    assert!(!t.following(), "it opened past the end of the screen");
    click(&mut t, 4, row, &filter);
    let closed = screen(&mut t, &filter, 40, 12);
    assert_eq!(closed, before);
    // What comes next shows.
    t.apply(Update::Item(item(23, Kind::Text, "And more.")));
    t.apply(Update::Item(item(24, Kind::Text, "And the last.")));
    let after = screen(&mut t, &filter, 40, 12);
    assert!(
        after.iter().any(|row| row.contains("And the last.")),
        "{after:?}"
    );
}

/// A fold the keyboard opens while the reader follows keeps its line
/// in sight.
#[test]
fn a_fold_opened_by_the_keyboard_stays_in_sight() {
    let mut t = Transcript::new();
    let filter = Filter::Main(View::Full);
    let theme = Theme::terminal();
    for id in 1..=20 {
        t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
    }
    t.apply(Update::Item(call(
        21,
        ActKind::Read,
        "src/a.rs",
        "read src/a.rs",
    )));
    let lines: Vec<String> = (1..=14).map(|n| format!("{n}\tline {n}")).collect();
    let mut result = item(22, Kind::Result, &lines.join("\n"));
    result.content = Some(ContentKind::Lines);
    result.call = Some(21);
    result.turn = Some(1);
    t.apply(Update::Item(result));
    let closed = screen(&mut t, &filter, 40, 10);
    assert!(closed[9].contains("read src/a.rs"), "{closed:?}");
    assert!(t.following());
    // The keyboard goes to the call's line and opens it.
    assert!(t.move_focus(0, &filter, &theme));
    assert_eq!(t.focus, Some(Target::Block(21)));
    screen(&mut t, &filter, 40, 10);
    t.open_focus(Some(true), &filter, &theme);
    let open = screen(&mut t, &filter, 40, 10);
    assert!(
        open.iter().any(|row| row.contains("read src/a.rs")),
        "the focused line left the screen: {open:?}"
    );
}

/// A reader who follows, opens a fold near the end with the keyboard and
/// closes it again sees the screen as it was and still follows, as with
/// the mouse. The fold's line stays on its row both times.
#[test]
fn a_fold_opened_and_closed_by_the_keyboard_follows_on() {
    let mut t = Transcript::new();
    let filter = Filter::Main(View::Full);
    let theme = Theme::terminal();
    for id in 1..=20 {
        t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
    }
    let lines: Vec<String> = (1..=30).map(|n| format!("Thought line {n}.")).collect();
    t.apply(Update::Item(item(21, Kind::Thinking, &lines.join("\n"))));
    t.apply(Update::Item(item(22, Kind::Text, "Done.")));
    let before = screen(&mut t, &filter, 40, 12);
    assert!(t.following());
    let row = before
        .iter()
        .position(|row| row.contains("Thought line 1."))
        .unwrap();
    t.focus = Some(Target::Block(21));
    t.open_focus(Some(true), &filter, &theme);
    let open = screen(&mut t, &filter, 40, 12);
    assert!(open[row].contains("Thought line 1."), "{open:?}");
    assert!(!t.following(), "it opened past the end of the screen");
    t.open_focus(Some(false), &filter, &theme);
    let closed = screen(&mut t, &filter, 40, 12);
    assert_eq!(closed, before);
    assert!(t.following());
    // What comes next shows.
    for id in 23..=40 {
        t.apply(Update::Item(item(id, Kind::Output, &format!("more {id}"))));
    }
    let after = screen(&mut t, &filter, 40, 12);
    assert!(after.iter().any(|row| row.contains("more 40")), "{after:?}");
}

/// Scrolled up a row into the blank rows below the end, the reader sees
/// output fill them, with no marker while the end shows. Once output passes
/// the bottom row, "↓ N new lines" shows and counts all that came since
/// following stopped.
#[test]
fn the_marker_shows_once_output_passes_the_bottom_row() {
    let mut t = Transcript::new();
    let filter = Filter::Main(View::Full);
    for id in 1..=20 {
        t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
    }
    t.apply(Update::Item(item(21, Kind::Output, "a\nb\nc\nd")));
    screen(&mut t, &filter, 40, 10);
    // The last output gets three lines shorter, and three blank rows stand
    // below it.
    t.apply(Update::Change(item(21, Kind::Output, "a")));
    let rows = screen(&mut t, &filter, 40, 10);
    assert!(t.following());
    assert_eq!(t.gap(), 3, "{rows:?}");
    t.scroll(-1);
    let rows = screen(&mut t, &filter, 40, 10);
    assert!(!t.following());
    assert_eq!(t.gap(), 2, "{rows:?}");
    assert!(t.drawn.marker.is_none(), "{rows:?}");
    // Two lines fill the blank rows.
    for id in 22..=23 {
        t.apply(Update::Item(item(id, Kind::Output, &format!("more {id}"))));
    }
    let rows = screen(&mut t, &filter, 40, 10);
    assert!(rows[9].contains("more 23"), "{rows:?}");
    assert!(t.drawn.marker.is_none(), "{rows:?}");
    // Two more pass the bottom row, and the rows stay.
    for id in 24..=25 {
        t.apply(Update::Item(item(id, Kind::Output, &format!("more {id}"))));
    }
    let rows = screen(&mut t, &filter, 40, 10);
    assert!(rows[9].contains("more 23"), "{rows:?}");
    assert!(rows[9].contains("4 new lines"), "{rows:?}");
}

/// The first wheel step up while a reply streams moves the rows the
/// reader saw by the whole step.
#[test]
fn the_first_step_up_is_whole_while_text_streams() {
    let mut t = Transcript::new();
    let filter = Filter::Main(View::Full);
    for id in 1..=30 {
        t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
    }
    let before = screen(&mut t, &filter, 40, 10);
    assert_eq!(before[0], "  line 21");
    // Two lines come in the frame the wheel turns in.
    t.apply(Update::Item(item(31, Kind::Output, "line 31")));
    t.apply(Update::Item(item(32, Kind::Output, "line 32")));
    t.scroll(-3);
    let after = screen(&mut t, &filter, 40, 10);
    assert_eq!(after[0], "  line 18", "{after:?}");
}

/// When the transcript's area gets a few rows back (the command menu
/// closes, the input box loses a line), the end is on the last row again.
#[test]
fn a_taller_area_brings_the_end_to_the_last_row() {
    let mut t = Transcript::new();
    let filter = Filter::Main(View::Full);
    for id in 1..=30 {
        t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
    }
    let before = screen(&mut t, &filter, 40, 10);
    assert_eq!(before[9], "  line 30");
    // The menu takes two rows, then gives them back.
    let menu = screen(&mut t, &filter, 40, 8);
    assert_eq!(menu[7], "  line 30");
    let after = screen(&mut t, &filter, 40, 10);
    assert_eq!(after, before, "the rows the menu took are blank");
}

/// On the whole screen, the command menu of a few rows opens over the
/// transcript's end and closes again, and the end is back on its row.
#[test]
fn the_menu_closing_leaves_no_blank_rows() {
    use crate::tui::app::{App, Info};
    use crate::tui::input::{Editor, History};
    use crate::tui::theme::{Catalog, Depth};
    use termelon::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = App::new(
        crate::remote::Snapshot::default(),
        Editor::new(History::load(None)),
        Info::default(),
        Catalog::load(None),
        Depth::Rgb,
    );
    for id in 1..=40 {
        app.transcript
            .apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
    }
    let draw = |app: &mut App| {
        crate::tui::golden::rows(60, 24, |frame| crate::tui::screen::draw(frame, app))
    };
    let row_of = |rows: &[String]| rows.iter().position(|row| row.trim() == "line 40");
    let before = draw(&mut app);
    for c in "/mo".chars() {
        app.typed(c);
    }
    let menu = draw(&mut app);
    for _ in 0..3 {
        app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    }
    let after = draw(&mut app);
    assert_ne!(
        row_of(&menu),
        row_of(&before),
        "the menu took rows: {menu:#?}"
    );
    assert_eq!(row_of(&after), row_of(&before), "{after:#?}");
}

// ----- a reader at random over sessions at random -----

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, below: u64) -> u64 {
        self.next() % below
    }
}

/// What a frame showed, for the checks.
struct Frame {
    rows: Vec<String>,
    refs: Vec<Option<RowRef>>,
    following: bool,
    gap: u32,
    /// Each block's sig and whether it has a blank line before it, by key.
    sigs: HashMap<u64, (u64, bool)>,
    /// The blocks' keys, in order.
    keys: Vec<u64>,
    area: (u16, u16),
    /// The feed's place, for a dump.
    place: String,
    /// Each block's height, by key.
    heights: HashMap<u64, usize>,
}

fn frame(t: &mut Transcript, filter: &Filter, width: u16, height: u16) -> Frame {
    let rows = screen(t, filter, width, height);
    let refs = t.drawn.rows.clone();
    let following = t.following();
    let gap = t.view.placement().gap;
    let theme = Theme::terminal();
    let blocks = t.blocks(filter, &theme, width as usize);
    let sigs = blocks
        .iter()
        .map(|block| (block.key, (block.sig, block.gap)))
        .collect();
    let own = matches!(filter, Filter::Worker(..));
    let heights = blocks
        .iter()
        .map(|block| (block.key, t.height(block, &theme, width as usize, own)))
        .collect();
    let placed: Vec<String> = t
        .view
        .placement()
        .items
        .iter()
        .map(|placed| {
            format!(
                "{}@{}+{}",
                t.view.feed().key(placed.item).copied().unwrap_or(0),
                placed.y,
                placed.height
            )
        })
        .collect();
    let place = format!(
        "anchor {:?} outcome {:?} gap {} placed {}",
        t.view.feed().anchor(),
        t.view.placement().outcome,
        gap,
        placed.join(" ")
    );
    Frame {
        rows,
        refs,
        following,
        gap,
        sigs,
        keys: blocks.iter().map(|block| block.key).collect(),
        area: (width, height),
        place,
        heights,
    }
}

fn dump(name: &str, shown: &Frame) {
    eprintln!("--- {name}: following {} {}", shown.following, shown.place);
    let tail: Vec<String> = shown
        .keys
        .iter()
        .rev()
        .take(14)
        .rev()
        .map(|key| {
            if *key == WORKING_KEY {
                "W".to_string()
            } else if is_count(*key) {
                format!("c{}", key & !COUNT_KEY)
            } else {
                key.to_string()
            }
        })
        .collect();
    eprintln!("    last blocks {}", tail.join(" "));
    for (row, text) in shown.rows.iter().enumerate() {
        let at = shown.refs[row]
            .map(|at| format!("{}:{}", at.key, at.line))
            .unwrap_or_default();
        eprintln!("{row:>3} {at:>10} |{text}");
    }
}

/// The checks on one frame. Returns what's wrong.
fn check(t: &mut Transcript, filter: &Filter, shown: &Frame) -> Vec<String> {
    let mut wrong = Vec::new();
    let theme = Theme::terminal();
    let (width, height) = shown.area;
    let own = matches!(filter, Filter::Worker(..));
    let blocks = t.blocks(filter, &theme, width as usize);
    // The feed's heights are the blocks' heights now.
    let placed: Vec<(u64, i64, u32)> = t
        .view
        .placement()
        .items
        .iter()
        .filter_map(|placed| {
            t.view
                .feed()
                .key(placed.item)
                .map(|key| (*key, placed.y, placed.height))
        })
        .collect();
    for (key, y, placed_height) in placed {
        let Some(block) = blocks.iter().find(|block| block.key == key) else {
            wrong.push(format!("the feed placed {key}, which is no block"));
            continue;
        };
        let now = t.height(block, &theme, width as usize, own);
        if now != placed_height as usize {
            wrong.push(format!(
                "block {key} at row {y} was placed {placed_height} rows high and is {now}"
            ));
        }
    }
    // Each row shows the line it says it shows.
    for (row, at) in shown.refs.iter().enumerate() {
        let Some(at) = at else { continue };
        let Some(block) = blocks.iter().find(|block| block.key == at.key) else {
            wrong.push(format!("row {row} shows {}, which is no block", at.key));
            continue;
        };
        // (Their time goes on between the frame and the check.)
        if matches!(block.shown, Shown::Worker(_) | Shown::Heading(_)) || block.face.clock.is_some()
        {
            continue;
        }
        let lines = t.block_rows(block, &theme, width as usize, own);
        let Some(line) = lines.get(at.line) else {
            wrong.push(format!(
                "row {row} shows line {} of block {}, which has {}",
                at.line,
                at.key,
                lines.len()
            ));
            continue;
        };
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        // (The marker covers the end of the last row, and a spinner stands
        // in the markers' column of a line that moves.)
        let marker = row + 1 == height as usize && t.drawn.marker.is_some();
        let moves = t.drawn.moving.iter().any(|line| line.at.1 as usize == row);
        let from = if moves { 2 } else { 0 };
        let cut: String = termelon::text::graphemes(&text)
            .scan(0usize, |used, (c, w)| {
                *used += usize::from(w);
                (*used <= width as usize).then_some(c)
            })
            .skip(from)
            .collect();
        let drawn: String = shown.rows[row].chars().skip(from).collect();
        if !marker && drawn.trim_end() != cut.trim_end() {
            wrong.push(format!(
                "row {row} shows {drawn:?}, and line {} of block {} is {cut:?}",
                at.line, at.key
            ));
        }
    }
    // Following, the end is in sight.
    if shown.following && height > 0 && !t.view.feed().is_empty() && !t.end_in_sight() {
        wrong.push("following with the end out of sight".into());
    }
    wrong
}

/// What must hold between two frames with nothing done by the reader.
fn check_still(before: &Frame, after: &Frame) -> Vec<String> {
    let mut wrong = Vec::new();
    if before.area != after.area {
        return wrong;
    }
    let same = |key: u64| {
        before
            .sigs
            .get(&key)
            .is_some_and(|sig| after.sigs.get(&key) == Some(sig))
    };
    let find = |at: &RowRef| {
        after.refs.iter().position(|other| {
            other.is_some_and(|other| (other.key, other.line) == (at.key, at.line))
        })
    };
    let Some(top) = before.refs.iter().flatten().next().copied() else {
        return wrong;
    };
    let was = before
        .refs
        .iter()
        .position(|at| *at == Some(top))
        .unwrap_or(0);
    if !same(top.key) {
        return wrong;
    }
    if !before.following && !after.following && was == 0 {
        // The row at the top is where it was.
        if find(&top) != Some(0) {
            wrong.push(format!(
                "scrolled up, the top row (block {} line {}) went to {:?}",
                top.key,
                top.line,
                find(&top)
            ));
        }
    }
    if before.following && after.following {
        // The first row doesn't move down, as blank rows stand below the
        // end however many there are.
        if let Some(now) = find(&top)
            && now > was
        {
            wrong.push(format!(
                "following, row {was} (block {} line {}) moved down to {now}, gap {} then {}",
                top.key, top.line, before.gap, after.gap
            ));
        }
    }
    wrong
}

/// A reader at random (the wheel, Home, End, clicks, the keyboard's focus,
/// drags, other views and a worker's, widths 16 to 119 and heights 0 to 33)
/// over sessions at random, a frame after every step. Each frame places
/// every block at the height it has now and shows on each row the line its
/// row table names, a reader who follows has the end in sight, the top row
/// of a reader who scrolled up and did nothing stays, a row of one who
/// follows doesn't move down, and a fold toggled
/// under the pointer keeps its line on its row. A block whose lines are
/// made from something its sig leaves out fails here.
///
/// `READER_SEEDS=n` runs n seeds (2), `READER_ONLY=seed,view` one run
/// (view 0 Full, 1 Quiet, 2 Messages), and `READER_DUMP=step` prints the
/// frames before and after that step.
#[test]
fn a_reader_at_random_sees_every_block_as_it_is() {
    use crate::remote::Received;
    use crate::testing::RandomSession;
    let theme = Theme::terminal();
    let mut all: Vec<String> = Vec::new();
    let number = |name: &str| -> Option<Vec<u64>> {
        let value = std::env::var(name).ok()?;
        Some(
            value
                .split(',')
                .filter_map(|part| part.parse().ok())
                .collect(),
        )
    };
    let seeds = number("READER_SEEDS")
        .and_then(|n| n.first().copied())
        .unwrap_or(2);
    let only = number("READER_ONLY");
    let dump_at = number("READER_DUMP").unwrap_or_default();
    for seed in 0..seeds {
        for (index, view) in View::ALL.into_iter().enumerate() {
            if only
                .as_ref()
                .is_some_and(|only| only[..] != [seed, index as u64])
            {
                continue;
            }
            let mut session = RandomSession::new(seed);
            let feed = session.feed().clone();
            let (start, mut updates) = feed.follow();
            let mut t = Transcript::new();
            t.reset(start);
            let mut rng = Rng(seed * 7919 + index as u64 + 1);
            let (mut width, mut height) = (72u16, 20u16);
            let mut filter = Filter::Main(view);
            let mut last = frame(&mut t, &filter, width, height);
            for step in 0..500u64 {
                let did = session.step();
                let mut took = Vec::new();
                while let Some(received) = updates.try_next() {
                    match received {
                        Received::Update(update) => {
                            took.push(match &update {
                                Update::Item(item) => format!("item {} {:?}", item.id, item.kind),
                                Update::Append { id, .. } => format!("append {id}"),
                                Update::Change(item) => {
                                    format!("change {} {:?}", item.id, item.kind)
                                }
                                Update::Remove { id } => format!("remove {id}"),
                                Update::Truncate { from } => format!("truncate {from}"),
                                _ => String::new(),
                            });
                            t.apply(update);
                        }
                        Received::Reset(snapshot) => t.reset(snapshot),
                    }
                }
                t.working = feed.subscribe().0.activity.working.then_some(0);
                let mut acted = true;
                let mut pinned = None;
                let what = match rng.below(40) {
                    0 | 1 => {
                        t.scroll(-(rng.below(30) as i64 + 1));
                        "scroll up"
                    }
                    2 => {
                        t.scroll(rng.below(30) as i64 + 1);
                        "scroll down"
                    }
                    3 => {
                        t.top();
                        "top"
                    }
                    4 => {
                        t.follow();
                        "follow"
                    }
                    5 => {
                        width = 16 + rng.below(104) as u16;
                        "width"
                    }
                    6 => {
                        height = rng.below(34) as u16;
                        "height"
                    }
                    7..=10 => {
                        // A click, as the app makes it.
                        let (x, y) = (
                            rng.below(width as u64) as u16,
                            rng.below(height.max(1) as u64) as u16,
                        );
                        let following = t.following();
                        t.hold();
                        if rng.below(3) == 0 {
                            last = frame(&mut t, &filter, width, height);
                            all.extend(check(&mut t, &filter, &last).into_iter().map(|wrong| {
                                format!("seed {seed} {view:?} step {step} held: {wrong}")
                            }));
                        }
                        t.release();
                        if following {
                            t.follow_on();
                        }
                        if let Some(key) = t.drawn.key_at(x, y) {
                            if t.is_counter(key) {
                                t.toggle_count(key, y);
                            } else if let Some(fold) = t.fold_at(x, y, &filter, &theme) {
                                t.toggle(fold.item, y, &filter, &theme);
                            }
                        }
                        pinned = t.acted.pin;
                        "click"
                    }
                    11 => {
                        t.move_focus(-((rng.below(3)) as isize), &filter, &theme);
                        "focus"
                    }
                    12 => {
                        t.open_focus(None, &filter, &theme);
                        "open focus"
                    }
                    13 => {
                        filter = match &filter {
                            Filter::Main(view) => Filter::Main(view.next()),
                            Filter::Worker(id, view) => Filter::Worker(id.clone(), view.next()),
                        };
                        "view"
                    }
                    14 => {
                        filter = match (&filter, t.workers().first()) {
                            (Filter::Main(view), Some(worker)) => {
                                Filter::Worker(worker.id.clone(), *view)
                            }
                            (Filter::Worker(_, view), _) => Filter::Main(*view),
                            (filter, None) => filter.clone(),
                        };
                        t.follow();
                        "watch"
                    }
                    15 => {
                        t.hold();
                        "hold"
                    }
                    16 => {
                        t.release();
                        "release"
                    }
                    17 => {
                        let a = t.point_at(
                            rng.below(width as u64) as u16,
                            rng.below(height.max(1) as u64) as u16,
                        );
                        let b = t.point_at(
                            rng.below(width as u64) as u16,
                            rng.below(height.max(1) as u64) as u16,
                        );
                        if let (Some(a), Some(b)) = (a, b) {
                            t.select(a, b, &filter);
                        }
                        let _ = t.selected_text(&filter, &theme);
                        "select"
                    }
                    _ => {
                        acted = false;
                        "nothing"
                    }
                };
                let shown = frame(&mut t, &filter, width, height);
                let mut wrong = check(&mut t, &filter, &shown);
                if !acted && !t.hold {
                    wrong.extend(check_still(&last, &shown));
                }
                // A fold toggled under the pointer keeps its line on its row,
                // unless the reader follows and it closed, or the
                // transcript's start is on the top row, which stops it short
                // or, following a transcript that fits, shows it from there.
                let closed = |key: u64| {
                    let height = |frame: &Frame| frame.heights.get(&key).copied().unwrap_or(0);
                    height(&shown) < height(&last)
                };
                let at_start = shown
                    .refs
                    .first()
                    .copied()
                    .flatten()
                    .is_some_and(|at| shown.keys.first() == Some(&at.key) && at.line == 0);
                if let Some(pin) = pinned
                    && !(shown.following && closed(pin.key))
                    && t.view.placement().outcome == termelon::feed::Outcome::Kept
                    && pin.row < height as usize
                {
                    let row = shown.refs.iter().position(|at| {
                        at.is_some_and(|at| (at.key, at.line) == (pin.key, pin.line))
                    });
                    let short =
                        at_start && (shown.following || row.is_some_and(|row| row < pin.row));
                    let at = shown.refs.get(pin.row).copied().flatten();
                    if !short && at.map(|at| (at.key, at.line)) != Some((pin.key, pin.line)) {
                        wrong.push(format!(
                            "the fold's line (block {} line {}) isn't on row {}, which shows {at:?}",
                            pin.key, pin.line, pin.row
                        ));
                    }
                }
                if dump_at.contains(&step) {
                    eprintln!(
                        "=== seed {seed} {view:?} step {step}: {did}, {what}, took {took:?}, filter {filter:?}"
                    );
                    dump("before", &last);
                    dump("after", &shown);
                }
                all.extend(wrong.into_iter().map(|wrong| {
                    format!("seed {seed} {view:?} step {step} ({did}, {what}): {wrong}")
                }));
                last = shown;
            }
        }
    }
    let total = all.len();
    for line in all.iter().take(80) {
        eprintln!("{line}");
    }
    assert_eq!(total, 0, "{total} wrong");
}

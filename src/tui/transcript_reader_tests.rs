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

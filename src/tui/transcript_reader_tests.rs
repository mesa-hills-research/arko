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

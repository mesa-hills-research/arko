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

//! The "Working…" line's spacing, measured on the whole screen: it has as
//! many blank rows below it (before the status line's rule, or the notes
//! that wait under it) as above it, whatever started the turn.

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::glyphs::{Symbols, tests::choose_symbols};
    use crate::remote::{Activity, CallStatus, Delivery, Item, Kind, Update, WorkerState};
    use crate::tui::app::{App, Info, tests::worker};
    use crate::tui::input::{Editor, History};
    use crate::tui::screen::draw;
    use crate::tui::theme::{Catalog, Depth};
    use crate::tui::transcript::View;

    fn item(id: u64, kind: Kind, text: &str) -> Item {
        let mut item = Item::new(kind, text);
        item.id = id;
        item.at = chrono::Utc::now().timestamp_millis();
        item
    }

    fn call(id: u64, text: &str, status: CallStatus) -> Item {
        let mut call = item(id, Kind::Call, text);
        call.status = Some(status);
        call
    }

    fn new_app() -> App {
        App::new(
            crate::remote::Snapshot::default(),
            Editor::new(History::load(None)),
            Info::default(),
            Catalog::load(None),
            Depth::Rgb,
        )
    }

    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
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

    /// What is right above the "Working…" line, or (`Nothing`) nothing is.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Above {
        /// The user's message, which started the turn.
        User,
        /// What Arko says as a turn that began by itself does (the workers'
        /// news, a monitor's alert, a job that ended).
        Notice,
        /// A worker's line.
        Worker,
        /// The agent's text, which went quiet.
        Text,
        /// The agent's message to the user, shown while it works.
        Message,
        /// The agent's reply to the turn before, which ended.
        Reply,
        /// A command's output.
        Output,
        /// A call, which ended.
        Call,
        /// A call and its output block, which ended.
        Result,
        Nothing,
    }

    impl Above {
        const ALL: [Above; 10] = [
            Above::User,
            Above::Notice,
            Above::Worker,
            Above::Text,
            Above::Message,
            Above::Reply,
            Above::Output,
            Above::Call,
            Above::Result,
            Above::Nothing,
        ];

        /// The blank rows on each side in the full view: one where the line
        /// follows what was said, none among the agent's calls.
        fn blank_rows(self) -> usize {
            match self {
                Above::Worker | Above::Call | Above::Result | Above::Nothing => 0,
                _ => 1,
            }
        }
    }

    /// An app whose agent works, with `above` the last row before its
    /// "Working…" line, on enough earlier turns to fill the screen, and a
    /// note the agent hasn't read waiting under the line when `note`.
    fn working(above: Above, note: bool, view: View) -> App {
        let mut app = new_app();
        app.view = view;
        let mut id = 0;
        let mut add = |app: &mut App, item: fn(u64) -> Item| {
            id += 1;
            app.transcript.apply(Update::Item(item(id)));
        };
        if above != Above::Nothing {
            for _ in 0..12 {
                add(&mut app, |id| item(id, Kind::Reply, "Earlier."));
                add(&mut app, |id| item(id, Kind::Turn, "3 s"));
            }
        }
        match above {
            Above::User => {
                let mut user = item(100, Kind::User, "Fix the tests.");
                user.delivery = Some(Delivery::Turn);
                app.transcript.apply(Update::Item(user));
            }
            Above::Notice => add(&mut app, |id| item(id, Kind::Notice, "r1 reported")),
            Above::Worker => {
                let mut line = item(100, Kind::WorkerLine, "");
                line.worker = Some("r1".into());
                app.transcript.apply(Update::Item(line));
                app.transcript
                    .apply(Update::Worker(worker("r1", WorkerState::Finished)));
            }
            Above::Text => {
                add(&mut app, |id| item(id, Kind::Text, "Looking at it."));
                app.transcript.quiet_text();
            }
            Above::Message => add(&mut app, |id| item(id, Kind::Message, "Halfway there.")),
            Above::Output => add(&mut app, |id| item(id, Kind::Output, "3 files changed")),
            Above::Call => add(&mut app, |id| call(id, "$ make", CallStatus::Ok)),
            Above::Result => {
                app.transcript
                    .apply(Update::Item(call(100, "$ make", CallStatus::Ok)));
                let mut result = item(101, Kind::Result, "12 passed");
                result.call = Some(100);
                app.transcript.apply(Update::Item(result));
            }
            Above::Reply | Above::Nothing => {}
        }
        if note {
            let mut note = item(200, Kind::User, "Use tabs.");
            note.delivery = Some(Delivery::Note);
            note.pending = true;
            app.transcript.apply(Update::Item(note));
        }
        // (A turn the agent started by itself begins after the last one's.)
        let since = chrono::Utc::now().timestamp_millis() + 5_000;
        app.set_activity(Activity {
            working: true,
            since: Some(since),
            question: None,
        });
        app
    }

    /// Whether `row` is the "Working…" line.
    fn is_working(row: &str) -> bool {
        row.contains("Working…") || row.contains("Working...")
    }

    /// Whether `row` is a rule, the full width of the screen.
    fn is_rule(row: &str, width: u16) -> bool {
        row.chars().count() == width as usize && row.chars().all(|c| c == '─' || c == '-')
    }

    /// The blank rows above the "Working…" line (up to the text above it)
    /// and below it (up to the next row of anything), and the row that
    /// follows the blank rows below.
    fn padding(rows: &[String]) -> Option<(usize, usize, usize)> {
        let at = rows.iter().position(|row| is_working(row))?;
        let blank = |row: &String| row.trim().is_empty();
        let above = rows[..at].iter().rev().take_while(|row| blank(row)).count();
        let below = rows[at + 1..].iter().take_while(|row| blank(row)).count();
        Some((above, below, at + 1 + below))
    }

    /// The sizes the full-screen view is used at: 60, 80 and 120 columns,
    /// and a short terminal of 12 rows.
    const SIZES: [(u16, u16); 5] = [(60, 24), (80, 24), (120, 24), (80, 12), (60, 12)];

    /// The views each size is drawn in: all of them at 80 columns, the full
    /// one at the others. With ASCII symbols, two sizes.
    fn runs(ascii: bool) -> Vec<(u16, u16, View)> {
        let mut runs = Vec::new();
        for (width, height) in SIZES {
            if ascii && (width, height) != (80, 24) && (width, height) != (60, 12) {
                continue;
            }
            for view in View::ALL {
                if view == View::Full || (width, height) == (80, 24) {
                    runs.push((width, height, view));
                }
            }
        }
        runs
    }

    /// Whatever started the turn, the line has the same number of blank rows
    /// above and below: a blank row on each side after what was said (the
    /// user's message, the agent's text, a message, a notice, a command's
    /// output, a reply), none on either side among the agent's calls (a
    /// call, its output, a worker's line), where it stands flush against the
    /// status line's rule. With a note waiting under it, which has a blank row
    /// of its own, a blank row on each side in every case. In every view, at
    /// every size, with ARKO_SYMBOLS=ascii and with the command menu docked.
    #[test]
    fn the_working_line_has_as_much_room_below_as_above() {
        for ascii in [false, true] {
            let _ascii = ascii.then(|| choose_symbols(Symbols::Ascii));
            for (width, height, view) in runs(ascii) {
                for (above, note) in Above::ALL
                    .into_iter()
                    .flat_map(|above| [(above, false), (above, true)])
                {
                    let mut app = working(above, note, view);
                    let rows = screen(&mut app, width, height);
                    let case = format!(
                        "{above:?}, note {note}, {view:?}, {width}x{height}, ascii {ascii}\n{}",
                        rows.join("\n")
                    );
                    let (above_rows, below_rows, next) =
                        padding(&rows).unwrap_or_else(|| panic!("no Working…: {case}"));
                    if above == Above::Nothing {
                        // Nothing above it: it is the first row.
                        assert_eq!(above_rows, 0, "{case}");
                        assert!(is_working(&rows[0]), "{case}");
                        continue;
                    }
                    assert_eq!(above_rows, below_rows, "{case}");
                    // The Messages view leaves a notice out, so the reply
                    // before it is above the line, and counts the calls in
                    // one row.
                    let wanted = match (view, above) {
                        _ if note => 1,
                        (View::Messages, Above::Notice) => 1,
                        (View::Messages, Above::Call | Above::Result) => 0,
                        _ => above.blank_rows(),
                    };
                    assert_eq!(above_rows, wanted, "{case}");
                    // What follows the blank rows below: the note that
                    // waits, or the rule above the status line.
                    if note {
                        assert!(rows[next].contains("Use tabs."), "{case}");
                    } else {
                        assert!(is_rule(&rows[next], width), "{case}");
                    }
                }
            }
        }
    }

    /// The command menu docked above the status line's rule leaves the
    /// line its room below it too, and a question has no line at all (the
    /// agent waits for the user, not for its model).
    #[test]
    fn the_menu_and_a_question_leave_the_line_as_it_is() {
        for above in [Above::User, Above::Call] {
            let mut app = working(above, false, View::Full);
            app.editor.insert('/');
            assert!(!app.menu_items().is_empty());
            let rows = screen(&mut app, 80, 30);
            let (before, after, next) = padding(&rows).expect("the line shows above the menu");
            assert_eq!(before, after, "{above:?}\n{}", rows.join("\n"));
            assert_eq!(before, above.blank_rows(), "{above:?}");
            // The menu's first row, not the rule, follows it.
            assert!(!is_rule(&rows[next], 80), "{above:?}\n{}", rows.join("\n"));
        }
        let mut app = working(Above::User, false, View::Full);
        let mut reading = crate::tui::app::tests::asking();
        reading.questions[0].waits = true;
        app.set_reading(reading);
        let rows = screen(&mut app, 80, 24);
        assert!(padding(&rows).is_none(), "{}", rows.join("\n"));
    }

    /// A worker's own view has the same room: its text above and below, a
    /// blank row on each side, and none among its calls.
    #[test]
    fn a_watched_workers_line_has_as_much_room_below_as_above() {
        for (kind, blank_rows) in [(Kind::Text, 1), (Kind::Call, 0)] {
            let mut app = new_app();
            let mut running = worker("r1", WorkerState::Running);
            running.started_at = Some(chrono::Utc::now().timestamp_millis() - 60_000);
            app.transcript.apply(Update::Worker(running));
            for id in 1..=10u64 {
                let mut text = item(id, Kind::Text, "Earlier.");
                text.agent = "r1".into();
                app.transcript.apply(Update::Item(text));
            }
            let mut last = if kind == Kind::Call {
                call(20, "$ make", CallStatus::Ok)
            } else {
                item(20, Kind::Text, "Looking at it.")
            };
            last.agent = "r1".into();
            app.transcript.apply(Update::Item(last));
            if kind == Kind::Text {
                app.transcript.quiet_text();
            }
            app.watch(Some("r1".into()));
            for (width, height) in SIZES {
                let rows = screen(&mut app, width, height);
                let case = format!("{kind:?} {width}x{height}\n{}", rows.join("\n"));
                let (above, below, next) = padding(&rows).unwrap_or_else(|| panic!("{case}"));
                assert_eq!((above, below), (blank_rows, blank_rows), "{case}");
                assert!(is_rule(&rows[next], width), "{case}");
            }
        }
    }

    /// The row of `rows` that has `text`.
    fn row_of(rows: &[String], text: &str) -> usize {
        rows.iter()
            .position(|row| row.contains(text))
            .unwrap_or_else(|| panic!("{text:?} not drawn:\n{}", rows.join("\n")))
    }

    /// What moves as the line comes and goes. Among the agent's calls it is
    /// one row that the next call takes over, so the calls above it stay
    /// put. After what was said, the next block (thinking, a call, text)
    /// takes its row, and the blank row it had below goes, so what is above
    /// moves down one row, once as the first thing of a turn comes.
    #[test]
    fn the_transcript_moves_only_by_the_one_row_the_line_gives_back() {
        // A call ended, the line stands under it, the next call begins.
        let mut app = working(Above::Call, false, View::Full);
        let first = row_of(&screen(&mut app, 60, 24), "$ make");
        app.transcript
            .apply(Update::Item(call(300, "$ test", CallStatus::Running)));
        let rows = screen(&mut app, 60, 24);
        assert!(
            padding(&rows).is_none(),
            "a call running shows the agent works"
        );
        assert_eq!(
            row_of(&rows, "$ make"),
            first,
            "the call takes the line's row"
        );

        // The user's message, the line, then thinking begins.
        let mut app = working(Above::User, false, View::Full);
        let before = row_of(&screen(&mut app, 60, 24), "Fix the tests.");
        app.transcript
            .apply(Update::Item(item(300, Kind::Thinking, "Where to start?")));
        let after = row_of(&screen(&mut app, 60, 24), "Fix the tests.");
        assert_eq!(after, before + 1, "down by the blank row below the line");
        // Then the first text.
        let mut app = working(Above::User, false, View::Full);
        app.transcript
            .apply(Update::Item(item(300, Kind::Text, "Starting.")));
        let after = row_of(&screen(&mut app, 60, 24), "Fix the tests.");
        assert_eq!(
            after,
            before + 1,
            "the text has the blank row the line had above"
        );
    }
}

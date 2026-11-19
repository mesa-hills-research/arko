//! The "Working…" line's spacing, measured on the whole screen. It has as
//! many blank rows below it (before the status line's rule) as above it (up
//! to what was said, or the notes that wait above it), whatever started the
//! turn.

#[cfg(test)]
mod tests {
    use crate::glyphs::{Symbols, tests::choose_symbols};
    use crate::remote::{Activity, CallStatus, Delivery, Item, Kind, Level, Update, WorkerState};
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
        crate::tui::golden::rows(width, height, |frame| draw(frame, app))
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

        /// The blank rows above the line in the full view, one where it
        /// follows what was said or the user's message, none after a line of
        /// work (a call, a worker's line, a notice), as the shared table
        /// spaces a line of work.
        fn blank_rows(self) -> usize {
            match self {
                Above::Worker | Above::Notice | Above::Call | Above::Result | Above::Nothing => 0,
                Above::User | Above::Text | Above::Message | Above::Reply | Above::Output => 1,
            }
        }
    }

    /// An app whose agent works, with `above` the last row before its
    /// "Working…" line, on enough earlier turns to fill the screen, and a
    /// note the agent hasn't read waiting above the line when `note`.
    fn working(above: Above, note: bool, view: View) -> App {
        let mut app = new_app();
        app.view = view;
        earlier_turns(&mut app, above != Above::Nothing);
        // (A turn the agent started by itself begins after the last one's.)
        let since = chrono::Utc::now().timestamp_millis() + 5_000;
        // What this turn adds comes after it began.
        let add = |app: &mut App, mut item: Item| {
            item.at = since;
            app.transcript.apply(Update::Item(item));
        };
        match above {
            Above::User => {
                let mut user = item(100, Kind::User, "Fix the tests.");
                user.delivery = Some(Delivery::Turn);
                add(&mut app, user);
            }
            Above::Notice => add(&mut app, item(100, Kind::Notice, "r1 reported")),
            Above::Worker => {
                // The agent's call that started the worker, which is the
                // worker's row, with its task and the worker's line.
                let mut start = crate::tui::inventory_tests::worker_start("r1", "Task of r1");
                start.id = 100;
                add(&mut app, start);
                let mut task = item(101, Kind::Result, "Task of r1");
                task.call = Some(100);
                task.content = Some(crate::remote::ContentKind::Muted);
                add(&mut app, task);
                let mut line = item(102, Kind::WorkerLine, "Task of r1");
                line.worker = Some("r1".into());
                add(&mut app, line);
                app.transcript
                    .apply(Update::Worker(worker("r1", WorkerState::Finished)));
            }
            Above::Text => {
                add(&mut app, item(100, Kind::Text, "Looking at it."));
                app.transcript.quiet_text();
            }
            Above::Message => add(&mut app, item(100, Kind::Message, "Halfway there.")),
            Above::Output => add(&mut app, item(100, Kind::Output, "3 files changed")),
            Above::Call => add(&mut app, call(100, "$ make", CallStatus::Ok)),
            Above::Result => {
                add(&mut app, call(100, "$ make", CallStatus::Ok));
                let mut result = item(101, Kind::Result, "12 passed");
                result.call = Some(100);
                add(&mut app, result);
            }
            Above::Reply | Above::Nothing => {}
        }
        if note {
            add(&mut app, unread_note(200));
        }
        works_since(&mut app, since);
        app
    }

    /// Enough earlier turns to fill the screen, when `fill`, each a reply.
    fn earlier_turns(app: &mut App, fill: bool) {
        if fill {
            for turn in 0..12 {
                let id = 1 + turn * 2;
                app.transcript
                    .apply(Update::Item(item(id, Kind::Reply, "Earlier.")));
                app.transcript
                    .apply(Update::Item(item(id + 1, Kind::Turn, "3 s")));
            }
        }
    }

    /// A note the agent hasn't read, which waits above the line.
    fn unread_note(id: u64) -> Item {
        let mut note = item(id, Kind::User, "Use tabs.");
        note.delivery = Some(Delivery::Note);
        note.pending = true;
        note
    }

    /// The main agent works, on a turn that began at `since`.
    fn works_since(app: &mut App, since: i64) {
        app.set_activity(Activity {
            reporting: false,
            working: true,
            since: Some(since),
            question: None,
        });
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

    /// The sizes the full-screen view is used at. They are 60, 80 and 120 columns,
    /// and a short terminal of 12 rows.
    const SIZES: [(u16, u16); 5] = [(60, 24), (80, 24), (120, 24), (80, 12), (60, 12)];

    /// The views each size is drawn in. All of them at 80 columns, the full
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

    /// The line is a line of work. It has a blank row above it after what
    /// was said (the user's message, the agent's text, a message, a
    /// command's output, a reply) and none after a line of work (a call, its
    /// output, a worker's line, a notice), and it stands flush against the
    /// status line's rule. With a note waiting above it, a blank row above
    /// in every case. In every view, at every size, with ARKO_SYMBOLS=ascii.
    #[test]
    fn the_working_line_is_spaced_as_a_line_of_work() {
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
                        // Nothing above it, so it is the first row, or the
                        // note is, with a blank row between them.
                        let first = if note { 2 } else { 0 };
                        assert_eq!(above_rows, first / 2, "{case}");
                        assert!(is_working(&rows[first]), "{case}");
                        assert_eq!(note, rows[0].contains("Use tabs."), "{case}");
                        continue;
                    }
                    assert_eq!(below_rows, 0, "{case}");
                    // (The Messages view counts the calls in one row.)
                    let wanted = match (view, above) {
                        _ if note => 1,
                        (View::Messages, Above::Call | Above::Result) => 0,
                        _ => above.blank_rows(),
                    };
                    assert_eq!(above_rows, wanted, "{case}");
                    // The note waits above the line's blank row, and the
                    // rule above the status line follows the line.
                    if note {
                        let at = next - 1;
                        assert!(rows[at - above_rows - 1].contains("Use tabs."), "{case}");
                    }
                    assert!(is_rule(&rows[next], width), "{case}");
                }
            }
        }
    }

    /// What starts a turn, the user's message, or what Arko starts one for
    /// while the agent waits for the user.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Start {
        /// The user's message, whose spacing the others have.
        Message,
        /// A worker's report, with the notice that opens the turn.
        Report,
        /// A background job that finished.
        Job,
        /// A monitor's alert.
        Alert,
        /// A monitor that ends soon.
        EndsSoon,
        /// The user's answers from the panel, which open the turn without a
        /// notice.
        Answers,
        /// What the user typed that the agent didn't read, which waits
        /// above the line.
        Unread,
        /// A model switch's new conversation.
        Switch,
    }

    impl Start {
        const ALL: [Start; 8] = [
            Start::Message,
            Start::Report,
            Start::Job,
            Start::Alert,
            Start::EndsSoon,
            Start::Answers,
            Start::Unread,
            Start::Switch,
        ];

        /// The notice that opens the turn, as `Opening::notice` words it.
        fn notice(self) -> Option<&'static str> {
            match self {
                Start::Report => Some("r1 reported"),
                Start::Job => Some("Job 4 finished"),
                Start::Alert => Some("Monitor 2 alerted"),
                Start::EndsSoon => Some("Monitor 2 ends in 3 min"),
                _ => None,
            }
        }
    }

    /// What comes in after the turn starts, before the agent's own output.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Then {
        Nothing,
        /// The calls of a worker that runs, which the Full view shows.
        WorkerCalls,
        /// A queued worker starts, and its card is placed in the new turn.
        WorkerStarts,
        /// A worker's warning.
        WorkerWarns,
    }

    /// What docks between the transcript and the status line's rule.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Below {
        Nothing,
        /// The chip of the agent's open questions.
        Chip,
        /// The panel that answers them.
        Panel,
        /// The command menu.
        Menu,
    }

    /// An app whose agent works on a turn that `start` began, after
    /// earlier turns that fill the screen, with `then` coming in after the
    /// start and `below` docked under the transcript.
    fn started(start: Start, then: Then, below: Below, view: View) -> App {
        let mut app = new_app();
        app.view = view;
        earlier_turns(&mut app, true);
        let since = chrono::Utc::now().timestamp_millis() + 5_000;
        let add = |app: &mut App, mut item: Item| {
            item.at = since;
            app.transcript.apply(Update::Item(item));
        };
        // The agent's question, before its reply, when the user answers it.
        if start == Start::Answers || below != Below::Nothing {
            app.transcript
                .apply(Update::Item(item(90, Kind::Question, "Tabs or spaces?")));
            app.transcript
                .apply(Update::Item(item(91, Kind::Reply, "Asked.")));
            app.transcript
                .apply(Update::Item(item(92, Kind::Turn, "3 s")));
        }
        // A worker that runs, and one that waits for a place.
        let mut running = worker("r2", WorkerState::Running);
        running.started_at = Some(since - 60_000);
        app.transcript.apply(Update::Worker(running));
        app.transcript
            .apply(Update::Worker(worker("r3", WorkerState::Queued)));
        match start {
            Start::Message => {
                let mut user = item(100, Kind::User, "Fix the tests.");
                user.delivery = Some(Delivery::Turn);
                add(&mut app, user);
            }
            Start::Unread => add(&mut app, unread_note(100)),
            Start::Switch => {
                add(
                    &mut app,
                    item(100, Kind::Conversation, "New conversation on sol"),
                );
                add(
                    &mut app,
                    item(101, Kind::Notice, "Switched to sol").level(Level::Info),
                );
            }
            Start::Answers => {}
            _ => {
                let notice = item(100, Kind::Notice, start.notice().unwrap_or_default())
                    .level(Level::Info)
                    .opens_turn();
                add(&mut app, notice);
            }
        }
        match then {
            Then::Nothing => {}
            Then::WorkerCalls => {
                for (id, status) in [(110, CallStatus::Ok), (111, CallStatus::Running)] {
                    let mut call = call(id, "$ cargo test", status);
                    call.agent = "r2".into();
                    add(&mut app, call);
                }
            }
            Then::WorkerStarts => {
                let mut line = item(110, Kind::WorkerLine, "Review the parser");
                line.worker = Some("r3".into());
                add(&mut app, line);
                let mut r3 = worker("r3", WorkerState::Running);
                r3.started_at = Some(since);
                app.transcript.apply(Update::Worker(r3));
            }
            Then::WorkerWarns => {
                let mut warning =
                    item(110, Kind::Notice, "rate limited, retrying").level(Level::Warn);
                warning.agent = "r2".into();
                add(&mut app, warning);
            }
        }
        match below {
            Below::Nothing => {}
            Below::Chip => {
                app.set_reading(crate::tui::app::tests::asking());
                app.panel.open = false;
            }
            Below::Panel => {
                app.set_reading(crate::tui::app::tests::asking());
                app.panel.open = true;
            }
            Below::Menu => app.editor.insert('/'),
        }
        works_since(&mut app, since);
        app
    }

    /// Whatever starts the turn (a worker's report, a job or a monitor, the
    /// user's answers, the notes they typed, a model switch), and whatever
    /// comes in before the agent's own output (a worker's calls, a queued
    /// worker's card, a worker's warning), the line is spaced as a line of
    /// work below what's above it: a blank row after the user's message, a
    /// note or a reply, none after a notice, a worker's line or its calls.
    /// None comes below it, before the chip, the panel, the menu or the
    /// status line's rule. The notes the user typed wait above it.
    #[test]
    fn a_turn_that_starts_by_itself_spaces_the_line_by_what_is_above_it() {
        let (width, height) = (80, 30);
        // Every case that fails, and the screen of the first.
        let mut failed: Vec<String> = Vec::new();
        let mut first = None;
        for view in View::ALL {
            for then in [
                Then::Nothing,
                Then::WorkerCalls,
                Then::WorkerStarts,
                Then::WorkerWarns,
            ] {
                for below in [Below::Nothing, Below::Chip, Below::Panel, Below::Menu] {
                    for start in Start::ALL {
                        let mut app = started(start, then, below, view);
                        let rows = screen(&mut app, width, height);
                        // (A note waits last. The main view shows a worker's
                        // card, not its calls or warnings, and Messages
                        // leaves out a notice that opens a turn.)
                        let work_above = match (start, then) {
                            (Start::Unread, _) => false,
                            (_, Then::WorkerStarts) => true,
                            (Start::Message | Start::Answers, _) => false,
                            (
                                Start::Report
                                | Start::Job
                                | Start::Alert
                                | Start::EndsSoon
                                | Start::Switch,
                                _,
                            ) => view != View::Messages,
                        };
                        let blank_above = usize::from(!work_above);
                        let case = format!("{start:?}, then {then:?}, below {below:?}, {view:?}");
                        let room = padding(&rows).map(|(before, after, next)| {
                            let at = next - after - 1;
                            let next = &rows[next];
                            let follows = match below {
                                Below::Nothing => is_rule(next, width),
                                Below::Chip => next.contains("Alt+Q"),
                                _ => !is_rule(next, width),
                            };
                            let note = start != Start::Unread
                                || rows[at - before - 1].contains("Use tabs.");
                            (before, after, follows && note)
                        });
                        if room != Some((blank_above, 0, true)) {
                            failed.push(format!("{case}: {room:?}"));
                            first.get_or_insert_with(|| format!("{case}\n{}", rows.join("\n")));
                        }
                    }
                }
            }
        }
        assert!(
            failed.is_empty(),
            "{} cases fail (blank rows above, below, and what follows them):\n{}\n\n{}",
            failed.len(),
            failed.join("\n"),
            first.unwrap_or_default()
        );
    }

    /// The command menu docked above the status line's rule follows the
    /// line with no blank row, as the rule does, and a question has no line
    /// at all (the agent waits for the user, not for its model).
    #[test]
    fn the_menu_and_a_question_leave_the_line_as_it_is() {
        for above in [Above::User, Above::Call] {
            let mut app = working(above, false, View::Full);
            app.editor.insert('/');
            assert!(!app.menu_items().is_empty());
            let rows = screen(&mut app, 80, 30);
            let (before, after, next) = padding(&rows).expect("the line shows above the menu");
            assert_eq!(after, 0, "{above:?}\n{}", rows.join("\n"));
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

    /// A worker's own view spaces the line the same way, a blank row above
    /// it after its text and none after its calls, and none below it.
    #[test]
    fn a_watched_workers_line_is_spaced_as_a_line_of_work() {
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
                assert_eq!((above, below), (blank_rows, 0), "{case}");
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

    /// Nothing above the line moves as it comes and goes. It is one row,
    /// which the next line of work takes over, a call among the agent's
    /// calls or a thought after what was said. Text has a blank row under
    /// it at the end, so drawn with text there, the rows above it stand a
    /// row higher.
    #[test]
    fn the_transcript_stays_where_it_is_as_the_line_goes() {
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
        let drawn = screen(&mut app, 60, 24);
        let before = row_of(&drawn, "Fix the tests.");
        let line = drawn
            .iter()
            .position(|row| is_working(row))
            .expect("the line");
        app.transcript
            .apply(Update::Item(item(300, Kind::Thinking, "Where to start?")));
        let rows = screen(&mut app, 60, 24);
        assert_eq!(
            row_of(&rows, "Fix the tests."),
            before,
            "nothing moves down"
        );
        assert_eq!(
            row_of(&rows, "Where to start?"),
            line,
            "the thought takes the line's row"
        );
        // Drawn with the text already there, the end is the text's blank row.
        let mut app = working(Above::User, false, View::Full);
        app.transcript
            .apply(Update::Item(item(300, Kind::Text, "Starting.")));
        let after = row_of(&screen(&mut app, 60, 24), "Fix the tests.");
        assert_eq!(after + 1, before, "the text has a blank row under it");
    }
}

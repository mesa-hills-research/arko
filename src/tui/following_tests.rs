//! What each input does to following, from each state, on termelon's
//! emulator through the app. The reader chooses to follow the end or to
//! read, and a press holds the view. A frame follows when they chose to,
//! nothing holds the view and the end shows. Each input of the table runs
//! from each state, alone and with output in the same frame, and every
//! frame is checked against what those three say.

use termelon::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use termelon::testing::TestTerminal;

use super::scroll_tests::{Session, frame, unique, wheel};
use crate::glyphs::{Symbols, tests::choose_symbols};
use crate::remote::{Activity, Kind, Update};
use crate::tui::app::App;
use crate::tui::transcript::View;

/// A session on the emulator, with the rows of its last frame.
struct Run {
    s: Session,
    term: TestTerminal,
    rows: Vec<String>,
    /// The first words of lines of plain output, which a click opens
    /// nothing at.
    plain: Vec<String>,
    /// The words of the command's line, a fold.
    fold: String,
}

impl Run {
    /// Two turns in `view` at 70 by 24, then output, a command and more
    /// output, which ends on the last row.
    fn new(view: View) -> Self {
        Self::sized(view, 70, 24)
    }

    fn sized(view: View, width: u16, height: u16) -> Self {
        let _full = choose_symbols(Symbols::Full);
        let mut s = Session::new(7, view);
        let term = crate::tui::golden::terminal(width, height);
        let mut quiet = |_: &mut App, _: &str| {};
        s.turn(&mut quiet);
        s.turn(&mut quiet);
        let mut run = Run {
            s,
            term,
            rows: Vec::new(),
            plain: Vec::new(),
            fold: String::new(),
        };
        run.draw();
        let first = run.output(10);
        let words = run.s.words(2);
        let call = run.s.command(&words);
        let call = run.s.add(call);
        run.s.ends(call, &words, 3);
        run.fold = format!("cargo test {words}");
        let last = run.output(8);
        run.plain = [first, last].concat();
        run.draw();
        run
    }

    fn app(&mut self) -> &mut App {
        &mut self.s.app
    }

    fn draw(&mut self) -> Vec<String> {
        let _full = choose_symbols(Symbols::Full);
        self.rows = frame(&mut self.term, &mut self.s.app);
        self.rows.clone()
    }

    /// Output of `lines` lines at the end, before the next frame. Returns
    /// the first word of each.
    fn output(&mut self, lines: usize) -> Vec<String> {
        let text: Vec<String> = (0..lines).map(|_| self.s.words(3)).collect();
        let item = self.s.item(Kind::Output, &text.join("\n"));
        self.s.add(item);
        text.iter()
            .map(|line| line.split(' ').next().unwrap_or_default().to_string())
            .collect()
    }

    /// A reply of `paragraphs` paragraphs at the end, which every view shows.
    fn reply(&mut self, paragraphs: usize) {
        let text: Vec<String> = (0..paragraphs).map(|_| self.s.words(3)).collect();
        let item = self.s.item(Kind::Text, &text.join("\n\n"));
        self.s.add(item);
    }

    /// The end gets `rows` rows shorter, drawn.
    fn shorten_end(&mut self, rows: usize) {
        let text: Vec<String> = (0..=rows).map(|_| self.s.words(3)).collect();
        let mut item = self.s.item(Kind::Output, &text.join("\n"));
        let id = self.s.add(item.clone());
        self.draw();
        item.text = text[0].clone();
        item.id = id;
        self.app().update(Update::Change(item), Activity::default);
        self.draw();
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.app().key(KeyEvent::new(code, modifiers));
    }

    fn mouse(&mut self, kind: MouseEventKind, y: usize) {
        let area = self.s.app.transcript.drawn.area;
        let y = area.y + u16::try_from(y).unwrap();
        self.app()
            .mouse(MouseEvent::new(kind, area.x + 4, y, KeyModifiers::NONE));
    }

    fn click(&mut self, y: usize) {
        self.mouse(MouseEventKind::Down(MouseButton::Left), y);
        self.mouse(MouseEventKind::Up(MouseButton::Left), y);
    }

    fn steps(&mut self, kind: MouseEventKind, n: usize) {
        for _ in 0..n {
            wheel(&mut self.s.app, kind);
        }
    }

    /// The row of a line of plain output on the screen.
    fn plain_row(&self) -> Option<usize> {
        self.rows.iter().position(|row| {
            row.split_whitespace()
                .next()
                .is_some_and(|word| self.plain.iter().any(|plain| plain == word))
        })
    }

    /// The row of the command's line on the screen.
    fn fold_row(&self) -> Option<usize> {
        self.rows.iter().position(|row| row.contains(&self.fold))
    }

    fn reading(&self) -> bool {
        self.s.app.transcript.reading()
    }

    fn following(&self) -> bool {
        self.s.app.transcript.following()
    }

    fn held(&self) -> bool {
        self.s.app.transcript.held()
    }

    fn gap(&self) -> u32 {
        self.s.app.transcript.gap()
    }

    /// The marker's text, while it shows.
    fn marker(&self) -> Option<String> {
        let rect = self.s.app.transcript.drawn.marker?;
        let area = self.s.app.transcript.drawn.area;
        let row = &self.rows[usize::from(rect.y - area.y)];
        Some(
            row.chars()
                .skip(usize::from(rect.x))
                .collect::<String>()
                .trim()
                .to_string(),
        )
    }

    /// How many new lines the marker says, 0 for "↓ end".
    fn new_lines(&self) -> Option<usize> {
        let marker = self.marker()?;
        Some(
            marker
                .split_whitespace()
                .nth(1)
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
        )
    }

    /// The checks of every frame. The view follows exactly when the reader
    /// chose to, nothing holds it and the end shows. The marker shows
    /// exactly when it doesn't follow and the end is off the screen. Text
    /// shows while there is any.
    fn check(&self, what: &str) {
        let t = &self.s.app.transcript;
        let shows = t.end_shows();
        assert_eq!(
            self.following(),
            !self.reading() && !self.held() && shows,
            "{what}: following, reading {}, held {}, end shows {shows}\n{}",
            self.reading(),
            self.held(),
            self.rows.join("\n")
        );
        assert_eq!(
            t.drawn.marker.is_some(),
            !self.following() && !shows,
            "{what}: the marker\n{}",
            self.rows.join("\n")
        );
        assert!(
            self.rows.iter().any(|row| !row.trim().is_empty()),
            "{what}: no text shows"
        );
    }
}

/// Where each unique line went from `before` to `after`, in rows down.
fn moved(before: &[String], after: &[String]) -> Vec<i64> {
    let now = unique(after);
    unique(before)
        .into_iter()
        .filter_map(|(text, was)| {
            let (_, y) = now.iter().find(|(row, _)| *row == text)?;
            Some(*y as i64 - was as i64)
        })
        .collect()
}

/// What the lines did.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Moves {
    Same,
    Down(i64),
    Up(i64),
    NotLower,
    /// Every line by the same rows, no further down than this.
    AtMost(i64),
    /// The clicked line kept its row.
    Kept,
    /// The top line's first word is still on the top row.
    TopKept,
    Any,
}

fn check_moves(moves: Moves, before: &[String], after: &[String], y: Option<usize>, what: &str) {
    let deltas = moved(before, after);
    let show = || {
        format!(
            "{what}: {moves:?}, {deltas:?}\n{}\n----\n{}",
            before.join("\n"),
            after.join("\n")
        )
    };
    match moves {
        Moves::Same => assert!(deltas.iter().all(|d| *d == 0), "{}", show()),
        // (No line in both moved them further than the screen.)
        Moves::Down(n) => assert!(deltas.iter().all(|d| *d == n), "{}", show()),
        Moves::Up(n) => assert!(deltas.iter().all(|d| *d == -n), "{}", show()),
        Moves::NotLower => assert!(deltas.iter().all(|d| *d <= 0), "{}", show()),
        Moves::AtMost(n) => assert!(
            deltas.iter().all(|d| *d <= n && Some(d) == deltas.first()),
            "{}",
            show()
        ),
        Moves::Kept => {
            let y = y.expect("the clicked row");
            let words = before[y]
                .trim_start_matches(['▸', '▾', ' '])
                .split(" · ")
                .next()
                .unwrap_or("");
            assert!(after[y].contains(words), "{}", show());
        }
        Moves::TopKept => {
            let word = before[0].split_whitespace().find(|w| w.starts_with('w'));
            if let Some(word) = word {
                assert!(after[0].split_whitespace().any(|w| w == word), "{}", show());
            }
        }
        Moves::Any => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Following, the end on the last row.
    F0,
    /// Following, 4 blank rows below the end.
    F4,
    /// Reading after a step up, the end in sight above 6 blank rows.
    Rv,
    /// Reading 12 rows up, the end off the screen.
    Rh,
    /// Held, pressed while following.
    HF,
    /// Held, pressed while reading.
    HR,
}

impl State {
    const ALL: [State; 6] = [
        State::F0,
        State::F4,
        State::Rv,
        State::Rh,
        State::HF,
        State::HR,
    ];

    fn make(self, view: View) -> Run {
        let mut run = Run::new(view);
        assert!(run.following(), "{self:?}");
        assert_eq!(run.gap(), 0, "{self:?}\n{}", run.rows.join("\n"));
        match self {
            State::F0 => {}
            State::F4 => {
                run.shorten_end(4);
                assert_eq!(run.gap(), 4, "{self:?}");
            }
            State::Rv => {
                run.shorten_end(9);
                run.steps(MouseEventKind::ScrollUp, 1);
                run.draw();
                assert!(run.reading() && run.gap() == 6, "{self:?}");
            }
            State::Rh | State::HR => {
                for _ in 0..4 {
                    run.steps(MouseEventKind::ScrollUp, 1);
                    run.draw();
                }
                assert!(run.reading() && run.marker().is_some(), "{self:?}");
            }
            State::HF => {}
        }
        if matches!(self, State::HF | State::HR) {
            let y = run.plain_row().expect("plain output on the screen");
            run.mouse(MouseEventKind::Down(MouseButton::Left), y);
            run.draw();
            assert!(run.held(), "{self:?}");
        }
        run.check(&format!("{self:?}"));
        run
    }

    fn held(self) -> bool {
        matches!(self, State::HF | State::HR)
    }

    fn reading(self) -> bool {
        matches!(self, State::Rv | State::Rh | State::HR)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Input {
    Up(usize),
    Down(usize),
    ShiftUp,
    ShiftDown,
    PageUp,
    PageDown,
    Home,
    End,
    Esc,
    Marker,
    CtrlG,
    ClickText,
    ClickFold,
    KeyFold,
    Send,
    Press,
    Release,
    DragRelease,
    FocusLost,
    Shorter,
    Width,
    ViewsRound,
    Output,
}

impl Input {
    const ALL: [Input; 25] = [
        Input::Up(1),
        Input::Up(5),
        Input::Down(1),
        Input::Down(40),
        Input::ShiftUp,
        Input::ShiftDown,
        Input::PageUp,
        Input::PageDown,
        Input::Home,
        Input::End,
        Input::Esc,
        Input::Marker,
        Input::CtrlG,
        Input::ClickText,
        Input::ClickFold,
        Input::KeyFold,
        Input::Send,
        Input::Press,
        Input::Release,
        Input::DragRelease,
        Input::FocusLost,
        Input::Shorter,
        Input::Width,
        Input::ViewsRound,
        Input::Output,
    ];

    /// Whether what the input acts on shows.
    fn applies(self, run: &Run) -> bool {
        match self {
            Input::Marker => run.marker().is_some(),
            Input::ClickFold | Input::KeyFold => run.fold_row().is_some(),
            Input::ClickText | Input::Press | Input::Release | Input::DragRelease => {
                run.plain_row().is_some()
            }
            _ => true,
        }
    }

    /// Does the input, before the next frame. The row it acted on, for a
    /// click.
    fn act(self, run: &mut Run) -> Option<usize> {
        match self {
            Input::Up(n) => run.steps(MouseEventKind::ScrollUp, n),
            Input::Down(n) => run.steps(MouseEventKind::ScrollDown, n),
            Input::ShiftUp => run.key(KeyCode::Up, KeyModifiers::SHIFT),
            Input::ShiftDown => run.key(KeyCode::Down, KeyModifiers::SHIFT),
            Input::PageUp => run.key(KeyCode::PageUp, KeyModifiers::NONE),
            Input::PageDown => run.key(KeyCode::PageDown, KeyModifiers::NONE),
            Input::Home => run.key(KeyCode::Home, KeyModifiers::NONE),
            Input::End => run.key(KeyCode::End, KeyModifiers::NONE),
            Input::Esc => run.key(KeyCode::Esc, KeyModifiers::NONE),
            Input::Marker => {
                let rect = run.s.app.transcript.drawn.marker.expect("the marker");
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    run.app().mouse(MouseEvent::new(
                        kind,
                        rect.x + 1,
                        rect.y,
                        KeyModifiers::NONE,
                    ));
                }
            }
            Input::CtrlG => run.key(KeyCode::Char('g'), KeyModifiers::CONTROL),
            Input::ClickText => {
                let y = run.plain_row().expect("plain output on the screen");
                run.click(y);
                return Some(y);
            }
            Input::ClickFold => {
                let y = run.fold_row().expect("the command on the screen");
                run.click(y);
                return Some(y);
            }
            Input::KeyFold => {
                let y = run.fold_row().expect("the command on the screen");
                run.key(KeyCode::Char('g'), KeyModifiers::CONTROL);
                run.key(KeyCode::Enter, KeyModifiers::NONE);
                return Some(y);
            }
            Input::Send => {
                run.key(KeyCode::Char('h'), KeyModifiers::NONE);
                run.key(KeyCode::Enter, KeyModifiers::NONE);
            }
            Input::Press => {
                let y = run.plain_row().expect("plain output on the screen");
                run.mouse(MouseEventKind::Down(MouseButton::Left), y);
            }
            Input::Release => {
                let y = run.plain_row().expect("plain output on the screen");
                run.mouse(MouseEventKind::Up(MouseButton::Left), y);
            }
            Input::DragRelease => {
                let y = run.plain_row().expect("plain output on the screen");
                run.mouse(MouseEventKind::Drag(MouseButton::Left), y + 1);
                run.mouse(MouseEventKind::Up(MouseButton::Left), y + 1);
            }
            Input::FocusLost => {
                run.app().focus_changed(false);
                run.app().focus_changed(true);
            }
            Input::Shorter => {
                let size = run.term.size();
                run.term.resize(size.width, size.height - 4);
            }
            Input::Width => {
                let size = run.term.size();
                run.term.resize(55, size.height);
            }
            Input::ViewsRound => {
                for _ in 0..3 {
                    run.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
                    run.draw();
                }
            }
            Input::Output => {
                run.output(5);
            }
        }
        None
    }

    /// What it does from `state`: whether the reader reads after it, whether
    /// the view is held, what the lines do, and whether the end is then on
    /// the last row with no blank rows. `None` where it doesn't apply.
    fn want(self, state: State) -> Option<(bool, bool, Moves, bool)> {
        use Input::*;
        use State::*;
        // (A screen of 19 rows less 2.)
        let page = 17;
        let held = state.held();
        let reading = state.reading();
        Some(match (state, self) {
            (_, Up(n)) => (true, held, Moves::Down(3 * n as i64), false),
            (_, ShiftUp) => (true, held, Moves::Down(3), false),
            (_, PageUp) => (true, held, Moves::Down(page), false),
            (Rv, Down(_) | ShiftDown | PageDown) => (false, false, Moves::Same, false),
            (F0 | F4 | HF, Down(_) | ShiftDown | PageDown) => (false, held, Moves::Same, false),
            (Rh | HR, Down(1) | ShiftDown) => (true, held, Moves::Up(3), false),
            (Rh | HR, Down(_) | PageDown) => (false, held, Moves::Up(12), !held),
            (_, Home) => (true, held, Moves::Any, false),
            (F0 | HF, End) => (false, false, Moves::Same, true),
            (F4, End) => (false, false, Moves::Down(4), true),
            (Rv, End) => (false, false, Moves::Down(6), true),
            (Rh | HR, End | Esc) => (false, false, Moves::Up(12), true),
            (F0 | F4 | Rv | HF, Esc) => (false, false, Moves::Same, false),
            (Rh, Marker) => (false, false, Moves::Up(12), true),
            (_, Marker) => return None,
            (HF | HR, CtrlG | ClickText | ClickFold | KeyFold | Press) => return None,
            (_, Send) => (false, false, Moves::Any, true),
            // (No line that opens shows there, so the last comes down from
            // above.)
            (Rv, CtrlG) => (true, false, Moves::Any, false),
            (_, CtrlG) => (reading, false, Moves::Same, false),
            (_, ClickText) => (reading, false, Moves::Same, false),
            (_, ClickFold | KeyFold) => (reading, false, Moves::Kept, false),
            (_, Press) => (reading, true, Moves::Same, false),
            (HF | HR, Release | DragRelease) => (reading, false, Moves::Same, false),
            (_, Release | DragRelease) => return None,
            (HF | HR, FocusLost) => (reading, false, Moves::Same, false),
            (_, FocusLost) => (reading, false, Moves::Same, false),
            (F0, Shorter) => (false, false, Moves::Up(4), false),
            (_, Shorter) => (reading, held, Moves::Same, false),
            (F0 | F4 | HF, Width) if !held => (false, false, Moves::Any, true),
            (_, Width) => (reading, held, Moves::TopKept, false),
            (HF | HR, ViewsRound) => return None,
            (F0 | F4, ViewsRound) => (false, false, Moves::Any, true),
            // (Back in Full, the end comes down rather than stand over
            // blank rows with lines hidden above.)
            (Rv, ViewsRound) => (true, false, Moves::Any, false),
            (_, ViewsRound) => (true, false, Moves::Same, false),
            (F0 | F4, Output) => (false, false, Moves::NotLower, false),
            (_, Output) => (reading, held, Moves::Same, false),
        })
    }
}

/// Every input of the table from every state, alone and with three rows of
/// output in the same frame, then three more rows in a frame of their own.
#[test]
fn every_input_from_every_state() {
    let mut ran = 0;
    for state in State::ALL {
        for input in Input::ALL {
            let Some((reading, held, moves, end_last)) = input.want(state) else {
                continue;
            };
            for with_output in [false, true] {
                let what = format!("{state:?}, {input:?}, output in the frame: {with_output}");
                let mut run = state.make(View::Full);
                if !input.applies(&run) {
                    continue;
                }
                let before = run.rows.clone();
                let y = input.act(&mut run);
                if with_output {
                    run.output(3);
                }
                let after = run.draw();
                run.check(&what);
                assert_eq!(
                    run.reading(),
                    reading,
                    "{what}: reading\n{}",
                    after.join("\n")
                );
                assert_eq!(run.held(), held, "{what}: held");
                // (Output while following moves the lines up, all alike, and
                // so does a scroll down to the rows it brought.)
                let down = matches!(input, Input::Down(_) | Input::ShiftDown | Input::PageDown);
                let moves = match moves {
                    Moves::Same if with_output && (run.following() || down) => Moves::AtMost(0),
                    Moves::Down(n) if with_output && run.following() => Moves::AtMost(n),
                    Moves::Up(n) if with_output && (run.following() || down) => Moves::AtMost(-n),
                    moves => moves,
                };
                check_moves(moves, &before, &after, y, &what);
                if end_last {
                    assert!(
                        run.following() && run.gap() == 0,
                        "{what}: the end on the last row"
                    );
                }
                // Three rows in a frame of their own.
                let before = after;
                run.output(3);
                let after = run.draw();
                run.check(&format!("{what}, then output"));
                let moves = if run.following() {
                    Moves::NotLower
                } else {
                    Moves::Same
                };
                check_moves(
                    moves,
                    &before,
                    &after,
                    None,
                    &format!("{what}, then output"),
                );
                ran += 1;
            }
        }
    }
    assert!(ran > 200, "{ran} cases");
}

impl Run {
    /// A command with `lines` lines of output at the end, before the next
    /// frame. Returns the words of its line.
    fn command(&mut self, lines: u64) -> String {
        let words = self.s.words(2);
        let call = self.s.command(&words);
        let call = self.s.add(call);
        self.s.ends(call, &words, lines);
        format!("cargo test {words}")
    }

    /// A thought of `lines` lines at the end, before the next frame.
    /// Returns its first line.
    fn thought(&mut self, lines: usize) -> String {
        let text: Vec<String> = (0..lines).map(|_| self.s.words(4)).collect();
        let item = self.s.item(Kind::Thinking, &text.join("\n"));
        self.s.add(item);
        text[0].clone()
    }

    /// The row whose text holds `words`.
    fn row_of(&self, words: &str) -> Option<usize> {
        self.rows.iter().position(|row| row.contains(words))
    }

    /// A run with no turns, only what `build` adds, drawn.
    fn bare(build: impl FnOnce(&mut Run)) -> Run {
        let _full = choose_symbols(Symbols::Full);
        let mut run = Run {
            s: Session::new(9, View::Full),
            term: crate::tui::golden::terminal(70, 24),
            rows: Vec::new(),
            plain: Vec::new(),
            fold: String::new(),
        };
        build(&mut run);
        run.draw();
        run
    }
}

/// A reader who scrolled up with the end in sight and toggles a fold that
/// leaves it in sight goes on reading, so what comes next doesn't move the
/// lines they went back to.
#[test]
fn a_fold_toggled_while_reading_keeps_the_reader_reading() {
    let mut run = Run::new(View::Full);
    let fold = run.command(3);
    run.output(4);
    run.draw();
    run.shorten_end(6);
    run.steps(MouseEventKind::ScrollUp, 1);
    run.draw();
    assert!(run.reading() && run.s.app.transcript.end_shows());
    let y = run.row_of(&fold).expect("the command shows");
    run.click(y);
    let toggled = run.draw();
    run.check("toggled");
    assert!(run.reading(), "{}", toggled.join("\n"));
    assert!(toggled[y].contains(&fold));
    run.output(30);
    let after = run.draw();
    run.check("output");
    check_moves(
        Moves::Same,
        &toggled,
        &after,
        None,
        "output after the toggle",
    );
    assert!(run.marker().is_some());
}

/// A follower opens a tall thought near the end, which takes the end off
/// the screen, and the view waits with the marker. Closed again, it
/// follows from there. Where output came meanwhile and the end still
/// doesn't show, it waits on, and a scroll down to the end follows.
#[test]
fn a_follower_who_opens_and_closes_a_tall_fold_follows_on() {
    for output in [0, 5] {
        let what = format!("{output} lines while open");
        let mut run = Run::new(View::Full);
        let first = run.thought(30);
        run.reply(1);
        run.draw();
        assert!(run.following());
        let y = run.row_of(&first).expect("the thought shows");
        run.click(y);
        let open = run.draw();
        run.check(&what);
        assert!(
            !run.reading() && !run.following(),
            "{what}\n{}",
            open.join("\n")
        );
        assert!(open[y].contains(&first));
        assert_eq!(
            run.new_lines(),
            Some(0),
            "{what}: the thought it opened isn't new"
        );
        if output > 0 {
            run.output(output);
            let more = run.draw();
            run.check(&what);
            check_moves(Moves::Same, &open, &more, None, &what);
        }
        run.click(y);
        let closed = run.draw();
        run.check(&what);
        assert!(closed[y].contains(&first), "{what}");
        assert!(!run.reading(), "{what}");
        if output == 0 {
            assert!(run.following(), "{what}\n{}", closed.join("\n"));
        } else {
            assert!(run.marker().is_some(), "{what}\n{}", closed.join("\n"));
            run.steps(MouseEventKind::ScrollDown, 40);
            run.draw();
            assert!(run.following(), "{what}");
        }
    }
}

/// A double click on a tall thought's line opens and closes it and selects
/// a word, and a follower follows on.
#[test]
fn a_double_click_on_a_tall_fold_leaves_a_follower_following() {
    let mut run = Run::new(View::Full);
    let first = run.thought(30);
    run.reply(1);
    run.draw();
    let y = run.row_of(&first).expect("the thought shows");
    run.click(y);
    run.click(y);
    let after = run.draw();
    run.check("double click");
    assert!(run.following(), "{}", after.join("\n"));
}

/// A flick down to the end and a click in one frame follow.
#[test]
fn a_flick_to_the_end_and_a_click_in_one_frame_follow() {
    let mut run = State::Rh.make(View::Full);
    run.steps(MouseEventKind::ScrollDown, 40);
    let y = run.plain_row().expect("plain output");
    run.click(y);
    run.draw();
    run.check("flick and click");
    assert!(run.following() && run.gap() == 0);
}

/// A click while output takes a follower's end off the screen catches up
/// to it at the release. A drag doesn't, so the selection stays in view.
#[test]
fn a_click_catches_up_with_the_end_and_a_drag_does_not() {
    for drag in [false, true] {
        let mut run = State::F0.make(View::Full);
        let y = run.plain_row().expect("plain output");
        run.mouse(MouseEventKind::Down(MouseButton::Left), y);
        if drag {
            run.mouse(MouseEventKind::Drag(MouseButton::Left), y + 1);
        }
        let held = run.draw();
        run.output(30);
        let more = run.draw();
        run.check("output while held");
        check_moves(Moves::Same, &held, &more, None, "held");
        run.mouse(MouseEventKind::Up(MouseButton::Left), y + usize::from(drag));
        let after = run.draw();
        run.check("released");
        assert!(!run.reading(), "drag {drag}");
        if drag {
            assert!(!run.following() && run.marker().is_some(), "drag");
            check_moves(Moves::Same, &more, &after, None, "the drag's release");
            run.steps(MouseEventKind::ScrollDown, 40);
            run.draw();
            assert!(run.following());
        } else {
            assert!(run.following() && run.gap() == 0, "{}", after.join("\n"));
        }
    }
}

/// A held view the wheel takes to the end stays held, so output after it
/// doesn't move the text under the pointer.
#[test]
fn a_held_view_at_the_end_stays_held() {
    let mut run = State::HR.make(View::Full);
    run.steps(MouseEventKind::ScrollDown, 40);
    let at_end = run.draw();
    run.check("at the end");
    assert!(run.held() && !run.reading());
    run.output(2);
    let after = run.draw();
    run.check("output");
    check_moves(Moves::Same, &at_end, &after, None, "held at the end");
}

/// A short transcript whose end got shorter, with rows held above the
/// screen. A scroll up that brings them down moves lines, so it reads, the
/// same in one frame or several.
#[test]
fn a_scroll_up_that_moves_a_line_reads_however_its_steps_fall() {
    let make = || {
        let mut run = Run::bare(|run| {
            run.output(12);
            run.output(10);
        });
        assert!(run.following());
        let text: Vec<String> = (0..10).map(|_| run.s.words(3)).collect();
        let mut item = run.s.item(Kind::Output, &text.join("\n"));
        let id = run.s.add(item.clone());
        run.draw();
        item.text = text[0].clone();
        item.id = id;
        run.app().update(Update::Change(item), Activity::default);
        run.draw();
        assert!(run.following() && run.gap() > 0);
        run
    };
    let mut one = make();
    let before = one.rows.clone();
    one.steps(MouseEventKind::ScrollUp, 1);
    let after = one.draw();
    one.check("one step");
    let deltas = moved(&before, &after);
    assert!(
        deltas.iter().any(|d| *d > 0),
        "the step moved lines\n{}",
        after.join("\n")
    );
    assert!(one.reading() && !one.following());
    // Six steps in one frame, and in six.
    let mut together = make();
    together.steps(MouseEventKind::ScrollUp, 6);
    together.draw();
    let mut apart = make();
    for _ in 0..6 {
        apart.steps(MouseEventKind::ScrollUp, 1);
        apart.draw();
    }
    assert_eq!(together.rows, apart.rows);
    assert_eq!(together.reading(), apart.reading());
    assert_eq!(together.following(), apart.following());
}

/// In a transcript wholly on the screen from its first row, a scroll up
/// and Home move nothing, and the view follows on.
#[test]
fn a_scroll_up_or_home_that_moves_nothing_changes_nothing() {
    let mut run = Run::bare(|run| {
        run.output(3);
        run.output(2);
    });
    let before = run.rows.clone();
    run.steps(MouseEventKind::ScrollUp, 1);
    let after = run.draw();
    run.check("step up");
    assert!(run.following() && !run.reading());
    check_moves(Moves::Same, &before, &after, None, "step up");
    run.key(KeyCode::Home, KeyModifiers::NONE);
    let after = run.draw();
    run.check("Home");
    assert!(run.following() && !run.reading());
    check_moves(Moves::Same, &before, &after, None, "Home");
}

/// Output in the frame where following stops counts as new.
#[test]
fn output_in_the_frame_following_stops_counts() {
    let mut run = State::F0.make(View::Full);
    run.steps(MouseEventKind::ScrollUp, 1);
    run.output(5);
    run.draw();
    run.check("step up and output");
    // (Output stacks under output.)
    assert_eq!(run.new_lines(), Some(5));
}

/// The marker counts the new rows below the last row. The ones the reader
/// saw fill blank rows aren't, and a step down that shows the rest follows.
#[test]
fn the_marker_counts_only_rows_below_the_screen() {
    let mut run = State::Rv.make(View::Full);
    run.output(7);
    run.draw();
    run.check("output");
    // (Seven rows, six of them in the blank rows.)
    assert_eq!(run.new_lines(), Some(1));
    run.steps(MouseEventKind::ScrollDown, 1);
    run.draw();
    run.check("step down");
    assert!(run.following() && run.marker().is_none());
}

/// Ctrl+O three times comes back to the block the reader left, and the
/// marker counts what came across the views.
#[test]
fn the_way_back_through_the_views_keeps_the_place_and_the_count() {
    let mut run = State::Rh.make(View::Full);
    run.reply(3);
    let before = run.draw();
    let count = run.new_lines();
    assert!(count.is_some_and(|n| n > 0), "{count:?}");
    for _ in 0..3 {
        run.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        run.draw();
        run.check("another view");
        assert!(run.reading());
    }
    assert_eq!(run.rows, before);
    assert_eq!(run.new_lines(), count);
}

/// The wheel over an overlay other than the help moves nothing behind it.
#[test]
fn the_wheel_over_an_overlay_moves_nothing_behind_it() {
    let mut run = State::F0.make(View::Full);
    let before = run.rows.clone();
    run.app().overlay = Some(crate::tui::overlay::Overlay::Settings {
        row: 0,
        picker: None,
    });
    run.steps(MouseEventKind::ScrollUp, 3);
    run.draw();
    run.app().overlay = None;
    let after = run.draw();
    assert!(run.following());
    check_moves(
        Moves::Same,
        &before,
        &after,
        None,
        "the wheel over the settings",
    );
}

/// Ctrl+G far up starts on the last row that opens on the screen, so
/// nothing moves, though one that opens comes below.
#[test]
fn ctrl_g_starts_on_the_screen() {
    let mut run = Run::new(View::Full);
    run.command(2);
    run.output(6);
    run.draw();
    for _ in 0..4 {
        run.steps(MouseEventKind::ScrollUp, 1);
        run.draw();
    }
    assert!(run.fold_row().is_some(), "{}", run.rows.join("\n"));
    let before = run.rows.clone();
    run.key(KeyCode::Char('g'), KeyModifiers::CONTROL);
    let after = run.draw();
    check_moves(Moves::Same, &before, &after, None, "Ctrl+G");
}

/// Back from a worker's view, the main view is as the reader left it.
#[test]
fn back_from_a_worker_the_main_view_is_as_the_reader_left_it() {
    let mut run = State::Rh.make(View::Full);
    let before = run.rows.clone();
    let marker = run.marker();
    run.app().watch(Some("r1".into()));
    run.draw();
    run.key(KeyCode::Esc, KeyModifiers::NONE);
    let after = run.draw();
    run.check("back");
    assert!(run.reading());
    assert_eq!(after, before);
    assert_eq!(run.marker(), marker);
}

/// A slash command leaves the view as it is.
#[test]
fn a_slash_command_leaves_the_view_as_it_is() {
    let mut run = State::Rv.make(View::Full);
    for c in "/workers".chars() {
        run.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    run.key(KeyCode::Enter, KeyModifiers::NONE);
    run.draw();
    assert!(run.reading());
}

/// In one frame, the last input that chooses wins. End and Esc drop a
/// scroll before them, a scroll after Esc reads, and a fold toggled drops
/// the frame's scroll.
#[test]
fn the_last_input_of_a_frame_wins() {
    let mut run = State::F0.make(View::Full);
    let before = run.rows.clone();
    run.steps(MouseEventKind::ScrollUp, 1);
    run.key(KeyCode::Esc, KeyModifiers::NONE);
    let after = run.draw();
    run.check("step up, Esc");
    assert!(run.following());
    check_moves(Moves::Same, &before, &after, None, "step up, Esc");
    let mut run = State::F0.make(View::Full);
    let before = run.rows.clone();
    run.key(KeyCode::Esc, KeyModifiers::NONE);
    run.steps(MouseEventKind::ScrollUp, 1);
    let after = run.draw();
    run.check("Esc, step up");
    assert!(run.reading());
    check_moves(Moves::Down(3), &before, &after, None, "Esc, step up");
    let mut run = State::F0.make(View::Full);
    let y = run.fold_row().expect("the command");
    run.steps(MouseEventKind::ScrollUp, 1);
    run.click(y);
    let after = run.draw();
    run.check("step up, fold");
    assert!(!run.reading());
    assert!(after[y].contains(&run.fold));
}

/// Two inputs in one frame: a press and its release, the wheel and a
/// press, a release and a taller screen, a fold and output.
#[test]
fn two_inputs_in_one_frame() {
    let mut run = State::F0.make(View::Full);
    let before = run.rows.clone();
    let y = run.plain_row().unwrap();
    run.click(y);
    let after = run.draw();
    run.check("press and release");
    assert!(run.following());
    check_moves(Moves::Same, &before, &after, None, "press and release");

    let mut run = State::F0.make(View::Full);
    let before = run.rows.clone();
    run.steps(MouseEventKind::ScrollUp, 1);
    run.mouse(MouseEventKind::Down(MouseButton::Left), 5);
    let after = run.draw();
    run.check("wheel and press");
    assert!(run.reading() && run.held());
    check_moves(Moves::Down(3), &before, &after, None, "wheel and press");

    let mut run = State::HR.make(View::Full);
    let before = run.rows.clone();
    let y = run.plain_row().unwrap();
    run.mouse(MouseEventKind::Up(MouseButton::Left), y);
    let size = run.term.size();
    run.term.resize(size.width, size.height + 3);
    let after = run.draw();
    run.check("release and taller");
    assert!(run.reading() && !run.held());
    check_moves(Moves::Same, &before, &after, None, "release and taller");

    let mut run = State::F0.make(View::Full);
    let y = run.fold_row().unwrap();
    run.click(y);
    run.output(3);
    let after = run.draw();
    run.check("fold and output");
    assert!(after[y].contains(&run.fold));
}

/// A reader at random through the app. Every frame, the view follows
/// exactly when the reader chose to, nothing holds it and the end shows,
/// and the marker shows exactly when it doesn't follow and the end is off
/// the screen. A frame of a scroll down alone has no line lower than
/// before, output alone changes no choice, and while reading or held it
/// moves no line on the screen. The marker's count grows only with output.
#[test]
fn a_reader_at_random_through_the_app() {
    let seeds: u64 = std::env::var("READER_SEEDS")
        .ok()
        .and_then(|seeds| seeds.parse().ok())
        .unwrap_or(2);
    for seed in 0..seeds {
        let mut run = Run::new(if seed % 2 == 0 {
            View::Full
        } else {
            View::Quiet
        });
        let mut rng = super::scroll_tests::Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        for step in 0..300 {
            let before = run.rows.clone();
            let reading = run.reading();
            let new = run.new_lines();
            let what = format!("seed {seed} step {step}");
            let kind = rng.below(12);
            let name = match kind {
                0 => {
                    run.steps(MouseEventKind::ScrollUp, 1 + rng.below(3) as usize);
                    "up"
                }
                1 => {
                    run.steps(MouseEventKind::ScrollDown, 1 + rng.below(8) as usize);
                    "down"
                }
                2..=4 => {
                    let lines = 1 + rng.below(6) as usize;
                    run.output(lines);
                    "output"
                }
                5 => {
                    run.key(KeyCode::End, KeyModifiers::NONE);
                    "End"
                }
                6 => {
                    run.key(KeyCode::Esc, KeyModifiers::NONE);
                    "Esc"
                }
                7 => {
                    let y = rng.below(run.rows.len().max(1) as u64) as usize;
                    run.click(y);
                    "click"
                }
                8 => {
                    let y = rng.below(run.rows.len().max(1) as u64) as usize;
                    if run.held() {
                        run.mouse(MouseEventKind::Up(MouseButton::Left), y);
                    } else {
                        run.mouse(MouseEventKind::Down(MouseButton::Left), y);
                    }
                    "press or release"
                }
                9 => {
                    run.shorten_end(1 + rng.below(4) as usize);
                    "shorter end"
                }
                10 => {
                    run.key(KeyCode::PageUp, KeyModifiers::NONE);
                    "PageUp"
                }
                _ => {
                    run.app().focus_changed(false);
                    run.app().focus_changed(true);
                    "focus"
                }
            };
            let after = run.draw();
            let what = format!("{what} {name}");
            run.check(&what);
            match name {
                "down" => check_moves(Moves::NotLower, &before, &after, None, &what),
                "output" => {
                    assert_eq!(run.reading(), reading, "{what}: output chose");
                    if !run.following() {
                        check_moves(Moves::Same, &before, &after, None, &what);
                    }
                    if let (Some(was), Some(now)) = (new, run.new_lines()) {
                        assert!(now >= was, "{what}: the count fell from {was} to {now}");
                    }
                }
                "focus" => assert_eq!(run.reading(), reading, "{what}: focus chose"),
                _ => {}
            }
        }
    }
}

/// Reading with blank rows below the end in sight, a scroll down moves
/// nothing and follows.
#[test]
fn a_scroll_down_from_reading_with_the_end_in_sight_moves_nothing() {
    for input in [
        Input::Down(1),
        Input::Down(40),
        Input::ShiftDown,
        Input::PageDown,
    ] {
        let mut run = State::Rv.make(View::Full);
        let before = run.rows.clone();
        input.act(&mut run);
        let after = run.draw();
        run.check(&format!("{input:?}"));
        assert!(run.following() && !run.reading(), "{input:?}");
        check_moves(Moves::Same, &before, &after, None, &format!("{input:?}"));
    }
}

/// A panel below the transcript opens and closes, and every line is back
/// on its row, with 0 to 6 blank rows below the end, and with a click or a
/// fold between. A taller window adds blank rows.
#[test]
fn a_panel_that_opens_and_closes_gives_every_line_back_its_row() {
    use crate::approvals::{Approval, ApprovalRequest, SkillApproval};
    use crate::remote::{Reading, WorkerState, WorkerSummary};
    type Toggle = fn(&mut Run, bool);
    let panels: [(&str, Toggle); 5] = [
        ("the command menu", |run, open| {
            run.key(
                if open {
                    KeyCode::Char('/')
                } else {
                    KeyCode::Backspace
                },
                KeyModifiers::NONE,
            );
        }),
        ("the input box", |run, open| {
            if open {
                run.key(KeyCode::Char('a'), KeyModifiers::NONE);
                for _ in 0..3 {
                    run.key(KeyCode::Enter, KeyModifiers::SHIFT);
                }
            } else {
                for _ in 0..4 {
                    run.key(KeyCode::Backspace, KeyModifiers::NONE);
                }
            }
        }),
        ("the approval tray", |run, open| {
            let approvals = if open {
                vec![ApprovalRequest {
                    id: "a1".into(),
                    batch: 1,
                    approval: Approval::Skill(SkillApproval {
                        name: "release-notes".into(),
                        ..SkillApproval::default()
                    }),
                }]
            } else {
                Vec::new()
            };
            run.app().set_reading(Reading {
                approvals,
                ..Reading::default()
            });
        }),
        ("the question chip", |run, open| {
            let reading = if open {
                super::app::tests::asking()
            } else {
                Reading::default()
            };
            run.app().set_reading(reading);
        }),
        ("the agent list", |run, open| {
            let mut worker = WorkerSummary::new("r1");
            if !open {
                worker.state = WorkerState::Finished;
                worker.ended_at = Some(0);
            }
            run.app().update(Update::Worker(worker), Activity::default);
        }),
    ];
    for (name, toggle) in panels {
        for blank in [0, 1, 2, 3, 6] {
            for between in ["nothing", "a click", "a fold"] {
                let what = format!("{name}, {blank} blank rows, {between}");
                let mut run = State::F0.make(View::Full);
                if blank > 0 {
                    run.shorten_end(blank);
                }
                let before = run.rows.clone();
                let gap = run.gap();
                toggle(&mut run, true);
                run.draw();
                run.check(&format!("{what}, open"));
                match between {
                    "a click" => {
                        if let Some(y) = run.plain_row() {
                            run.click(y);
                            run.draw();
                        }
                    }
                    "a fold" => {
                        if let Some(y) = run.fold_row() {
                            run.click(y);
                            run.draw();
                            run.click(y);
                            run.draw();
                        }
                    }
                    _ => {}
                }
                toggle(&mut run, false);
                let after = run.draw();
                run.check(&format!("{what}, closed"));
                assert!(run.following(), "{what}");
                check_moves(Moves::Same, &before, &after, None, &what);
                assert_eq!(run.gap(), gap, "{what}: blank rows");
            }
        }
    }
    let mut run = State::F0.make(View::Full);
    let before = run.rows.clone();
    let size = run.term.size();
    run.term.resize(size.width, size.height + 5);
    let after = run.draw();
    assert!(run.following());
    check_moves(Moves::Same, &before, &after, None, "a taller window");
    assert_eq!(run.gap(), 5, "a taller window's rows are blank");
}

/// A follower whose tall fold took the end off the screen keeps their
/// block through another view and back, and through a worker's view and
/// back, as a reader does.
#[test]
fn a_follower_away_from_the_end_keeps_their_block_in_another_view() {
    for worker in [false, true] {
        let what = if worker { "a worker's view" } else { "Ctrl+O" };
        let mut run = Run::new(View::Full);
        let first = run.thought(30);
        run.reply(1);
        run.draw();
        let y = run.row_of(&first).expect("the thought shows");
        run.click(y);
        let open = run.draw();
        assert!(!run.reading() && !run.following() && run.marker().is_some());
        if worker {
            run.app().watch(Some("r1".into()));
            run.draw();
            run.key(KeyCode::Esc, KeyModifiers::NONE);
            run.draw();
        } else {
            for _ in 0..3 {
                run.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
                run.draw();
                run.check(what);
                assert!(!run.reading(), "{what}");
            }
        }
        let after = run.draw();
        run.check(what);
        assert_eq!(after, open, "{what}");
        assert!(!run.following() && !run.reading(), "{what}");
    }
}

/// What a streaming reply grew by while the reader reads is still counted
/// in another view and back.
#[test]
fn the_count_of_a_growing_reply_carries_to_another_view() {
    for view in [View::Full, View::Quiet] {
        let mut run = Run::new(view);
        let text: Vec<String> = (0..2).map(|_| run.s.words(3)).collect();
        let reply = run.s.item(Kind::Text, &text.join("\n\n"));
        let reply = run.s.add(reply);
        run.draw();
        assert!(run.following(), "{view:?}");
        for _ in 0..4 {
            run.steps(MouseEventKind::ScrollUp, 1);
            run.draw();
        }
        assert!(run.reading(), "{view:?}");
        for _ in 0..12 {
            let more = format!("\n\n{}", run.s.words(3));
            run.s.append(reply, &more);
        }
        run.draw();
        let count = run.new_lines();
        assert!(count.is_some_and(|n| n > 0), "{view:?}: {count:?}");
        run.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        run.draw();
        if run.marker().is_some() {
            assert!(
                run.new_lines().is_some_and(|n| n > 0),
                "{view:?}: in the next view"
            );
        }
        for _ in 0..2 {
            run.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
            run.draw();
        }
        assert_eq!(run.new_lines(), count, "{view:?}: back where it was");
    }
}

/// The block kept for the way back through the views is forgotten at
/// another width, where its row names other text, so the way round
/// comes back to the top line's text.
#[test]
fn the_place_kept_for_the_views_goes_at_another_width() {
    let mut run = Run::new(View::Full);
    run.reply(1);
    run.draw();
    for _ in 0..6 {
        run.steps(MouseEventKind::ScrollUp, 1);
        run.draw();
    }
    assert!(run.reading());
    for _ in 0..3 {
        run.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        run.draw();
    }
    let size = run.term.size();
    run.term.resize(40, size.height);
    let before = run.draw();
    for _ in 0..3 {
        run.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        run.draw();
    }
    assert_eq!(run.rows, before);
}

impl Run {
    fn state(&self) -> String {
        format!(
            "choice {} · following {} · held {} · end shows {} · gap {} · marker {:?} · view {:?}",
            if self.reading() { "read" } else { "follow" },
            self.following(),
            self.held(),
            self.s.app.transcript.end_shows(),
            self.gap(),
            self.marker(),
            self.s.app.view,
        )
    }
}

/// Each row's first word of the session's (`w` and a number), where no other
/// row starts with it, with its row. The marker over a row's end doesn't
/// hide it.
fn firsts(rows: &[String]) -> std::collections::HashMap<String, usize> {
    let mut seen: std::collections::HashMap<String, (usize, usize)> = Default::default();
    for (y, row) in rows.iter().enumerate() {
        let word = row.split_whitespace().find(|word| {
            word.len() > 1 && word.starts_with('w') && word[1..].bytes().all(|b| b.is_ascii_digit())
        });
        if let Some(word) = word {
            let entry = seen.entry(word.to_string()).or_insert((y, 0));
            entry.1 += 1;
        }
    }
    seen.into_iter()
        .filter(|(_, (_, count))| *count == 1)
        .map(|(word, (y, _))| (word, y))
        .collect()
}

/// How far each line both frames show moved, down when positive.
fn shifts(before: &[String], after: &[String]) -> Vec<i64> {
    let now = firsts(after);
    firsts(before)
        .into_iter()
        .filter_map(|(word, was)| now.get(&word).map(|y| *y as i64 - was as i64))
        .collect()
}

/// A reader at random through the app, with more of the table's inputs:
/// the wheel, keys, output, End, Esc, clicks, presses, releases, drags, a
/// shorter end, Home, focus, Ctrl+O and the views round, Ctrl+G and keys
/// on the rows, other sizes, the command menu, sending, a worker's view and
/// back, and the marker. Every frame follows exactly when the reader chose
/// to, nothing holds the view and the end shows, the marker shows exactly
/// when it doesn't follow and the end is off the screen, and each input
/// does to the choice and the lines what the table says.
#[test]
fn a_wider_reader_at_random_through_the_app() {
    let seeds: u64 = std::env::var("READER_SEEDS")
        .ok()
        .and_then(|seeds| seeds.parse().ok())
        .unwrap_or(2);
    let steps = 400;
    let mut wrong: std::collections::BTreeMap<String, (u64, String)> = Default::default();
    let mut frames = 0u64;
    for seed in 0..seeds {
        let view = View::ALL[(seed % 3) as usize];
        let mut p = Run::new(view);
        let mut rng = super::scroll_tests::Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (mut width, mut height) = (70u16, 24u16);
        let mut menu = false;
        for step in 0..steps {
            let before = p.rows.clone();
            let (reading, held, following) = (p.reading(), p.held(), p.following());
            let (ends, gap) = (p.s.app.transcript.end_shows(), p.gap());
            let marker = p.marker();
            let focus = p.s.app.transcript.focus.is_some();
            let was = p.state();
            let rows = p.rows.len().max(1) as u64;
            let kind = rng.below(31);
            let name: &str = match kind {
                0 | 1 => {
                    p.steps(MouseEventKind::ScrollUp, 1 + rng.below(3) as usize);
                    "up"
                }
                2 | 3 => {
                    p.steps(MouseEventKind::ScrollDown, 1 + rng.below(8) as usize);
                    "down"
                }
                4..=8 => {
                    p.output(1 + rng.below(6) as usize);
                    "output"
                }
                9 => {
                    p.key(KeyCode::End, KeyModifiers::CONTROL);
                    "End"
                }
                10 => {
                    if menu {
                        "nothing"
                    } else {
                        p.key(KeyCode::Esc, KeyModifiers::NONE);
                        "Esc"
                    }
                }
                11 | 12 => {
                    p.click(rng.below(rows) as usize);
                    "click"
                }
                13 => {
                    let y = rng.below(rows) as usize;
                    if p.held() {
                        p.mouse(MouseEventKind::Up(MouseButton::Left), y);
                        "release"
                    } else {
                        p.mouse(MouseEventKind::Down(MouseButton::Left), y);
                        "press"
                    }
                }
                14 => {
                    p.shorten_end(1 + rng.below(4) as usize);
                    "shorter end"
                }
                15 => {
                    p.key(KeyCode::PageUp, KeyModifiers::NONE);
                    "PageUp"
                }
                16 => {
                    p.key(KeyCode::PageDown, KeyModifiers::NONE);
                    "PageDown"
                }
                17 => {
                    p.key(KeyCode::Home, KeyModifiers::CONTROL);
                    "Home"
                }
                18 => {
                    p.app().focus_changed(false);
                    p.app().focus_changed(true);
                    "focus"
                }
                19 => {
                    p.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
                    "view"
                }
                20 => {
                    p.key(KeyCode::Char('g'), KeyModifiers::CONTROL);
                    "Ctrl+G"
                }
                21 => {
                    if focus {
                        let code = match rng.below(4) {
                            0 => KeyCode::Up,
                            1 => KeyCode::Down,
                            2 => KeyCode::Enter,
                            _ => KeyCode::Char(' '),
                        };
                        p.key(code, KeyModifiers::NONE);
                        "a key on the rows"
                    } else {
                        "nothing"
                    }
                }
                22 => {
                    // A drag of a few rows, and its release.
                    let y = rng.below(rows) as usize;
                    let to = rng.below(rows) as usize;
                    p.mouse(MouseEventKind::Down(MouseButton::Left), y);
                    p.mouse(MouseEventKind::Drag(MouseButton::Left), to);
                    if rng.below(2) == 0 {
                        p.draw();
                    }
                    p.mouse(MouseEventKind::Up(MouseButton::Left), to);
                    "drag"
                }
                23 => {
                    height = 12 + rng.below(22) as u16;
                    p.term.resize(width, height);
                    "height"
                }
                24 => {
                    width = [55, 70, 90][rng.below(3) as usize];
                    p.term.resize(width, height);
                    "width"
                }
                25 => {
                    if focus {
                        "nothing"
                    } else {
                        if menu {
                            p.key(KeyCode::Backspace, KeyModifiers::NONE);
                        } else {
                            p.key(KeyCode::Char('/'), KeyModifiers::NONE);
                        }
                        menu = !menu;
                        "menu"
                    }
                }
                29 => {
                    // Three views round, or a panel opened and closed.
                    if menu || focus {
                        "nothing"
                    } else if rng.below(2) == 0 {
                        for _ in 0..2 {
                            p.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
                            p.draw();
                        }
                        p.key(KeyCode::Char('o'), KeyModifiers::CONTROL);
                        "views round"
                    } else {
                        p.key(KeyCode::Char('/'), KeyModifiers::NONE);
                        p.draw();
                        p.key(KeyCode::Backspace, KeyModifiers::NONE);
                        "menu round"
                    }
                }
                26 => {
                    if menu {
                        "nothing"
                    } else {
                        p.key(KeyCode::Char('h'), KeyModifiers::NONE);
                        p.key(KeyCode::Enter, KeyModifiers::NONE);
                        "send"
                    }
                }
                27 => {
                    if menu || focus {
                        "nothing"
                    } else {
                        p.app().watch(Some("r1".into()));
                        p.draw();
                        p.key(KeyCode::Esc, KeyModifiers::NONE);
                        "worker and back"
                    }
                }
                28 => {
                    if let Some(rect) = p.s.app.transcript.drawn.marker {
                        for kind in [
                            MouseEventKind::Down(MouseButton::Left),
                            MouseEventKind::Up(MouseButton::Left),
                        ] {
                            p.app().mouse(MouseEvent::new(
                                kind,
                                rect.x + 1,
                                rect.y,
                                KeyModifiers::NONE,
                            ));
                        }
                        "marker"
                    } else {
                        "nothing"
                    }
                }
                _ => "nothing",
            };
            let after = p.draw();
            frames += 1;
            let mut say = |kind: &str, detail: String| {
                let key = format!("{name}: {kind}");
                let entry = wrong.entry(key).or_insert_with(|| {
                    let mut text = format!(
                        "seed {seed} step {step} {view:?}\n  was  {was}\n  now  {detail}\n"
                    );
                    for y in 0..before.len().max(after.len()) {
                        let a = before.get(y).map_or("", String::as_str);
                        let b = after.get(y).map_or("", String::as_str);
                        let a: String = a.chars().take(60).collect();
                        let b: String = b.chars().take(60).collect();
                        text.push_str(&format!("{y:>2} {a:<60}│ {b}\n"));
                    }
                    (0, text)
                });
                entry.0 += 1;
            };
            let now = format!("{} · {width}×{height}", p.state());
            if before.is_empty() || after.is_empty() {
                // (No rows for the transcript.)
                continue;
            }
            // The frame follows exactly when the reader chose to, nothing
            // holds the view and the end shows.
            if p.following() != (!p.reading() && !p.held() && p.s.app.transcript.end_shows()) {
                say("following isn't choice, hold and end", now.clone());
            }
            if p.s.app.transcript.drawn.marker.is_some()
                != (!p.following() && !p.s.app.transcript.end_shows())
            {
                say("the marker", now.clone());
            }
            if !after.iter().any(|row| !row.trim().is_empty()) {
                say("no text shows", now.clone());
            }
            let deltas = shifts(&before, &after);
            let lower = deltas.iter().any(|d| *d > 0);
            let any = deltas.iter().any(|d| *d != 0);
            let same_size = before.len() == after.len();
            let jumped = deltas.is_empty();
            match name {
                "nothing" => {
                    if after != before || p.reading() != reading {
                        say("a frame with nothing done changed", now.clone());
                    }
                }
                "down" | "PageDown" => {
                    if lower {
                        say("a scroll down moved a line down", now.clone());
                    }
                    if p.reading() && !reading {
                        say("a scroll down chose to read", now.clone());
                    }
                }
                "up" | "PageUp" | "Home" => {
                    if !held && !reading && !jumped && lower != p.reading() {
                        say(
                            "a scroll up: read without a line moving, or the other way",
                            now.clone(),
                        );
                    }
                    if reading && !p.reading() {
                        say("a scroll up chose to follow", now.clone());
                    }
                }
                "output" => {
                    if p.reading() != reading {
                        say("output changed the choice", now.clone());
                    }
                    if !p.following() && any {
                        say("output moved a line while not following", now.clone());
                    }
                    if p.following() && following && lower {
                        say("output moved a line down while following", now.clone());
                    }
                }
                "focus" | "press" | "release" | "drag" | "height" | "width" | "view" | "menu" => {
                    if p.reading() != reading {
                        say("the choice changed", now.clone());
                    }
                    if (name == "press" || (name == "focus" && !held)) && any {
                        say("a line moved", now.clone());
                    }
                    if name == "menu" && reading && same_size && any {
                        say("a line moved", now.clone());
                    }
                }
                "click" => {
                    // (A click on the marker is End.)
                    if marker.is_none() && p.reading() != reading {
                        say("the choice changed", now.clone());
                    }
                }
                "End" | "send" | "marker" => {
                    if !(name == "marker" && held) && !(p.following() && p.gap() == 0) {
                        say("not at the end with no blank rows", now.clone());
                    }
                }
                "Esc" => {
                    if !focus && p.reading() {
                        say("still reading", now.clone());
                    }
                    if !focus && following && any {
                        say("a follower's lines moved", now.clone());
                    }
                    if !focus && !following && ends && !held && any {
                        say("lines moved with the end in sight", now.clone());
                    }
                }
                "views round" => {
                    if p.reading() != reading {
                        say("the choice changed", now.clone());
                    }
                    // (Over blank rows below the end, the end comes down in
                    // the view the reader comes back to, rule 9.)
                    let settles = ends && gap > 0;
                    if reading && !held && !settles && (after != before || p.marker() != marker) {
                        say(
                            "a reader's screen or marker isn't as they left it",
                            now.clone(),
                        );
                    }
                    if !reading && !held && !following && after != before {
                        say(
                            "a follower away from the end isn't where they were",
                            now.clone(),
                        );
                    }
                    if following && gap == 0 && after != before {
                        say("a follower's screen changed", now.clone());
                    }
                }
                "menu round" => {
                    if p.reading() != reading {
                        say("the choice changed", now.clone());
                    }
                    if after != before {
                        say("the screen isn't as it was", now.clone());
                    }
                }
                "worker and back" => {
                    if p.reading() != reading {
                        say("the choice changed", now.clone());
                    }
                    if reading && !held && (after != before || p.marker() != marker) {
                        say(
                            "a reader's screen or marker isn't as they left it",
                            now.clone(),
                        );
                    }
                    if !reading && !held && !following && after != before {
                        say(
                            "a follower away from the end isn't where they were",
                            now.clone(),
                        );
                    }
                    if following && gap == 0 && after != before {
                        say("a follower's screen changed", now.clone());
                    }
                }
                _ => {}
            }
        }
    }
    let report: Vec<String> = wrong
        .iter()
        .map(|(kind, (count, first))| format!("{kind}: {count} times, first at {first}"))
        .collect();
    assert!(
        wrong.is_empty(),
        "{frames} frames, {} kinds wrong\n{}",
        wrong.len(),
        report.join("\n")
    );
}

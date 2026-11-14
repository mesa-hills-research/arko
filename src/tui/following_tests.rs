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
        Some(row.chars().skip(usize::from(rect.x)).collect::<String>().trim().to_string())
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
    let show = || format!("{what}: {moves:?}, {deltas:?}\n{}\n----\n{}", before.join("\n"), after.join("\n"));
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
            let words = before[y].trim_start_matches(['▸', '▾', ' ']).split(" · ").next().unwrap_or("");
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
    const ALL: [State; 6] = [State::F0, State::F4, State::Rv, State::Rh, State::HF, State::HR];

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
    const ALL: [Input; 23] = [
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
            Input::ClickFold => run.fold_row().is_some(),
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
                    run.app()
                        .mouse(MouseEvent::new(kind, rect.x + 1, rect.y, KeyModifiers::NONE));
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
    /// the last row with no blank rows. `None` where it doesn't apply, or
    /// where the table's answer needs termelon 0.2.1 (a scroll down from
    /// reading with the end in sight, which an ignored test checks).
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
            (Rv, Down(_) | ShiftDown | PageDown) => return None,
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
            (HF | HR, CtrlG | ClickText | ClickFold | Press) => return None,
            // (No line that opens shows there, so the last comes down from
            // above.)
            (Rv, CtrlG) => (true, false, Moves::Any, false),
            (_, CtrlG) => (reading, false, Moves::Same, false),
            (_, ClickText) => (reading, false, Moves::Same, false),
            (_, ClickFold) => (reading, false, Moves::Kept, false),
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
                assert_eq!(run.reading(), reading, "{what}: reading\n{}", after.join("\n"));
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
                    assert!(run.following() && run.gap() == 0, "{what}: the end on the last row");
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
                check_moves(moves, &before, &after, None, &format!("{what}, then output"));
                ran += 1;
            }
        }
    }
    assert!(ran > 200, "{ran} cases");
}

//! The terminal's own cursor, visible only where the user types.
//!
//! ratatui writes the cells a frame changes with the real cursor moving
//! from one to the next, then shows the cursor and moves it to the input
//! box in a later write. A cursor left visible by the frame before hops
//! across the changed cells, and the one it shows is, until the next write,
//! on the last cell written. A terminal that paints between those writes
//! shows the cursor for a moment on the Working… line or in the status
//! line. [`CursorBackend`] hides the cursor before it writes cells and
//! shows it after moving it to the input box, so it is hidden or in its
//! place at every point of the output, however the output is split.

use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::queue;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// ratatui's crossterm backend, with the cursor kept off the cells it
/// writes. Showing the cursor takes effect when the frame moves it, or at
/// the next flush.
pub struct CursorBackend<W: Write> {
    inner: CrosstermBackend<W>,
    /// The frame shows the cursor, once it moves it.
    show: bool,
}

impl<W: Write> CursorBackend<W> {
    pub fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            show: false,
        }
    }

    /// Shows the cursor, now that it is where it goes.
    fn show_now(&mut self) -> io::Result<()> {
        if std::mem::take(&mut self.show) {
            queue!(self.inner, Show)?;
        }
        Ok(())
    }
}

impl<W: Write> Drop for CursorBackend<W> {
    fn drop(&mut self) {
        // A `Terminal` asks to show the cursor as it is dropped, and no
        // flush follows.
        if self.show_now().is_ok() {
            let _ = Write::flush(&mut self.inner);
        }
    }
}

impl<W: Write> Write for CursorBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(self)
    }
}

impl<W: Write> Backend for CursorBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut content = content.peekable();
        if content.peek().is_none() {
            return Ok(());
        }
        queue!(self.inner, Hide)?;
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.show = false;
        queue!(self.inner, Hide)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.show = true;
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let Position { x, y } = position.into();
        queue!(self.inner, MoveTo(x, y))?;
        self.show_now()
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        self.show_now()?;
        Write::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::remote::{
        Activity, CallStatus, Delivery, Item, Kind, Question, Update, WorkerState,
    };
    use crate::tui::app::App;
    use crate::tui::app::tests::{app, worker};
    use crate::tui::overlay::Overlay;
    use crate::tui::screen;
    use crate::tui::transcript::View;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::{Terminal, TerminalOptions, Viewport};

    /// What a terminal is sent, shared with the test that reads it.
    #[derive(Clone, Default)]
    struct Capture(Rc<RefCell<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A terminal's cursor as the bytes sent to it move it. It notes where
    /// the cursor is each time it is shown or moved while it is visible, and
    /// each time a character is drawn at it.
    struct Replay {
        visible: bool,
        at: Position,
        /// The visible cursor's place at each note, and whether a character
        /// was drawn there.
        notes: Vec<(Position, bool)>,
        /// The start of a sequence the last bytes broke off.
        rest: Vec<u8>,
    }

    impl Replay {
        /// A fresh terminal shows its cursor.
        fn new() -> Self {
            Self {
                visible: true,
                at: Position::ORIGIN,
                notes: Vec::new(),
                rest: Vec::new(),
            }
        }

        /// The cursor, where the terminal shows it.
        fn shown(&self) -> Option<Position> {
            self.visible.then_some(self.at)
        }

        fn note(&mut self, drawn: bool) {
            if self.visible {
                self.notes.push((self.at, drawn));
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            let mut data = std::mem::take(&mut self.rest);
            data.extend_from_slice(bytes);
            let mut i = 0;
            while i < data.len() {
                let byte = data[i];
                if byte == 0x1b {
                    if data.get(i + 1) != Some(&b'[') {
                        i += 2;
                        continue;
                    }
                    let Some(len) = data[i + 2..]
                        .iter()
                        .position(|byte| (0x40..=0x7e).contains(byte))
                    else {
                        break;
                    };
                    let params = String::from_utf8_lossy(&data[i + 2..i + 2 + len]).into_owned();
                    self.sequence(&params, data[i + 2 + len]);
                    i += 3 + len;
                } else if byte < 0x20 {
                    i += 1;
                } else {
                    let len = match byte {
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xff => 4,
                        _ => 1,
                    };
                    if i + len > data.len() {
                        break;
                    }
                    self.note(true);
                    self.at.x += 1;
                    i += len;
                }
            }
            self.rest = data[i..].to_vec();
        }

        fn sequence(&mut self, params: &str, last: u8) {
            match (params, last) {
                ("?25", b'h') => {
                    self.visible = true;
                    self.note(false);
                }
                ("?25", b'l') => self.visible = false,
                (_, b'H') => {
                    let mut parts = params
                        .split(';')
                        .map(|part| part.parse::<u16>().unwrap_or(1));
                    let row = parts.next().unwrap_or(1);
                    let column = parts.next().unwrap_or(1);
                    self.at = Position {
                        x: column - 1,
                        y: row - 1,
                    };
                    self.note(false);
                }
                _ => {}
            }
        }
    }

    /// A terminal of `width` by `height` cells that sends its bytes to the
    /// returned capture.
    fn terminal(width: u16, height: u16) -> (Terminal<CursorBackend<Capture>>, Capture) {
        let capture = Capture::default();
        let terminal = Terminal::with_options(
            CursorBackend::new(capture.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, width, height)),
            },
        )
        .unwrap();
        (terminal, capture)
    }

    /// Cells are drawn with the cursor hidden, and it ends in its place.
    #[test]
    fn the_cursor_is_hidden_while_cells_are_written() {
        let (mut terminal, capture) = terminal(20, 3);
        let mut replay = Replay::new();
        for frame in 0..4 {
            terminal
                .draw(|f| {
                    f.render_widget(format!("frame {frame}"), f.area());
                    f.set_cursor_position(Position { x: 2, y: 2 });
                })
                .unwrap();
            replay.feed(&capture.0.take());
            assert_eq!(replay.shown(), Some(Position { x: 2, y: 2 }), "{frame}");
        }
        let place = (Position { x: 2, y: 2 }, false);
        assert!(
            replay.notes.iter().all(|note| *note == place),
            "{:?}",
            replay.notes
        );
    }

    /// A frame that changes nothing writes no cells and no colours, only
    /// where the cursor goes.
    #[test]
    fn a_frame_that_changes_nothing_writes_no_cells() {
        let (mut terminal, capture) = terminal(20, 3);
        let mut frame = |x| {
            terminal
                .draw(|f| {
                    f.render_widget("same", f.area());
                    f.set_cursor_position(Position { x, y: 2 });
                })
                .unwrap();
            String::from_utf8(capture.0.take()).unwrap()
        };
        frame(1);
        assert_eq!(frame(5), "\x1b[3;6H\x1b[?25h");
    }

    /// A frame with no cursor leaves it hidden, and the next one that has it
    /// shows it where it goes.
    #[test]
    fn a_frame_without_a_cursor_hides_it() {
        let (mut terminal, capture) = terminal(20, 3);
        let mut replay = Replay::new();
        let places = [Some((3, 1)), None, None, Some((4, 0))];
        for (frame, place) in places.into_iter().enumerate() {
            terminal
                .draw(|f| {
                    f.render_widget(format!("frame {frame}"), f.area());
                    if let Some((x, y)) = place {
                        f.set_cursor_position(Position { x, y });
                    }
                })
                .unwrap();
            replay.feed(&capture.0.take());
            let shown = place.map(|(x, y)| Position { x, y });
            assert_eq!(replay.shown(), shown, "frame {frame}");
        }
        let places = [Position { x: 3, y: 1 }, Position { x: 4, y: 0 }];
        assert!(
            replay
                .notes
                .iter()
                .all(|(at, drawn)| !drawn && places.contains(at)),
            "{:?}",
            replay.notes
        );
    }

    /// A cursor waiting to be shown is shown as the backend is dropped.
    #[test]
    fn a_cursor_waiting_to_show_shows_as_the_backend_drops() {
        let (mut terminal, capture) = terminal(20, 3);
        terminal.show_cursor().unwrap();
        assert!(capture.0.borrow().is_empty());
        drop(terminal);
        assert_eq!(String::from_utf8(capture.0.take()).unwrap(), "\x1b[?25h");
    }

    /// The screen's frames and what a terminal is sent for them, and the
    /// cursor ratatui's own test backend says the same frames leave.
    struct Watch {
        terminal: Terminal<CursorBackend<Capture>>,
        capture: Capture,
        reference: Terminal<TestBackend>,
        replay: Replay,
    }

    impl Watch {
        fn new(width: u16, height: u16) -> Self {
            let (terminal, capture) = terminal(width, height);
            Self {
                terminal,
                capture,
                reference: Terminal::new(TestBackend::new(width, height)).unwrap(),
                replay: Replay::new(),
            }
        }

        fn resize(&mut self, width: u16, height: u16) {
            self.terminal
                .resize(Rect::new(0, 0, width, height))
                .unwrap();
            self.reference.backend_mut().resize(width, height);
        }

        /// Draws `frames` frames of `app`, as the animation does while the
        /// agent works. The status line's time moves, and so do the spinner
        /// and the highlight with the clock. At every character the
        /// terminal is sent, the cursor is hidden, or where the input box
        /// puts it, or where it was before the frame. It ends where the box
        /// puts it.
        fn frames(&mut self, label: &str, app: &mut App, frames: usize) {
            for frame in 0..frames {
                if app.working_since.is_some() {
                    app.working_since = Some(Instant::now() - Duration::from_secs(frame as u64));
                }
                std::thread::sleep(Duration::from_millis(12));
                let before = self.replay.shown();
                self.terminal.draw(|f| screen::draw(f, app)).unwrap();
                self.reference.draw(|f| screen::draw(f, app)).unwrap();
                self.replay.feed(&self.capture.0.take());
                let wanted = self
                    .reference
                    .backend()
                    .cursor_visible()
                    .then(|| self.reference.backend().cursor_position());
                let at = format!("{label}, frame {frame}");
                assert_eq!(self.replay.shown(), wanted, "{at}");
                for (place, drawn) in self.replay.notes.drain(..) {
                    assert!(!drawn, "{at}, a cell drawn at the visible cursor");
                    assert!(
                        Some(place) == wanted || Some(place) == before,
                        "{at}, the cursor shows at {place:?}"
                    );
                }
            }
        }

        /// The screen's rows as text.
        fn rows(&self) -> Vec<String> {
            let buffer = self.reference.backend().buffer();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol().to_string())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect()
        }
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.key(KeyEvent::new(code, modifiers));
    }

    fn mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        app.mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }

    /// While the agent works, the screen is drawn again and again, and the
    /// terminal's cursor is on neither the Working… line nor the status
    /// line at any point of what it is sent. It shows in the input box
    /// alone, in every view and state.
    #[test]
    fn the_cursor_shows_in_the_input_box_alone_while_the_screen_moves() {
        let mut app = app();
        // A turn began, a command ran, and the agent waits on its model.
        let mut user = Item::new(Kind::User, "Run the checks");
        user.delivery = Some(Delivery::Turn);
        let mut call = Item::new(Kind::Call, "$ cargo test");
        call.status = Some(CallStatus::Ok);
        let result = Item::new(Kind::Result, "12 passed");
        for (id, mut item) in [user, call, result].into_iter().enumerate() {
            item.id = id as u64 + 1;
            app.update(Update::Item(item), Activity::default);
        }
        let mut watch = Watch::new(100, 30);
        watch.frames("idle", &mut app, 3);

        let mut activity = Activity {
            reporting: false,
            working: true,
            since: Some(0),
            question: None,
        };
        app.set_activity(activity.clone());
        watch.frames("working", &mut app, 6);
        let rows = watch.rows();
        assert!(
            rows.iter().any(|row| row.ends_with(" Working…")),
            "{rows:#?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("working")),
            "the status line shows: {rows:#?}"
        );

        for view in [View::Quiet, View::Messages, View::Full] {
            app.view = view;
            watch.frames(view.name(), &mut app, 4);
        }

        for text in ["What next?", "\nsecond line", " and a third"] {
            for c in text.chars() {
                if c == '\n' {
                    press(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL);
                } else {
                    press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
                }
            }
            watch.frames("typing", &mut app, 2);
        }
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        watch.frames("a moved cursor", &mut app, 2);

        for id in ["r1", "r2"] {
            app.update(
                Update::Worker(worker(id, WorkerState::Running)),
                Activity::default,
            );
        }
        app.set_activity(activity.clone());
        watch.frames("workers listed", &mut app, 4);
        app.watch(Some("r1".into()));
        watch.frames("a worker's view", &mut app, 4);
        app.list_focus = Some(1);
        watch.frames("the list focused", &mut app, 3);
        assert_eq!(watch.replay.shown(), None);
        app.list_focus = None;
        app.watch(None);
        watch.frames("back in the main view", &mut app, 2);

        for press_ in ["/", "w"] {
            press(
                &mut app,
                KeyCode::Char(press_.chars().next().unwrap()),
                KeyModifiers::NONE,
            );
            watch.frames("the command menu", &mut app, 2);
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        activity.question = Some(Question {
            id: 1,
            text: "Tabs or spaces?".into(),
        });
        app.set_activity(activity.clone());
        watch.frames("a question", &mut app, 4);
        activity.question = None;
        app.set_activity(activity);

        let left = MouseButton::Left;
        mouse(&mut app, MouseEventKind::Down(left), 4, 1);
        for column in [10, 20, 30] {
            mouse(&mut app, MouseEventKind::Drag(left), column, 1);
            watch.frames("a selection", &mut app, 2);
        }
        mouse(&mut app, MouseEventKind::Up(left), 30, 1);
        watch.frames("a selection made", &mut app, 2);

        for overlay in [
            Overlay::Quit {
                choice: 0,
                dont_ask: false,
            },
            Overlay::Settings {
                row: 0,
                picker: None,
            },
        ] {
            app.overlay = Some(overlay);
            watch.frames("a popup", &mut app, 4);
            assert_eq!(watch.replay.shown(), None);
            app.overlay = None;
            watch.frames("the popup closed", &mut app, 2);
        }

        for (width, height) in [(60, 20), (140, 40), (20, 5), (100, 30)] {
            watch.resize(width, height);
            watch.frames(&format!("{width} by {height}"), &mut app, 3);
        }
    }
}

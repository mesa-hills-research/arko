//! Debug snapshots, in builds with the `dbg-snapshot` feature, for testing a
//! build before release. Ctrl+S saves the frame on the screen, and
//! `/snapshot [NOTE]` the frame drawn once the command has run, as text in
//! `snapshots/` in Arko's folder. Whoever reads `screen.txt` then reads the
//! characters the screen showed, cell for cell, `screen.styled` adds their
//! colours and attributes, and `meta.json` says where the cursor was.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use serde::Serialize;
use tuiframe::buffer::Buffer;
use tuiframe::{CompletedFrame, CursorShape};

use super::app::{App, Effect};
use super::overlay::Overlay;
use crate::remote::Level;

/// The frame the terminal took last, its cells and its cursor.
#[derive(Debug, Clone)]
pub struct Drawn {
    pub buffer: Buffer,
    /// Where the frame put the cursor, `None` when it hid it.
    pub cursor: Option<tuiframe::Cursor>,
}

impl Default for Drawn {
    fn default() -> Self {
        Self {
            buffer: Buffer::empty(tuiframe::Rect::ZERO),
            cursor: None,
        }
    }
}

impl Drawn {
    /// Keeps `frame`, which the terminal took.
    pub fn keep(&mut self, frame: &CompletedFrame) {
        self.buffer.clone_from(frame.buffer());
        self.cursor = frame.cursor();
    }
}

/// A frame of the view as text, with what the view showed with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// The frame, for `screen.txt` ([`screen_text`]).
    pub screen: String,
    /// The frame with its colours, attributes and cursor, for
    /// `screen.styled`, in the screen goldens' format.
    pub styled: String,
    pub meta: Meta,
}

/// What `meta.json` holds. The executable's facts are read as it is saved.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Meta {
    /// When the snapshot was taken, which names its folder.
    pub time: DateTime<Local>,
    pub note: Option<String>,
    pub version: &'static str,
    /// `debug` or `release`.
    pub build: &'static str,
    pub executable: Option<PathBuf>,
    pub executable_modified: Option<DateTime<Local>>,
    pub terminal: Terminal,
    pub theme: String,
    /// `Full`, `Quiet` or `Messages`.
    pub view: String,
    /// The menu, picker or other overlay open over the transcript.
    pub overlay: Option<&'static str>,
    /// The command menu shows above the input box.
    pub command_menu: bool,
    /// What takes the keys.
    pub focus: &'static str,
    /// The terminal window has the focus, as far as it says.
    pub window_focused: bool,
    /// The cursor in the input box, by row and column of its text.
    pub input_cursor: Option<Cursor>,
    /// The terminal's cursor, as the frame left it.
    pub screen_cursor: ScreenCursor,
    pub transcript: Scroll,
    pub session_id: Option<String>,
    pub session_folder: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Terminal {
    pub columns: u16,
    pub rows: u16,
    pub term: Option<String>,
    pub term_program: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
}

/// The terminal's cursor, its cell and its shape while it shows.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ScreenCursor {
    pub visible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row: Option<u16>,
    /// `default` (the user's own), `block`, `underline` or `bar`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape: Option<&'static str>,
}

impl ScreenCursor {
    fn of(cursor: Option<tuiframe::Cursor>) -> Self {
        Self {
            visible: cursor.is_some(),
            column: cursor.map(|cursor| cursor.position.x),
            row: cursor.map(|cursor| cursor.position.y),
            shape: cursor.map(|cursor| match cursor.shape {
                CursorShape::Default => "default",
                CursorShape::Block => "block",
                CursorShape::Underline => "underline",
                CursorShape::Bar => "bar",
            }),
        }
    }
}

/// Where the transcript is scrolled to.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Scroll {
    pub follows_bottom: bool,
    /// The key of the block at the top of the screen, when scrolled up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_block: Option<u64>,
    /// How many of that block's lines are above the screen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_block_lines_above: Option<usize>,
}

/// The text of `buffer`, one line per row at its full width with trailing
/// spaces kept, each cell's symbol as drawn. A wide character shows once,
/// so the cell it covers adds nothing.
pub fn screen_text(buffer: &Buffer) -> String {
    let mut text = super::buffer_lines(buffer).join("\n");
    text.push('\n');
    text
}

impl App {
    /// A snapshot of the frame drawn last, with what the view shows now and
    /// `note` (none when empty).
    pub fn snapshot(&self, note: &str) -> Snapshot {
        let Drawn { buffer, cursor } = &self.drawn_frame;
        let area = buffer.area();
        let anchor = self.transcript.reading_position();
        let meta = Meta {
            time: Local::now(),
            note: (!note.is_empty()).then(|| note.to_string()),
            version: env!("CARGO_PKG_VERSION"),
            build: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            executable: None,
            executable_modified: None,
            terminal: Terminal {
                columns: area.width,
                rows: area.height,
                term: std::env::var("TERM").ok(),
                term_program: std::env::var("TERM_PROGRAM").ok(),
            },
            theme: self.theme.name.clone(),
            view: format!("{:?}", self.view),
            overlay: self.overlay.as_ref().map(overlay_name),
            command_menu: !self.menu_items().is_empty(),
            focus: self.focus(),
            window_focused: self.focused,
            input_cursor: (self.input_width > 0).then(|| {
                let (row, column) = self.editor.position(self.input_width);
                Cursor { row, column }
            }),
            screen_cursor: ScreenCursor::of(*cursor),
            transcript: Scroll {
                follows_bottom: anchor.is_none(),
                top_block: anchor.map(|(key, _)| key),
                top_block_lines_above: anchor.map(|(_, lines)| lines),
            },
            session_id: self
                .info
                .session_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            session_folder: self.info.session_dir.clone(),
        };
        Snapshot {
            screen: screen_text(buffer),
            styled: super::golden::styled(buffer, cursor.map(|cursor| cursor.position)),
            meta,
        }
    }

    /// Says in the transcript where the snapshot went and copies its
    /// folder's path, or says what failed.
    pub fn snapshot_saved(&mut self, saved: Result<PathBuf, String>) {
        match saved {
            Ok(folder) => {
                let path = folder.display().to_string();
                self.effects_push(Effect::Notice(
                    Level::Info,
                    format!("Saved a snapshot in {path}"),
                ));
                self.effects_push(Effect::Copy {
                    text: path,
                    what: "the snapshot's folder".into(),
                });
            }
            Err(why) => self.effects_push(Effect::Notice(Level::Error, why)),
        }
    }

    /// What takes the keys, in the order [`App::key`] offers them.
    fn focus(&self) -> &'static str {
        if self.overlay.is_some() {
            "overlay"
        } else if self.tray.focused {
            "approval tray"
        } else if self.panel.open {
            "answer panel"
        } else if self.list_focus.is_some() {
            "agent list"
        } else if self.transcript.focus.is_some() {
            "transcript rows"
        } else {
            "input box"
        }
    }
}

fn overlay_name(overlay: &Overlay) -> &'static str {
    match overlay {
        Overlay::Help { .. } => "help",
        Overlay::Settings {
            picker: Some(_), ..
        } => "settings, theme picker",
        Overlay::Settings { .. } => "settings",
        Overlay::Remote { .. } => "web page links",
        Overlay::Model(_) => "model picker",
        Overlay::Keys(_) => "keys menu",
        Overlay::OpenRouter(_) => "OpenRouter menu",
        Overlay::Names(_) => "session names menu",
        Overlay::Resume(_) => "resume picker",
        Overlay::Quit { .. } => "quit question",
        Overlay::AskOff { .. } => "ask-mcp off question",
        Overlay::Download { .. } => "download question",
    }
}

/// [`save`], on a thread of the blocking pool, since the view's runtime has
/// only the view's thread.
pub async fn save_off_thread(root: PathBuf, snapshot: Snapshot) -> Result<PathBuf, String> {
    tokio::task::spawn_blocking(move || save(&root, snapshot))
        .await
        .unwrap_or_else(|error| Err(format!("Couldn't save the snapshot ({error})")))
}

/// Saves `snapshot` in a new folder in `snapshots` in Arko's folder `root`,
/// named for when it was taken, and on Unix points `snapshots/latest` at it.
/// Returns the folder, or a line saying what failed.
pub fn save(root: &Path, mut snapshot: Snapshot) -> Result<PathBuf, String> {
    let failed = |path: &Path, error: std::io::Error| {
        format!("Couldn't save the snapshot in {} ({error})", path.display())
    };
    let snapshots = root.join("snapshots");
    std::fs::create_dir_all(&snapshots).map_err(|error| failed(&snapshots, error))?;
    let (name, folder) =
        new_folder(&snapshots, snapshot.meta.time).map_err(|error| failed(&snapshots, error))?;
    let meta = &mut snapshot.meta;
    meta.executable = std::env::current_exe().ok();
    meta.executable_modified = meta
        .executable
        .as_ref()
        .and_then(|path| path.metadata().ok()?.modified().ok())
        .map(DateTime::from);
    let json = serde_json::to_string_pretty(meta).map_err(|error| error.to_string())? + "\n";
    let files = [
        ("screen.txt", &snapshot.screen),
        ("screen.styled", &snapshot.styled),
        ("meta.json", &json),
    ];
    for (file, text) in files {
        let path = folder.join(file);
        std::fs::write(&path, text).map_err(|error| failed(&path, error))?;
    }
    #[cfg(unix)]
    link_latest(&snapshots, &name).map_err(|error| {
        format!(
            "Saved a snapshot in {}, but couldn't point latest at it ({error})",
            folder.display()
        )
    })?;
    #[cfg(not(unix))]
    let _ = name;
    Ok(folder)
}

/// Makes a new folder in `snapshots` named for `time`, as `20261110-143022`,
/// then `20261110-143022-02` and on for more in the same second, so the
/// names sort by time. Returns its name and path.
fn new_folder(snapshots: &Path, time: DateTime<Local>) -> std::io::Result<(String, PathBuf)> {
    let stamp = time.format("%Y%m%d-%H%M%S").to_string();
    for count in 1..100 {
        let name = match count {
            1 => stamp.clone(),
            _ => format!("{stamp}-{count:02}"),
        };
        let folder = snapshots.join(&name);
        match std::fs::create_dir(&folder) {
            Ok(()) => return Ok((name, folder)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "99 snapshots this second already",
    ))
}

/// Points `snapshots/latest` at the folder `name`, through a link made
/// beside it and renamed over it, so `latest` is never missing.
#[cfg(unix)]
fn link_latest(snapshots: &Path, name: &str) -> std::io::Result<()> {
    let temporary = snapshots.join(format!(".latest-{name}"));
    let _ = std::fs::remove_file(&temporary);
    std::os::unix::fs::symlink(name, &temporary)?;
    std::fs::rename(&temporary, snapshots.join("latest"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::app;
    use tuiframe::event::{KeyCode, KeyEvent, KeyModifiers};
    use tuiframe::layout::Rect;
    use tuiframe::style::Style;
    use tuiframe::testing::TestTerminal;

    /// Draws a frame of `app` and keeps it, as the view's loop does.
    fn draw(terminal: &mut TestTerminal, app: &mut App) {
        let drawn = terminal
            .draw(|frame| super::super::screen::draw(frame, app))
            .unwrap();
        app.drawn_frame.keep(&drawn);
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.key(KeyEvent::new(code, modifiers));
    }

    fn meta_json(folder: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(folder.join("meta.json")).unwrap()).unwrap()
    }

    #[test]
    fn the_text_has_every_cell_as_drawn_and_a_wide_character_once() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
        buffer.set_string(0, 0, "┌──┐", Style::default());
        buffer.set_string(0, 1, "│日✓│", Style::default());
        buffer.set_string(0, 2, "ok", Style::default());
        assert_eq!(screen_text(&buffer), "┌──┐    \n│日✓│   \nok      \n");
    }

    /// Ctrl+S saves the frame on the screen with the help open over it, and
    /// leaves the help, the box and the next frame as they were.
    #[test]
    fn ctrl_s_saves_the_frame_on_the_screen_and_changes_nothing() {
        let mut app = app();
        let mut terminal = crate::tui::golden::terminal(80, 24);
        for c in "draft".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.overlay = Some(Overlay::Help { scroll: 0 });
        draw(&mut terminal, &mut app);
        let shown = screen_text(&app.drawn_frame.buffer);

        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(app.overlay, Some(Overlay::Help { scroll: 0 }));
        assert_eq!(app.editor.text(), "draft");
        let effects = app.take_effects();
        let [Effect::Snapshot(snapshot)] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        assert_eq!(snapshot.screen, shown);
        assert!(shown.contains("Keys"));
        draw(&mut terminal, &mut app);
        assert_eq!(screen_text(&app.drawn_frame.buffer), shown);

        let temp = tempfile::tempdir().unwrap();
        let folder = save(temp.path(), (**snapshot).clone()).unwrap();
        assert_eq!(folder.parent().unwrap(), temp.path().join("snapshots"));
        assert_eq!(
            std::fs::read_to_string(folder.join("screen.txt")).unwrap(),
            shown
        );
        // The help has the keys, so the terminal's cursor is hidden.
        let styled = std::fs::read_to_string(folder.join("screen.styled")).unwrap();
        assert_eq!(styled, snapshot.styled);
        assert!(
            styled.starts_with("size 80 24\ncursor 0 0 hidden\n"),
            "{styled}"
        );
        assert!(
            styled.lines().any(|line| line.starts_with("style ")),
            "{styled}"
        );
        let meta = meta_json(&folder);
        assert_eq!(
            meta["screen_cursor"],
            serde_json::json!({ "visible": false })
        );
        assert_eq!(meta["overlay"], "help");
        assert_eq!(meta["focus"], "overlay");
        assert_eq!(meta["note"], serde_json::Value::Null);
        assert_eq!(meta["terminal"]["columns"], 80);
        assert_eq!(meta["version"], env!("CARGO_PKG_VERSION"));
        assert!(meta["executable"].is_string());
    }

    /// `/snapshot NOTE` saves the frame drawn once the box has cleared,
    /// with its words as the note.
    #[test]
    fn the_command_saves_the_next_frame_with_its_note() {
        let mut app = app();
        let mut terminal = crate::tui::golden::terminal(80, 24);
        for c in "/snapshot gap after the tick".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        draw(&mut terminal, &mut app);
        assert!(screen_text(&app.drawn_frame.buffer).contains("/snapshot gap"));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.editor.is_empty());
        assert_eq!(
            app.take_effects(),
            [Effect::SnapshotNextFrame("gap after the tick".into())]
        );

        // The view's loop draws the frame, then saves it.
        draw(&mut terminal, &mut app);
        let snapshot = app.snapshot("gap after the tick");
        assert!(!snapshot.screen.contains("/snapshot"));
        // The cursor is in the empty box, where the terminal shows it.
        let shown = terminal.cursor().unwrap().position;
        let temp = tempfile::tempdir().unwrap();
        let folder = save(temp.path(), snapshot).unwrap();
        let meta = meta_json(&folder);
        assert_eq!(meta["note"], "gap after the tick");
        assert_eq!(
            meta["screen_cursor"],
            serde_json::json!({
                "visible": true,
                "column": shown.x,
                "row": shown.y,
                "shape": "default",
            })
        );
        assert_eq!(
            meta["input_cursor"],
            serde_json::json!({ "row": 0, "column": 0 })
        );
        let styled = std::fs::read_to_string(folder.join("screen.styled")).unwrap();
        let line = format!("cursor {} {} visible default\n", shown.x, shown.y);
        assert!(styled.contains(&line), "{styled}");

        // Its folder goes to the transcript and the clipboard.
        app.snapshot_saved(Ok(folder.clone()));
        let path = folder.display().to_string();
        assert_eq!(
            app.take_effects(),
            [
                Effect::Notice(Level::Info, format!("Saved a snapshot in {path}")),
                Effect::Copy {
                    text: path,
                    what: "the snapshot's folder".into()
                }
            ]
        );
    }

    #[test]
    fn two_snapshots_in_one_second_get_folders_that_sort_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = app().snapshot("");
        let first = save(temp.path(), snapshot.clone()).unwrap();
        let second = save(temp.path(), snapshot).unwrap();
        assert_ne!(first, second);
        assert!(first.file_name() < second.file_name());
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(temp.path().join("snapshots/latest")).unwrap(),
            second.file_name().unwrap()
        );
    }
}

//! The full-screen view: Arko's front end when standard input and output
//! are a terminal (`--repl` keeps the line prompt). It is a client of the
//! session as the web page is: it shows the [`Feed`](remote::Feed) and the
//! [`Meter`](remote::Meter)'s figures, and what the user types reaches the
//! session as a line typed at the prompt does (the terminal's own input,
//! so `/exit` and `/remote` work here and the feed says it came from the
//! terminal).
//!
//! Four rules shape it: the transcript is what the user is there for, and
//! everything else is small and quiet; nothing moves under the reader;
//! what is unusual shows, not what is normal; and the keys are those of
//! chat apps, pagers and readline.

mod app;
mod clipboard;
mod input;
mod keys_menu;
mod markdown;
mod model_picker;
mod overlay;
mod prefs;
mod resume_picker;
mod screen;
pub(crate) mod theme;
mod transcript;
mod wrap;

use std::io::{IsTerminal, Stdout, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{Command, cursor, execute, queue};
use futures_util::{FutureExt, StreamExt};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::glyphs;
use crate::remote;
use crate::ui::{self, Input, Output};
use app::{App, Effect, Info};
pub use transcript::Banner;

/// How long Enter waits to see if more keys follow it: keys that come
/// together are a paste the terminal didn't mark (the classic Windows
/// console, where pastes arrive as keys), whose line breaks must not send.
const ENTER_WAIT: Duration = Duration::from_millis(25);
/// A paste of keys ends when none came for this long.
const BURST_GAP: Duration = Duration::from_millis(40);
/// Frames are drawn at most this often while output streams.
const FRAME: Duration = Duration::from_millis(33);

/// What the view needs of the session.
pub struct Setup {
    pub feed: remote::Feed,
    pub meter: Arc<dyn remote::Meter>,
    /// Where the user's input goes: the session reads it as the terminal's.
    pub input: mpsc::UnboundedSender<Input>,
    /// Arko's folder: the history, the view's choices, the user's themes.
    pub root: PathBuf,
    pub session_dir: PathBuf,
    pub workspace: PathBuf,
    pub handover_at: usize,
    /// The agents' thinking shows whole (`--show-thinking`), else folded.
    pub show_thinking: bool,
    /// The session's facts, for the banner.
    pub banner: Banner,
    /// The API keys, which the keys menu adds and removes.
    pub keys: Arc<crate::keys::Keys>,
    /// The jobs in the background to ask about: quitting would stop them.
    pub quit_asks: mpsc::UnboundedReceiver<ui::QuitAsk>,
}

/// The view while it runs.
pub struct Screen {
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Option<String>>,
    output: Arc<Output>,
}

/// Whether the full-screen view can run: standard input and output are a
/// terminal that can draw (not TERM=dumb).
pub fn available() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var("TERM").map_or(true, |term| term != "dumb")
}

/// Takes the screen, and shows the session there from now on: `output`'s
/// lines go to the feed. When the terminal can't be set up, the line
/// prompt reads the input instead, and `None` is returned.
pub fn start(setup: Setup, output: &Arc<Output>) -> Option<Screen> {
    let history = setup.root.join("history.txt");
    // Settled first: its warning, if any, prints before the screen is taken.
    glyphs::symbols();
    let catalog = theme::Catalog::load(Some(&setup.root.join("themes")));
    let depth = theme::depth();
    let terminal = match open() {
        Ok(terminal) => terminal,
        Err(error) => {
            close();
            eprintln!(
                "arko: the full-screen view couldn't start ({error}), so Arko uses the line prompt"
            );
            if let Some(printer) =
                ui::spawn_input(history, setup.input.clone(), Some(setup.keys.clone()))
            {
                output.attach(printer);
            }
            return None;
        }
    };
    output.to_feed(setup.feed.clone());
    for warning in &catalog.warnings {
        setup.feed.notice(remote::Level::Warn, warning);
    }
    let (snapshot, updates) = setup.feed.subscribe();
    let workspace = setup
        .workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| setup.workspace.display().to_string());
    let info = Info {
        workspace,
        handover_at: setup.handover_at,
        session_dir: setup.session_dir.clone(),
        prefs: Some(setup.root.join("tui.json")),
    };
    let editor = input::Editor::new(input::History::load(Some(history)));
    let mut app = App::new(snapshot, editor, info, catalog, depth);
    app.transcript.banner = Some(setup.banner.clone());
    app.transcript.set_open_thinking(setup.show_thinking);
    app.shift_enter = terminal.1;
    if !app.mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    let (stop, stopped) = oneshot::channel();
    let input = setup.input.clone();
    let task = tokio::spawn(async move {
        let running = std::panic::AssertUnwindSafe(run(terminal.0, app, setup, updates, stopped));
        match running.catch_unwind().await {
            Ok(reply) => reply,
            Err(_) => {
                // The panic hook restored the terminal and said why; the
                // session ends as Ctrl-D ends it (it is saved).
                close();
                let _ = input.send(Input::Eof);
                None
            }
        }
    });
    Some(Screen {
        stop,
        task,
        output: output.clone(),
    })
}

impl Screen {
    /// Gives the screen back: the terminal as it was, and then the last
    /// reply printed where the user can scroll back to it.
    pub async fn close(self) {
        let _ = self.stop.send(());
        let reply = self.task.await.ok().flatten();
        // Whatever happened to the task, the terminal is restored.
        close();
        self.output.detach();
        if let Some(reply) = reply {
            let reply = glyphs::render(reply.trim()).into_owned();
            println!("{reply}\n");
        }
    }
}

/// Mouse reports for buttons, the wheel and drags (motion while a button is
/// down, to select text): not every motion, which crossterm's own command
/// asks for too (any-event tracking, crossterm/src/event.rs:329) and which
/// would wake the view at every move of the mouse.
struct EnableMouse;

impl Command for EnableMouse {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        // Buttons and the wheel, drags, in SGR coordinates.
        f.write_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        // The console reports mouse input records (crossterm's way).
        crossterm::event::EnableMouseCapture.execute_winapi()
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

/// The terminal made the view's: raw mode, the alternate screen, the
/// mouse, pastes and focus reported, and Shift+Enter told apart from
/// Enter where the terminal can (the kitty keyboard protocol). Returns the
/// terminal and whether Shift+Enter is reported.
fn open() -> std::io::Result<(Terminal<CrosstermBackend<Stdout>>, bool)> {
    crossterm::terminal::enable_raw_mode()?;
    OPEN.store(true, std::sync::atomic::Ordering::SeqCst);
    // A panic restores the terminal before its message prints (as
    // ratatui's init does, ratatui/src/init.rs:566).
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        close();
        previous(info);
    }));
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouse, EnableFocusChange)?;
    // Not in the classic Windows console without escape codes
    // (crossterm/src/event.rs:413-435): pastes come as keys there.
    let _ = execute!(stdout, EnableBracketedPaste);
    // The title is saved, to be put back (xterm's title stack; others
    // ignore it).
    if !cfg!(windows) {
        let _ = write!(stdout, "\x1b[22;0t");
    }
    // Asked once, before events are read: the answer is read as an event
    // (crossterm/src/terminal/sys/unix.rs:183-267); Windows answers no
    // (crossterm/src/terminal/sys/windows.rs:75), but its console's key
    // records carry Shift with Enter anyway.
    let enhanced = matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    );
    if enhanced {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    ENHANCED.store(enhanced, std::sync::atomic::Ordering::SeqCst);
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;
    Ok((terminal, enhanced || cfg!(windows)))
}

/// The keyboard protocol was pushed: it is popped as the view closes.
static ENHANCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The view has the terminal: [`close`] gives it back once.
static OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The terminal as it was: safe to call more than once, and from the panic
/// hook. (Only once: leaving the alternate screen again would put the
/// cursor back where the view started, over what was printed since.)
fn close() {
    if !OPEN.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let _ = crossterm::terminal::disable_raw_mode();
    let mut stdout = std::io::stdout();
    if ENHANCED.swap(false, std::sync::atomic::Ordering::SeqCst) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        stdout,
        DisableMouseCapture,
        DisableFocusChange,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        cursor::Show
    );
    if !cfg!(windows) {
        let _ = write!(stdout, "\x1b[23;0t");
    }
    let _ = stdout.flush();
}

/// A paste that came as keys: where it started in the box, and when it
/// ends unless more come.
struct Burst {
    start: usize,
    until: Instant,
}

/// Whether `event` types into the box: a character, or a plain Enter.
fn types(event: &Event) -> bool {
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
            KeyCode::Char(_) => !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
            KeyCode::Enter => key.modifiers.is_empty(),
            _ => false,
        },
        _ => false,
    }
}

/// The view's loop: terminal events, the feed's updates and the clock, until
/// `stop`. Returns the last reply.
async fn run(
    mut terminal: Terminal<CrosstermBackend<Stdout>>,
    mut app: App,
    mut setup: Setup,
    mut updates: broadcast::Receiver<remote::Update>,
    mut stop: oneshot::Receiver<()>,
) -> Option<String> {
    let mut events = EventStream::new();
    let mut events_open = true;
    let mut clock = tokio::time::interval(Duration::from_millis(500));
    let mut frames = tokio::time::interval(FRAME);
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Something changed: drawn at the next frame. Keys draw at once.
    let mut dirty = false;
    let mut draw_now = true;
    let mut title = String::new();
    let mut enter: Option<Instant> = None;
    let mut burst: Option<Burst> = None;
    app.set_reading(setup.meter.read());
    loop {
        if draw_now {
            draw(&mut terminal, &mut app);
            draw_now = false;
            dirty = false;
            let now = app.title();
            if now != title {
                let _ = execute!(std::io::stdout(), crossterm::terminal::SetTitle(&now));
                title = now;
            }
        }
        let enter_at = enter.unwrap_or_else(Instant::now);
        let preview_at = app.preview_due();
        let burst_at = burst
            .as_ref()
            .map_or_else(Instant::now, |burst| burst.until);
        let mut bell = false;
        tokio::select! {
            _ = &mut stop => break,
            Some(ask) = setup.quit_asks.recv() => {
                app.ask_quit(ask.jobs);
                draw_now = true;
            }
            event = events.next(), if events_open => {
                let Some(Ok(event)) = event else {
                    // The terminal is gone: as Ctrl-D at the prompt.
                    events_open = false;
                    let _ = setup.input.send(Input::Eof);
                    continue;
                };
                // What came together comes as one batch.
                let mut batch = vec![event];
                batch.extend(waiting(&mut events).await);
                handle(&mut app, batch, &mut enter, &mut burst);
                draw_now = true;
            }
            update = updates.recv() => {
                match update {
                    Ok(update) => {
                        bell |= app.update(update, || setup.feed.activity());
                        // Those waiting too, before the next frame.
                        while let Ok(update) = updates.try_recv() {
                            bell |= app.update(update, || setup.feed.activity());
                        }
                        // The web page just started says so in the feed:
                        // its links show at once.
                        if app.waits_for_page() {
                            app.set_reading(setup.meter.read());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let (snapshot, fresh) = setup.feed.subscribe();
                        updates = fresh;
                        app.reset(snapshot);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // (The feed outlives the view.)
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                    }
                }
                // Drawn at the next frame: streaming text doesn't redraw at
                // every chunk.
                dirty = true;
            }
            _ = frames.tick(), if dirty => draw_now = true,
            _ = clock.tick() => {
                let reading = setup.meter.read();
                let workers = app.list_ticks();
                // (The quit popup's jobs show how long they have run.)
                let quitting = matches!(app.overlay, Some(overlay::Overlay::Quit { .. }));
                if reading != app.reading || app.activity.working || workers || app.hint.is_some() || quitting {
                    app.set_reading(reading);
                    dirty = true;
                    // Yes stops the jobs the popup shows: drawn before the
                    // next key.
                    draw_now |= quitting;
                }
            }
            _ = tokio::time::sleep_until(preview_at.unwrap_or_else(Instant::now).into()), if preview_at.is_some() => {
                // The theme picker's selection has rested: its preview.
                if app.preview(Instant::now()) {
                    draw_now = true;
                }
            }
            _ = tokio::time::sleep_until(enter_at.into()), if enter.is_some() => {
                let batch = waiting(&mut events).await;
                if batch.is_empty() {
                    // No key followed the Enter: it was typed, and sends.
                    enter = None;
                    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                } else {
                    handle(&mut app, batch, &mut enter, &mut burst);
                }
                draw_now = true;
            }
            _ = tokio::time::sleep_until(burst_at.into()), if burst.is_some() => {
                let batch = waiting(&mut events).await;
                if batch.is_empty() {
                    if let Some(burst) = burst.take() {
                        app.editor.fold_paste(burst.start);
                    }
                } else {
                    handle(&mut app, batch, &mut enter, &mut burst);
                }
                draw_now = true;
            }
        }
        if bell {
            let _ = std::io::stdout().write_all(b"\x07");
        }
        for effect in app.take_effects() {
            match effect {
                Effect::Input(input) => {
                    let _ = setup.input.send(input);
                }
                Effect::Copy { text, what } => {
                    // The terminal's clipboard, and the system's: the status
                    // line says "Copied" only when the system's took it.
                    let mut stdout = std::io::stdout();
                    let _ = execute!(
                        stdout,
                        crossterm::clipboard::CopyToClipboard::to_clipboard_from(text.as_bytes())
                    );
                    app.say(match clipboard::copy(&text) {
                        clipboard::Copied::System => format!("Copied {what}"),
                        clipboard::Copied::Terminal => "Sent to the terminal's clipboard".into(),
                        clipboard::Copied::Failed(why) => why,
                    });
                }
                Effect::Mouse(on) => {
                    let _ = if on {
                        execute!(std::io::stdout(), EnableMouse)
                    } else {
                        execute!(std::io::stdout(), DisableMouseCapture)
                    };
                }
                Effect::Clear => {
                    let _ = terminal.clear();
                }
                Effect::AddKey { key, value } => {
                    let outcome = setup.keys.add(key, value);
                    app.keys_done(key, outcome);
                    // The models it makes reachable, for `/model`'s picker.
                    app.set_reading(setup.meter.read());
                }
                Effect::RemoveKey(key) => {
                    let outcome = setup.keys.remove(key);
                    app.keys_done(key, outcome);
                    app.set_reading(setup.meter.read());
                }
            }
            draw_now = true;
        }
    }
    let reply = app.transcript.last_reply().map(str::to_string);
    close();
    reply
}

/// The events already waiting, without waiting for more. (Polled in this
/// task, not with a no-op waker: the stream registers the waker of its
/// first pending poll only, so a no-op one would leave the next keys unread
/// until something else woke the loop, crossterm/src/event/stream.rs:112-131.)
async fn waiting(events: &mut EventStream) -> Vec<Event> {
    let mut batch = Vec::new();
    while let Ok(Some(Ok(event))) = tokio::time::timeout(Duration::ZERO, events.next()).await {
        batch.push(event);
    }
    batch
}

/// A batch of terminal events. Keys that came together are a paste (where
/// the terminal doesn't mark pastes): their line breaks are line breaks,
/// not sends. A lone Enter waits a moment for what follows it.
fn handle(
    app: &mut App,
    batch: Vec<Event>,
    enter: &mut Option<Instant>,
    burst: &mut Option<Burst>,
) {
    let now = Instant::now();
    let typed = batch.iter().filter(|event| types(event)).count();
    // (Three keys or more: two quick keys of a person's can come in one
    // read while the view is busy drawing.)
    let mut pasting = typed >= 3 || burst.is_some();
    if enter.take().is_some() {
        if typed > 0 {
            // The Enter that waited was a paste's.
            if burst.is_none() {
                *burst = Some(Burst {
                    start: app.editor.cursor(),
                    until: now,
                });
            }
            app.newline();
            pasting = true;
        } else {
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
    }
    if pasting && burst.is_none() {
        *burst = Some(Burst {
            start: app.editor.cursor(),
            until: now,
        });
    }
    let typing_box = app.overlay.is_none() && app.list_focus.is_none();
    for event in batch {
        match event {
            Event::Key(key)
                if typing_box
                    && key.code == KeyCode::Enter
                    && key.modifiers.is_empty()
                    && key.kind != KeyEventKind::Release =>
            {
                if pasting {
                    app.newline();
                } else {
                    *enter = Some(now + ENTER_WAIT);
                }
            }
            Event::Key(key) => app.key(key),
            Event::Paste(text) => app.paste(&text),
            Event::Mouse(mouse) => {
                if app.mouse {
                    app.mouse(mouse);
                }
            }
            Event::FocusGained => app.focused = true,
            Event::FocusLost => app.focused = false,
            Event::Resize(..) => {}
        }
    }
    if pasting && let Some(burst) = burst {
        // From now: a long batch takes a while in a debug build.
        burst.until = Instant::now() + BURST_GAP;
    }
}

/// A frame, drawn as one update where the terminal can (synchronized
/// output, CSI ?2026: crossterm/src/terminal.rs:433-450; others ignore it),
/// so it doesn't flicker.
fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) {
    let _ = queue!(terminal.backend_mut(), BeginSynchronizedUpdate);
    let _ = terminal.draw(|frame| screen::draw(frame, app));
    let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventState, MouseEvent};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    /// A paste that comes as keys doesn't send at its line breaks, and a
    /// long one folds into a chip once it ends; a lone Enter sends after
    /// its wait.
    #[test]
    fn keys_that_come_together_are_a_paste() {
        let mut app = app::tests::app();
        let (mut enter, mut burst) = (None, None);
        let paste: Vec<Event> = "one\ntwo"
            .chars()
            .map(|c| {
                if c == '\n' {
                    key(KeyCode::Enter)
                } else {
                    key(KeyCode::Char(c))
                }
            })
            .collect();
        handle(&mut app, paste, &mut enter, &mut burst);
        assert_eq!(app.editor.text(), "one\ntwo");
        assert!(enter.is_none() && burst.is_some());
        assert!(app.take_effects().is_empty());
        burst = None;
        // Typed, one key at a time: the Enter waits, then sends when
        // nothing follows (the loop's timer).
        handle(&mut app, vec![key(KeyCode::Enter)], &mut enter, &mut burst);
        assert!(enter.is_some());
        handle(
            &mut app,
            vec![Event::Mouse(MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })],
            &mut enter,
            &mut burst,
        );
        let sent = app.take_effects();
        assert_eq!(sent, [Effect::Input(Input::Line("one\ntwo".into()))]);
        // An Enter a key follows at once was a paste's.
        handle(
            &mut app,
            vec![key(KeyCode::Char('a'))],
            &mut enter,
            &mut burst,
        );
        handle(&mut app, vec![key(KeyCode::Enter)], &mut enter, &mut burst);
        handle(
            &mut app,
            vec![key(KeyCode::Char('b'))],
            &mut enter,
            &mut burst,
        );
        assert_eq!(app.editor.text(), "a\nb");
        assert!(app.take_effects().is_empty());
    }
}

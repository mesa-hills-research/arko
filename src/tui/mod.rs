//! The full-screen view, Arko's front end when standard input and output
//! are a terminal (`--repl` keeps the line prompt). It is a client of the
//! session as the web page is. It shows the [`Feed`](remote::Feed) and the
//! [`Meter`](remote::Meter)'s figures, and what the user types reaches the
//! session as a line typed at the prompt does (the terminal's own input,
//! so `/exit` and `/remote` work here and the feed says it came from the
//! terminal).
//!
//! Four rules shape it. The transcript is what the user is there for, and
//! everything else is small and quiet. Nothing moves under the reader.
//! What is unusual shows, not what is normal. The keys are those of
//! chat apps, pagers and readline.

// While it has the terminal, only its writer writes to it (clippy.toml).
#![cfg_attr(not(test), deny(clippy::disallowed_methods, clippy::disallowed_macros))]

mod app;
mod approvals;
mod backend;
mod cache;
mod clipboard;
mod dictation;
mod input;
mod keys_menu;
mod markdown;
mod model_picker;
mod motion;
mod names_menu;
mod openrouter_menu;
mod overlay;
mod prefs;
mod questions;
mod resume_picker;
mod screen;
mod select;
pub(crate) mod theme;
mod transcript;
mod working;
mod wrap;
mod writer;

use std::io::{IsTerminal, Write};
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
use tokio::sync::{mpsc, oneshot};

use crate::glyphs;
use crate::remote;
use crate::ui::{self, Input, Output};
use app::{App, Effect, Info, Sending};
use backend::CursorBackend;
pub use transcript::Banner;
use writer::Writer;

/// How long Enter waits to see if more keys follow it. Keys that come
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
    /// The session's details, for the words dictation favours.
    pub details: Arc<crate::details::Details>,
    pub meter: Arc<dyn remote::Meter>,
    /// Where the user's input goes. The session reads it as the terminal's.
    pub input: mpsc::UnboundedSender<Input>,
    /// Where what the user sends goes, each with where its verdict comes
    /// ([`crate::Request::Send`]).
    pub requests: mpsc::UnboundedSender<crate::Request>,
    /// Arko's folder, with the history, the view's choices, the user's themes.
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
    /// The jobs in the background to ask about, because quitting would stop them.
    pub quit_asks: mpsc::UnboundedReceiver<ui::QuitAsk>,
}

/// The view while it runs.
pub struct Screen {
    stop: oneshot::Sender<()>,
    /// The last reply, once the view's loop ends.
    reply: oneshot::Receiver<Option<String>>,
    output: Arc<Output>,
}

/// Whether the full-screen view can run, which it does when standard input and output are a
/// terminal that takes escape codes (not TERM=dumb, nor the classic Windows console without
/// them).
#[allow(clippy::disallowed_methods, reason = "before the writer starts")]
pub fn available() -> bool {
    // The classic Windows console without escape codes would carry out some
    // of the view's commands at once, ahead of the text its writer still
    // holds, so the line prompt runs there.
    #[cfg(windows)]
    if !crossterm::ansi_support::supports_ansi() {
        return false;
    }
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var("TERM").map_or(true, |term| term != "dumb")
}

/// Takes the screen, and shows the session there from now on. `output`'s
/// lines go to the feed. When the terminal can't be set up, the line
/// prompt reads the input instead, and `None` is returned.
pub fn start(setup: Setup, output: &Arc<Output>) -> Option<Screen> {
    let history = setup.root.join("history.txt");
    // Settled first, so its warning, if any, prints before the screen is taken.
    glyphs::symbols();
    let catalog = theme::Catalog::load(Some(&setup.root.join("themes")));
    let depth = theme::depth();
    let terminal = match open(setup.session_dir.join(crate::session::LOG_FILE)) {
        Ok(terminal) => terminal,
        Err(error) => {
            say_it_didnt_start(&error);
            if let Some(printer) = ui::spawn_input(
                setup.root.clone(),
                setup.input.clone(),
                Some(setup.keys.clone()),
                Some(setup.feed.clone()),
                Some(setup.details.clone()),
            ) {
                output.attach(printer);
            }
            return None;
        }
    };
    output.to_feed(setup.feed.clone());
    for warning in &catalog.warnings {
        setup.feed.notice(remote::Level::Warn, warning);
    }
    let (snapshot, updates) = setup.feed.follow();
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
        root: Some(setup.root.clone()),
    };
    let editor = input::Editor::new(input::History::load(Some(history)));
    let mut app = App::new(snapshot, editor, info, catalog, depth);
    app.transcript.banner = Some(setup.banner.clone());
    // The logins the session signs in with, `--codex-auth` included.
    app.login_paths = {
        let keys = setup.keys.clone();
        std::sync::Arc::new(move || keys.login_paths())
    };
    app.transcript.set_open_thinking(setup.show_thinking);
    let (mut terminal, screen, shift_enter) = terminal;
    app.shift_enter = shift_enter;
    if !app.mouse {
        let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
    }
    let (stop, stopped) = oneshot::channel();
    let input = setup.input.clone();
    let ended = input.clone();
    let session = tokio::runtime::Handle::current();
    let view = move || async move {
        let running = std::panic::AssertUnwindSafe(run(
            terminal, screen, app, setup, updates, stopped, session,
        ));
        match running.catch_unwind().await {
            Ok(reply) => reply,
            Err(_) => {
                // The panic hook restored the terminal and said why. The
                // session ends as Ctrl-D ends it (it is saved).
                close();
                let _ = ended.send(Input::Eof);
                None
            }
        }
    };
    let reply = match on_a_thread(crate::context::handle(), view) {
        Ok(reply) => reply,
        Err(error) => {
            let said = format!("arko: the full-screen view couldn't start ({error})\n");
            give_back(said.as_bytes());
            let _ = input.send(Input::Eof);
            oneshot::channel().1
        }
    };
    Some(Screen {
        stop,
        reply,
        output: output.clone(),
    })
}

/// Says why the full screen didn't start, on standard error, once the
/// terminal is as it was.
#[allow(clippy::disallowed_macros, reason = "the terminal is as it was")]
fn say_it_didnt_start(error: &std::io::Error) {
    eprintln!("arko: the full-screen view couldn't start ({error}), so Arko uses the line prompt");
}

/// The name of the view's thread.
const THREAD: &str = "arko-view";

/// Runs the future `view` makes on a thread of its own ([`THREAD`]), with a
/// runtime of its own, for the session of `context`. What the view waits on
/// (a lock, a slow call) then holds up no thread of the session's runtime,
/// and the session's runtime stuck holds up no frame of the view. The
/// future's output comes back on the receiver.
fn on_a_thread<F>(
    context: Option<Arc<crate::context::SessionContext>>,
    view: impl FnOnce() -> F + Send + 'static,
) -> std::io::Result<oneshot::Receiver<F::Output>>
where
    F: std::future::Future + 'static,
    F::Output: Send + 'static,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (done, output) = oneshot::channel();
    std::thread::Builder::new()
        .name(THREAD.into())
        .spawn(move || {
            crate::context::within(context, || {
                let _ = done.send(runtime.block_on(view()));
                // (What the view started that hasn't ended, such as a check
                // of the session names, holds up nothing.)
                runtime.shutdown_background();
            });
        })?;
    Ok(output)
}

impl Screen {
    /// Gives the screen back, with the terminal as it was, and then the last
    /// reply printed where the user can scroll back to it.
    pub async fn close(self) {
        let _ = self.stop.send(());
        let reply = self.reply.await.ok().flatten();
        // Whatever happened to the task, the terminal is restored, and the
        // reply follows on the terminal's own screen.
        let reply = reply
            .map(|reply| format!("{}\n\n", glyphs::render(reply.trim())))
            .unwrap_or_default();
        let given = give_back(reply.as_bytes());
        self.output.detach();
        // (What Arko prints next goes straight to the terminal, after this.)
        if let Some((writer, ticket)) = given {
            let _ = tokio::task::spawn_blocking(move || writer.wait(ticket, CLOSE_WAIT)).await;
        }
    }
}

/// Mouse reports for buttons, the wheel and drags (motion while a button is
/// down, to select text), not every motion, which crossterm's own command
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

/// The terminal made the view's, namely raw mode, the alternate screen, the
/// mouse, pastes and focus reported, and Shift+Enter told apart from
/// Enter where the terminal can (the kitty keyboard protocol). From then on
/// its writer ([`Writer`]) sends it everything, and its watchdog's lines go
/// to the log at `log`. Returns the terminal, another handle on its writer,
/// and whether Shift+Enter is reported. When it fails, the terminal is as it
/// was.
fn open(log: PathBuf) -> std::io::Result<(Terminal<CursorBackend<Writer>>, Writer, bool)> {
    crossterm::terminal::enable_raw_mode()?;
    // A panic that ends the view restores the terminal before its message
    // prints (as ratatui's init does, ratatui/src/init.rs:566). Another
    // thread's, which dictation's thread recovers from (and the speech
    // engine's workers, whose panics it rethrows there), goes to the log
    // and leaves the screen.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !ends_the_view(std::thread::current().name()) {
            log::error!("{info}");
            return;
        }
        close_and_wait();
        previous(info);
    }));
    // Asked once, before events are read, and before the writer starts,
    // since the question goes to the terminal and its answer comes back as
    // an event (crossterm/src/terminal/sys/unix.rs:183-267). Windows
    // answers no (crossterm/src/terminal/sys/windows.rs:75), but its
    // console's key records carry Shift with Enter anyway.
    #[allow(clippy::disallowed_methods, reason = "before the writer starts")]
    let asked = crossterm::terminal::supports_keyboard_enhancement();
    let enhanced = matches!(asked, Ok(true));
    let mut reset = Vec::new();
    write_close(&mut reset, enhanced);
    let mut writer = match Writer::start(writer::standard_output(), log, reset) {
        Ok(writer) => writer,
        Err(error) => {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(error);
        }
    };
    *crate::lock(&WRITER) = Some(writer.handle());
    let _ = execute!(writer, EnterAlternateScreen, EnableMouse, EnableFocusChange);
    // Not in the classic Windows console without escape codes
    // (crossterm/src/event.rs:413-435), so pastes come as keys there.
    let _ = execute!(writer, EnableBracketedPaste);
    // The title is saved, to be put back (xterm's title stack, and others
    // ignore it).
    if !cfg!(windows) {
        let _ = write!(writer, "\x1b[22;0t");
    }
    if enhanced {
        let _ = execute!(
            writer,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let _ = clear_new_screen(&mut writer);
    let screen = writer.handle();
    match Terminal::new(CursorBackend::new(writer)) {
        Ok(terminal) => Ok((terminal, screen, enhanced || cfg!(windows))),
        Err(error) => {
            // (Waited for off the runtime's thread, as what's printed next
            // goes straight to the terminal.)
            crate::blocking(close_and_wait);
            Err(error)
        }
    }
}

fn clear_new_screen(writer: &mut impl Write) -> std::io::Result<()> {
    // There is no cursor to preserve on a new alternate screen. Ratatui's
    // clear asks for it, which some terminals cannot report during startup.
    execute!(
        writer,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        cursor::MoveTo(0, 0)
    )
}

/// Whether a panic on the thread named `thread` ends the view, the view's
/// own, the main thread's, or the session's runtime's (tokio's default
/// name, which the command's runtime keeps).
fn ends_the_view(thread: Option<&str>) -> bool {
    matches!(thread, Some("main" | "tokio-runtime-worker" | THREAD))
}

/// The writer of the view that has or last had the terminal, the one owner
/// of the terminal's state. Locked only to take a handle, never together
/// with the writer's own lock.
static WRITER: std::sync::Mutex<Option<Writer>> = std::sync::Mutex::new(None);
/// How long giving the terminal back waits for it to take what is written,
/// before what follows (a panic's message, the end of the process).
const CLOSE_WAIT: Duration = Duration::from_secs(1);

/// Gives the terminal back through the view's writer ([`Writer::finish`]),
/// with `tail` after it, and raw mode off the first time. Safe to call more
/// than once, and from any thread. Returns the writer and the ticket to
/// wait on, unless no view had the terminal.
fn give_back(tail: &[u8]) -> Option<(Writer, writer::Ticket)> {
    let writer = writer_handle()?;
    let (ticket, ended) = writer.finish(tail);
    if ended {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    Some((writer, ticket))
}

/// Gives the terminal back, without waiting for it to take what does.
fn close() {
    give_back(&[]);
}

/// Gives the terminal back, then waits at most [`CLOSE_WAIT`] for it to take
/// what does, for what comes next outside the view (a panic's message, the
/// end of the process).
fn close_and_wait() {
    if let Some((writer, ticket)) = give_back(&[]) {
        writer.wait(ticket, CLOSE_WAIT);
    }
}

/// A handle on the view's writer, once it has one.
fn writer_handle() -> Option<Writer> {
    let writer = crate::lock(&WRITER);
    writer.as_ref().map(Writer::handle)
}

/// What gives the terminal back, written to `out`, with the keyboard protocol
/// popped (`pop`, when it was pushed), the mouse, focus and paste reports
/// off, the main screen back, the cursor shown and the title put back.
fn write_close(out: &mut impl Write, pop: bool) {
    // A frame cut short leaves its synchronized update open, and the
    // terminal then holds back what follows.
    let _ = queue!(out, EndSynchronizedUpdate);
    if pop {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        DisableMouseCapture,
        DisableFocusChange,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        cursor::Show
    );
    if !cfg!(windows) {
        let _ = write!(out, "\x1b[23;0t");
    }
    let _ = out.flush();
}

/// Gives the terminal back now, as the view does when it closes, for a
/// process about to end without closing it (a signal). It does nothing
/// when the view doesn't have the terminal.
pub(crate) fn give_back_terminal() {
    close_and_wait();
}

/// Cancel, which ends an escape sequence that a frame cut short left open.
#[cfg(unix)]
const CAN: u8 = 0x18;

/// How long an end that can't wait tries for a lock.
#[cfg(unix)]
const LOCK_WAIT: Duration = Duration::from_millis(50);

/// Gives the terminal back now, for a process that ends at once because its
/// runtime is stuck, or whose writer died, where [`give_back_terminal`] may
/// wait for what doesn't come. Unless the writer wrote the reset, it ends
/// the view's output ([`Writer::take_over`]) and writes the reset itself,
/// after a [`CAN`], straight to standard output's descriptor, on a thread
/// of its own that it waits for `wait` at most. It does nothing when no
/// view had the terminal.
#[cfg(unix)]
pub(crate) fn give_back_terminal_at_once(wait: Duration) {
    let found =
        writer::try_lock_for(&WRITER, LOCK_WAIT).map(|writer| writer.as_ref().map(Writer::handle));
    let reset = match found {
        Some(None) => return,
        Some(Some(writer)) => writer.take_over(LOCK_WAIT),
        None => writer::Reset::Unknown,
    };
    let reset = match reset {
        writer::Reset::Done => return,
        writer::Reset::Needed(reset) => reset,
        // (Popping the keyboard protocol that wasn't pushed is harmless.)
        writer::Reset::Unknown => {
            let mut reset = Vec::new();
            write_close(&mut reset, true);
            reset
        }
    };
    let mut bytes = vec![CAN];
    bytes.extend_from_slice(&reset);
    let (written, done) = std::sync::mpsc::channel();
    let writing = std::thread::Builder::new()
        .name("arko-reset".into())
        .spawn(move || {
            let _ = writer::standard_output().write_all(&bytes);
            let _ = written.send(());
        });
    if writing.is_ok() {
        let _ = done.recv_timeout(wait);
    }
}

/// Takes the terminal as the view does, for a test of how it's given back.
#[cfg(test)]
pub(crate) fn take_terminal() -> std::io::Result<impl Sized> {
    // (With no log for its writer's watchdog.)
    open(PathBuf::new())
}

/// What giving the terminal back undoes, and its sequence, for tests.
#[cfg(all(test, unix))]
pub(crate) const UNDONE: [(&str, &str); 10] = [
    ("synchronized update", "\x1b[?2026l"),
    ("keyboard protocol", "\x1b[<1u"),
    ("button reports", "\x1b[?1000l"),
    ("drag reports", "\x1b[?1002l"),
    ("SGR mouse", "\x1b[?1006l"),
    ("focus reports", "\x1b[?1004l"),
    ("bracketed paste", "\x1b[?2004l"),
    ("alternate screen", "\x1b[?1049l"),
    ("cursor", "\x1b[?25h"),
    ("title", "\x1b[23;0t"),
];

/// A paste that came as keys, with where it started in the box, and when it
/// ends unless more come.
struct Burst {
    start: usize,
    until: Instant,
}

/// Whether `event` types into the box, a character, or a plain Enter.
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

/// The view's loop over terminal events, the feed's updates and the clock, until
/// `stop`. What it starts for the session runs on the session's runtime
/// (`session`). Returns the last reply.
async fn run(
    mut terminal: Terminal<CursorBackend<Writer>>,
    screen: Writer,
    mut app: App,
    mut setup: Setup,
    mut updates: remote::Following,
    mut stop: oneshot::Receiver<()>,
    session: tokio::runtime::Handle,
) -> Option<String> {
    let mut events = EventStream::new();
    let mut events_open = true;
    let mut clock = tokio::time::interval(Duration::from_millis(500));
    let mut frames = tokio::time::interval(FRAME);
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // While the screen shows something that moves, its frames.
    let mut animation = tokio::time::interval(motion::FRAME);
    animation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Something changed, drawn at the next frame. Keys draw at once.
    let mut dirty = false;
    let mut draw_now = true;
    let mut title = String::new();
    // The terminal hasn't taken the last frame, so the next waits for it.
    let mut held = false;
    let mut enter: Option<Instant> = None;
    let mut burst: Option<Burst> = None;
    // The cache timer's words as last seen, drawn again when they change.
    let mut cache_label = None;
    // What each way of naming sessions by a model lacks, checked off this
    // loop's thread (the check runs Claude Code).
    let (names_found, mut names_checks) = mpsc::unbounded_channel();
    // Its clock turns the loop twice a second, so a loop that doesn't turn
    // for long is stuck, and the log says where. It watches the first read
    // of the status too.
    let watchdog = {
        let feed = setup.feed.clone();
        let terminal = screen.handle();
        let log = setup.session_dir.join(crate::session::LOG_FILE);
        let watched = crate::watchdog::Watched::Loop("The full screen's loop");
        crate::watchdog::Watchdog::start(watched, log, move || {
            // (Its frames wait while the terminal doesn't take them.)
            let waiting = terminal.waiting().filter(|waited| waited.as_secs() >= 1);
            match waiting {
                Some(waited) => format!(
                    "{} The terminal hasn't taken a frame for {} s.",
                    feed.holder(),
                    waited.as_secs()
                ),
                None => feed.holder(),
            }
        })
    };
    watchdog.stage("reading the status");
    app.set_reading(setup.meter.read());
    // The changes to OpenRouter's settings are made on a thread of their own,
    // one after another, and their results come back here.
    let settings_worker = openrouter_menu::Worker::start();
    let (settings_done, mut settings_results) = mpsc::unbounded_channel();
    // What became of what the user sent, as the session says.
    let (verdict_done, mut verdicts) = mpsc::unbounded_channel();
    // The view's dictation and its microphone, while it records, and where
    // its events come.
    let mut recording: (
        Option<crate::dictation::Dictation>,
        Option<crate::dictation::Capture>,
    ) = (None, None);
    let (recording_events, mut recording_received) = mpsc::unbounded_channel();
    loop {
        watchdog.turned();
        // The writer writes no more, because the terminal closed or the
        // writer's thread ended, so the view ends, as when the terminal
        // closes, and the session with it.
        if let Some(failure) = screen.failure() {
            if failure == writer::Failure::Died {
                log::error!("The full screen's writer stopped, so Arko ends");
                #[cfg(unix)]
                give_back_terminal_at_once(CLOSE_WAIT);
            }
            if events_open {
                give(&setup.requests, &verdict_done, control(Input::Eof));
            }
            break;
        }
        if draw_now {
            watchdog.stage("drawing");
            held = !draw_when_written(&mut terminal, &screen, &mut app, &mut title);
            draw_now = false;
            dirty = false;
        }
        let enter_at = enter.unwrap_or_else(Instant::now);
        let preview_at = app.preview_due();
        let approvals_at = app.approvals_due();
        let burst_at = burst
            .as_ref()
            .map_or_else(Instant::now, |burst| burst.until);
        let mut bell = false;
        watchdog.stage("waiting");
        tokio::select! {
            _ = &mut stop => break,
            Some(ask) = setup.quit_asks.recv() => {
                watchdog.stage("asking to quit");
                app.ask_quit(ask.jobs);
                draw_now = true;
            }
            Some((check, missing)) = names_checks.recv() => {
                watchdog.stage("taking a check of the session names");
                app.names_checked(&check, missing);
                draw_now = true;
            }
            event = events.next(), if events_open => {
                let Some(Ok(event)) = event else {
                    // The terminal is gone, as Ctrl-D at the prompt.
                    events_open = false;
                    give(&setup.requests, &verdict_done, control(Input::Eof));
                    continue;
                };
                watchdog.stage("taking keys");
                // What came together comes as one batch.
                let mut batch = vec![event];
                batch.extend(waiting(&mut events).await);
                handle(&mut app, batch, &mut enter, &mut burst);
                draw_now = true;
            }
            received = updates.next() => {
                watchdog.stage("taking the feed's updates");
                let (rings, asked) = take_updates(&mut app, received, &mut updates, &setup.feed);
                bell |= rings;
                // The web page just started says so in the feed, so its links
                // show at once. So do the agent's questions, in the answer
                // panel.
                if app.waits_for_page() || asked {
                    watchdog.stage("reading the status");
                    app.set_reading(setup.meter.read());
                }
                // Drawn at the next frame, because streaming text doesn't redraw at
                // every chunk.
                dirty = true;
            }
            Some((sending, verdict)) = verdicts.recv() => {
                watchdog.stage("taking what became of a send");
                match verdict {
                    Some(verdict) => app.verdict(sending, verdict),
                    None => app.no_verdict(sending),
                }
                draw_now = true;
            }
            Some(result) = settings_results.recv() => {
                watchdog.stage("taking OpenRouter's settings");
                app.openrouter_done(result);
                draw_now = true;
            }
            Some(update) = recording_received.recv() => {
                watchdog.stage("taking the dictation's events");
                app.dictation_event(update);
                while let Ok(update) = recording_received.try_recv() {
                    app.dictation_event(update);
                }
                draw_now = true;
            }
            _ = frames.tick(), if dirty => draw_now = true,
            () = screen.written(), if held => draw_now = true,
            _ = animation.tick(), if app.transcript.drawn.moving => draw_now = true,
            _ = clock.tick() => {
                watchdog.stage("reading the status");
                let reading = setup.meter.read();
                let workers = app.list_ticks();
                // (The quit popup's jobs show how long they have run.)
                let quitting = matches!(app.overlay, Some(overlay::Overlay::Quit { .. }));
                // (The prompt cache's timer counts down while the agent waits.)
                let counting = cache_label != app.cache_label();
                cache_label = app.cache_label();
                // (OpenRouter's models arrive in the background.)
                let models = app.openrouter_moved();
                if models || reading != app.reading || app.activity.working || workers || app.hint.is_some() || quitting || counting {
                    app.set_reading(reading);
                    dirty = true;
                    // Yes stops the jobs the popup shows, drawn before the
                    // next key.
                    draw_now |= quitting;
                }
            }
            _ = tokio::time::sleep_until(approvals_at.unwrap_or_else(Instant::now).into()), if approvals_at.is_some() => {
                draw_now = true;
            }
            _ = tokio::time::sleep_until(preview_at.unwrap_or_else(Instant::now).into()), if preview_at.is_some() => {
                // The theme picker's selection has rested, and its preview is drawn.
                watchdog.stage("previewing a theme");
                if app.preview(Instant::now()) {
                    draw_now = true;
                }
            }
            _ = tokio::time::sleep_until(enter_at.into()), if enter.is_some() => {
                watchdog.stage("taking keys");
                let batch = waiting(&mut events).await;
                if batch.is_empty() {
                    // No key followed the Enter, so it was typed, and sends.
                    enter = None;
                    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                } else {
                    handle(&mut app, batch, &mut enter, &mut burst);
                }
                draw_now = true;
            }
            _ = tokio::time::sleep_until(burst_at.into()), if burst.is_some() => {
                watchdog.stage("taking keys");
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
        watchdog.stage("carrying out what the view asked");
        // (What it starts, such as a notice's timer, outlives the view.)
        let _session = session.enter();
        bell |= app.take_ring();
        if bell {
            let out = terminal.backend_mut();
            let _ = out.write_all(b"\x07");
            let _ = out.flush();
        }
        // (Carrying out one can ask for another, such as a dictation's
        // notice.)
        let mut effects = app.take_effects();
        while !effects.is_empty() {
            for effect in effects {
                match effect {
                    Effect::Notice(level, text) => setup.feed.notice(level, &text),
                    Effect::Send(sending) => give(&setup.requests, &verdict_done, sending),
                    Effect::Input(input) => give(&setup.requests, &verdict_done, control(input)),
                    Effect::Copy { text, what } => {
                        // The terminal's clipboard, and the system's. The status
                        // line says "Copied" only when the system's took it.
                        let _ = execute!(
                            terminal.backend_mut(),
                            crossterm::clipboard::CopyToClipboard::to_clipboard_from(
                                text.as_bytes()
                            )
                        );
                        app.say(match clipboard::copy(&text) {
                            clipboard::Copied::System => format!("Copied {what}"),
                            clipboard::Copied::Terminal => {
                                "Sent to the terminal's clipboard".into()
                            }
                            clipboard::Copied::Failed(why) => why,
                        });
                    }
                    Effect::Mouse(on) => {
                        let out = terminal.backend_mut();
                        let _ = if on {
                            execute!(out, EnableMouse)
                        } else {
                            execute!(out, DisableMouseCapture)
                        };
                    }
                    Effect::Clear => {
                        let _ = clear_for_redraw(&mut terminal);
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
                    Effect::OpenRouter { root, edit } => {
                        let done = settings_done.clone();
                        settings_worker.submit(move || {
                            let _ = done.send(openrouter_menu::make(&root, &edit));
                        });
                    }
                    Effect::Dictate(dictate) => {
                        carry_out(dictate, &mut app, &setup, &mut recording, &recording_events)
                    }
                    Effect::CheckNames(cancel) => {
                        let found = names_found.clone();
                        tokio::task::spawn_blocking(move || {
                            let missing = crate::session_names::missing_until(&cancel.0);
                            if !cancel.0.load(std::sync::atomic::Ordering::Relaxed) {
                                let _ = found.send((cancel, missing));
                            }
                        });
                    }
                }
                draw_now = true;
            }
            effects = app.take_effects();
        }
    }
    let reply = app.transcript.last_reply().map(str::to_string);
    close();
    reply
}

/// Carries out what the view's recording needs of the dictation service
/// and the microphone. Its events come back on the view's own channel.
fn carry_out(
    dictate: dictation::Dictate,
    app: &mut App,
    setup: &Setup,
    (open, capture): &mut (
        Option<crate::dictation::Dictation>,
        Option<crate::dictation::Capture>,
    ),
    events: &mpsc::UnboundedSender<crate::dictation::DictationUpdate>,
) {
    use crate::dictation::{DictationOptions, DictationService, Source};
    match dictate {
        dictation::Dictate::Open { download } => {
            let sink = recording_sink(events.clone());
            match DictationService::shared(&setup.root).open(DictationOptions { download }, sink) {
                Ok(dictation) => {
                    crate::dictation::favour_session_words(
                        &dictation,
                        &setup.root,
                        &setup.feed,
                        &setup.details,
                    );
                    app.dictation_opened(dictation.id());
                    *open = Some(dictation);
                }
                Err(error) => app.dictation_refused(error),
            }
        }
        dictation::Dictate::Record => {
            if let Some(dictation) = open.as_ref() {
                match dictation.record(&Source::from_env()) {
                    Ok(recording) => *capture = Some(recording),
                    Err(why) => {
                        *open = None;
                        app.dictation_failed(why);
                    }
                }
            }
        }
        // (The dictation is kept until it ends, so Esc can still cancel it.)
        dictation::Dictate::Finish => {
            *capture = None;
            if let Some(dictation) = open.as_mut() {
                dictation.finish();
            }
        }
        // (Dropping the dictation cancels it, if it hasn't ended.)
        dictation::Dictate::Cancel | dictation::Dictate::Close => {
            *capture = None;
            *open = None;
        }
    }
}

/// Where the view's own recording's events go, to the view's loop, on a
/// channel of their own. The feed's subscription drops what a lagging view
/// misses and starts over from a history that holds no recording.
fn recording_sink(
    events: mpsc::UnboundedSender<crate::dictation::DictationUpdate>,
) -> crate::dictation::Sink {
    Box::new(move |update| {
        let _ = events.send(update);
    })
}

fn clear_for_redraw<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
) -> Result<(), B::Error> {
    // Fullscreen resize clears and invalidates the buffers without asking
    // for a cursor position to preserve, unlike Terminal::clear.
    let area = terminal.size()?.into();
    terminal.resize(area)
}

/// Takes `received` from the feed and the updates that wait after it,
/// before the next frame. Where the view fell behind, also while it takes
/// those, it starts over from the snapshot it gets instead. Returns whether
/// the bell should sound, and whether the status should be read again, for
/// the agent's questions (also when the view started over, as some may
/// have come while it was behind).
fn take_updates(
    app: &mut App,
    received: remote::Received,
    updates: &mut remote::Following,
    feed: &remote::Feed,
) -> (bool, bool) {
    let (mut bell, mut asked) = (false, false);
    let mut next = Some(received);
    while let Some(received) = next {
        match received {
            remote::Received::Update(update) => {
                asked |= asks(&update);
                bell |= app.update(update, || feed.activity());
            }
            remote::Received::Reset(snapshot) => {
                asked = true;
                bell |= app.reset(snapshot);
            }
        }
        next = updates.try_next();
    }
    (bell, asked)
}

/// What became of a send, as the session says it (`None` when it stopped
/// before it said), for the view's loop.
type Verdicts = mpsc::UnboundedSender<(Sending, Option<crate::Verdict>)>;

/// A stop, a quit or a move to the background, which puts nothing back.
fn control(input: Input) -> Sending {
    Sending {
        input,
        restore: app::Restore::Nothing,
    }
}

/// Gives the session `sending`, in the order the view gives everything, and
/// brings its verdict back to the view's loop through `verdicts`. When the
/// session no longer takes anything, it is refused as closed at once.
fn give(requests: &mpsc::UnboundedSender<crate::Request>, verdicts: &Verdicts, sending: Sending) {
    let (reply, verdict) = oneshot::channel();
    let request = crate::Request::Send {
        input: sending.input.clone(),
        reply,
    };
    let done = verdicts.clone();
    if requests.send(request).is_err() {
        let _ = done.send((sending, Some(crate::Verdict::closed())));
        return;
    }
    tokio::spawn(async move {
        let verdict = verdict.await.ok();
        let _ = done.send((sending, verdict));
    });
}

/// Whether `update` is of a question card, when the agent asked, or its
/// questions changed.
fn asks(update: &remote::Update) -> bool {
    matches!(
        update,
        remote::Update::Item(item) | remote::Update::Change(item)
            if item.kind == remote::Kind::Question
    )
}

/// The events already waiting, without waiting for more. (Polled in this
/// task, not with a no-op waker, because the stream registers the waker of its
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
/// the terminal doesn't mark pastes), so their line breaks are line breaks,
/// not sends. A lone Enter waits a moment for what follows it.
fn handle(
    app: &mut App,
    batch: Vec<Event>,
    enter: &mut Option<Instant>,
    burst: &mut Option<Burst>,
) {
    let now = Instant::now();
    let typed = batch.iter().filter(|event| types(event)).count();
    // (Three keys or more, because two quick keys of a person's can come in one
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
            Event::Resize(..) => app.resized = true,
        }
    }
    if pasting && let Some(burst) = burst {
        // From now, since a long batch takes a while in a debug build.
        burst.until = Instant::now() + BURST_GAP;
    }
}

/// Draws a frame of `app`, with the window's title when it changed from
/// `title`, unless the terminal hasn't taken the last frame from `screen`,
/// the terminal's writer, yet. Then the frame waits, and the changes go in
/// the next one the terminal gets. After a resize, the frame is drawn
/// whole. Whether it was drawn.
fn draw_when_written(
    terminal: &mut Terminal<CursorBackend<Writer>>,
    screen: &Writer,
    app: &mut App,
    title: &mut String,
) -> bool {
    if screen.busy() {
        return false;
    }
    // (Cleared only by a whole frame, which a skipped one isn't.)
    if app.resized && clear_for_redraw(terminal).is_ok() {
        app.resized = false;
    }
    draw(terminal, app);
    let now = app.title();
    if now != *title {
        let _ = execute!(terminal.backend_mut(), crossterm::terminal::SetTitle(&now));
        *title = now;
    }
    true
}

/// A frame, drawn as one update where the terminal can (synchronized
/// output, CSI ?2026, crossterm/src/terminal.rs:433-450, and others ignore it),
/// so it doesn't flicker.
fn draw(terminal: &mut Terminal<CursorBackend<Writer>>, app: &mut App) {
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

    #[test]
    fn a_new_alternate_screen_clears_without_asking_for_the_cursor() {
        let mut bytes = Vec::new();
        clear_new_screen(&mut bytes).unwrap();
        assert_eq!(bytes, b"\x1b[2J\x1b[1;1H");
    }

    #[test]
    fn clearing_for_redraw_repaints_unchanged_cells() {
        use ratatui::backend::{Backend, TestBackend};

        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        let draw = |terminal: &mut Terminal<TestBackend>| {
            terminal
                .draw(|frame| frame.render_widget("Still here", frame.area()))
                .unwrap();
        };
        draw(&mut terminal);
        // A corrupted screen needs repainting even when no cells changed.
        terminal.backend_mut().clear().unwrap();
        clear_for_redraw(&mut terminal).unwrap();
        draw(&mut terminal);
        terminal.backend().assert_buffer_lines([
            "Still here          ",
            "                    ",
            "                    ",
        ]);
    }

    /// A paste that comes as keys doesn't send at its line breaks, and a
    /// long one folds into a chip once it ends. A lone Enter sends after
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
        // Typed, one key at a time, so the Enter waits, then sends when
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
        assert_eq!(
            sent,
            [Effect::Send(
                crate::tui::app::Sending::of_box(
                    Input::Line("one\ntwo".into()),
                    "one\ntwo",
                    Vec::new()
                )
                .with_message(Some(app.tray.message.clone()))
            )]
        );
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

    /// Only panics on the threads that run the view give the terminal back.
    /// Dictation's thread recovers from its own, and the speech engine's
    /// workers' come back to it.
    #[test]
    fn only_the_views_threads_panics_give_the_terminal_back() {
        assert!(ends_the_view(Some("main")));
        assert!(ends_the_view(Some("tokio-runtime-worker")));
        assert!(ends_the_view(Some(THREAD)));
        for other in [
            Some("arko-dictation"),
            Some("speech-worker-3"),
            Some("arko-microphone"),
            None,
        ] {
            assert!(!ends_the_view(other), "{other:?}");
        }
        // The names the threads have.
        let dictation = std::thread::Builder::new()
            .name("arko-dictation".into())
            .spawn(|| ends_the_view(std::thread::current().name()))
            .unwrap();
        assert!(!dictation.join().unwrap());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        let ended = runtime
            .block_on(runtime.spawn(async { ends_the_view(std::thread::current().name()) }))
            .unwrap();
        assert!(ended, "the view's runtime");
    }

    /// When the feed's subscription lags and the view starts over from the
    /// history, its recording keeps its words and still gets every event,
    /// which come on their own channel.
    #[test]
    fn a_lagging_view_keeps_its_recording() {
        use crate::dictation::{DictationEvent, DictationUpdate};
        let feed = remote::Feed::new(remote::MAIN_AGENT);
        let (_, mut updates) = feed.follow();
        let mut app = app::tests::app();
        let (events, mut received) = mpsc::unbounded_channel();
        let mut sink = recording_sink(events);
        app.toggle_dictation();
        app.dictation_opened(7);
        app.take_effects();
        let text = |words: &str| DictationEvent::Text {
            settled: words.into(),
            tentative: String::new(),
        };
        let mut deliver = |app: &mut App| {
            while let Ok(update) = received.try_recv() {
                app.dictation_event(update);
            }
        };
        sink(DictationUpdate {
            id: 7,
            event: DictationEvent::Listening,
        });
        sink(DictationUpdate {
            id: 7,
            event: text("Hello"),
        });
        deliver(&mut app);
        for n in 0..5000 {
            feed.notice(remote::Level::Info, &format!("note {n}"));
        }
        sink(DictationUpdate {
            id: 7,
            event: text(" world"),
        });
        // The subscription lagged, and the view starts over as its loop does.
        let behind = updates.try_next().unwrap();
        assert!(matches!(behind, remote::Received::Reset(_)));
        take_updates(&mut app, behind, &mut updates, &feed);
        deliver(&mut app);
        assert_eq!(app.editor.text(), "Hello world");
        assert_eq!(
            app.recording.as_ref().map(|recording| recording.phase),
            Some(dictation::Phase::Listening)
        );
    }

    /// A view that falls behind the feed while it takes the updates that
    /// wait starts over from the history, and rings the bell for the turn
    /// that ended meanwhile, as it does when it falls behind while it waits.
    #[test]
    fn a_view_that_falls_behind_while_it_takes_updates_starts_over() {
        let feed = remote::Feed::new(remote::MAIN_AGENT);
        let (snapshot, mut updates) = feed.follow();
        let mut app = app::tests::app();
        app.focused = false;
        app.reset(snapshot);
        feed.turn_started();
        let started = updates.next().now_or_never().unwrap();
        assert!(!take_updates(&mut app, started, &mut updates, &feed).0);
        assert!(app.activity.working);
        // The loop takes one update, and more come before it takes the
        // rest, the turn's end and more than the broadcast keeps.
        feed.notice(remote::Level::Info, "first");
        let first = updates.next().now_or_never().unwrap();
        feed.turn_ended("3 s · $0.01", None);
        for n in 0..3000 {
            feed.notice(remote::Level::Info, &format!("note {n}"));
        }
        let (bell, asked) = take_updates(&mut app, first, &mut updates, &feed);
        assert!(bell, "the turn ended while it was behind");
        assert!(asked, "the status is read again");
        assert!(!app.activity.working);
        let shown = |app: &App| {
            let newest = feed.subscribe().0.items.last().unwrap().id;
            app.transcript.item(newest).map(|item| item.text.clone())
        };
        assert_eq!(shown(&app).as_deref(), Some("note 2999"));
        // It goes on from the snapshot it started over from.
        feed.notice(remote::Level::Info, "after");
        let after = updates.next().now_or_never().unwrap();
        assert!(matches!(&after, remote::Received::Update(_)));
        take_updates(&mut app, after, &mut updates, &feed);
        assert_eq!(shown(&app).as_deref(), Some("after"));
    }

    /// Giving the terminal back undoes all the view did to it, namely a
    /// frame's synchronized update, the keyboard protocol, the mouse, focus
    /// and paste reports, the alternate screen, the cursor and the title.
    #[cfg(unix)]
    #[test]
    fn the_terminal_is_given_back_whole() {
        let mut out = Vec::new();
        write_close(&mut out, true);
        let written = String::from_utf8(out).unwrap();
        for (what, sequence) in UNDONE {
            assert!(written.contains(sequence), "{what}: {written:?}");
        }
        // The protocol is popped only when it was pushed.
        let mut unpushed = Vec::new();
        write_close(&mut unpushed, false);
        assert!(!String::from_utf8(unpushed).unwrap().contains("\x1b[<1u"));
    }

    /// The view runs on a thread of its own, so a view that waits without
    /// awaiting (for a lock, say) holds up none of the session's tasks, and
    /// a session whose runtime is stuck holds up none of the view's frames.
    #[test]
    fn the_view_and_the_session_dont_hold_each_other_up() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::mpsc::channel;
        const LONG: Duration = Duration::from_secs(5);
        // One thread runs the session's tasks, and its timers when it rests.
        let session = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (release, waiting) = channel::<()>();
        let ticks = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let view = {
            let (ticks, stop) = (ticks.clone(), stop.clone());
            move || async move {
                let _ = waiting.recv();
                let mut frames = tokio::time::interval(Duration::from_millis(10));
                while !stop.load(Ordering::Relaxed) {
                    frames.tick().await;
                    ticks.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::current().name().map(str::to_string)
            }
        };
        let reply = on_a_thread(None, view).unwrap();

        // The view waits, and the session's timer fires.
        let (fired, fires) = channel();
        session.spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = fired.send(());
        });
        assert!(fires.recv_timeout(LONG).is_ok(), "the session was held up");

        // The session's thread is stuck, and the view's frames go on.
        release.send(()).unwrap();
        let (unstick, stuck) = channel::<()>();
        session.spawn(async move {
            let _ = stuck.recv();
        });
        let before = ticks.load(Ordering::Relaxed);
        let started = Instant::now();
        while ticks.load(Ordering::Relaxed) < before + 10 {
            assert!(started.elapsed() < LONG, "the view was held up");
            std::thread::sleep(Duration::from_millis(10));
        }
        unstick.send(()).unwrap();
        stop.store(true, Ordering::Relaxed);
        assert_eq!(reply.blocking_recv().unwrap().as_deref(), Some(THREAD));
    }

    /// What the view gives the session, its messages and its stops alike,
    /// goes in one channel, so the session reads them in the order given.
    /// When the session no longer takes anything, a send is refused as
    /// closed at once.
    #[tokio::test]
    async fn the_view_gives_everything_in_one_order() {
        let (requests, mut received) = mpsc::unbounded_channel();
        let (verdicts, mut verdicts_back) = mpsc::unbounded_channel();
        let message = Sending::of_box(Input::Line("Hello.".into()), "Hello.", Vec::new());
        give(&requests, &verdicts, message);
        give(&requests, &verdicts, control(Input::Interrupt));
        let read: Vec<Input> = std::iter::from_fn(|| received.try_recv().ok())
            .map(|request| match request {
                crate::Request::Send { input, reply } => {
                    let _ = reply.send(crate::Verdict::accepted(remote::Delivery::Command, None));
                    input
                }
                _ => panic!("a send"),
            })
            .collect();
        assert_eq!(read, [Input::Line("Hello.".into()), Input::Interrupt]);
        drop(received);
        give(
            &requests,
            &verdicts,
            Sending::of_box(Input::Line("Late.".into()), "Late.", Vec::new()),
        );
        let mut closed = None;
        for _ in 0..3 {
            let (sending, verdict) = verdicts_back.recv().await.unwrap();
            if sending.input == Input::Line("Late.".into()) {
                closed = verdict;
            }
        }
        assert_eq!(closed, Some(crate::Verdict::closed()));
    }

    /// A terminal's screen as the bytes sent to it draw it. Characters go at
    /// the cursor, which moves (`CSI row;col H`), and the screen clears
    /// (`CSI 2J`, and `CSI K` to the line's end). The other sequences
    /// (colours, modes, the title) change nothing here.
    #[cfg(unix)]
    struct Grid {
        rows: Vec<Vec<String>>,
        at: (usize, usize),
    }

    #[cfg(unix)]
    impl Grid {
        fn new(width: usize, height: usize) -> Self {
            Self {
                rows: vec![vec![" ".to_string(); width]; height],
                at: (0, 0),
            }
        }

        /// The terminal took a new size, with its cells left as they were
        /// where they still fit.
        fn resize(&mut self, width: usize, height: usize) {
            self.rows.resize(height, Vec::new());
            for row in &mut self.rows {
                row.resize(width, " ".to_string());
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            let text = String::from_utf8_lossy(bytes);
            let mut chars = text.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\x1b' => match chars.next() {
                        Some('[') => {
                            let mut params = String::new();
                            for c in chars.by_ref() {
                                if ('\x40'..='\x7e').contains(&c) {
                                    self.sequence(&params, c);
                                    break;
                                }
                                params.push(c);
                            }
                        }
                        // An OSC (the title), to its BEL or ST.
                        Some(']') => {
                            while let Some(c) = chars.next() {
                                if c == '\x07' || (c == '\x1b' && chars.next() == Some('\\')) {
                                    break;
                                }
                            }
                        }
                        _ => {}
                    },
                    c if c < ' ' => {}
                    c => {
                        let width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
                        let (x, y) = self.at;
                        if let Some(row) = self.rows.get_mut(y) {
                            if let Some(cell) = row.get_mut(x) {
                                *cell = c.to_string();
                            }
                            for covered in 1..width {
                                if let Some(cell) = row.get_mut(x + covered) {
                                    cell.clear();
                                }
                            }
                        }
                        self.at.0 += width;
                    }
                }
            }
        }

        fn sequence(&mut self, params: &str, last: char) {
            match last {
                'H' => {
                    let mut parts = params
                        .split(';')
                        .map(|part| part.parse::<usize>().unwrap_or(1));
                    let row = parts.next().unwrap_or(1);
                    let column = parts.next().unwrap_or(1);
                    self.at = (column - 1, row - 1);
                }
                'J' if params == "2" => {
                    for row in &mut self.rows {
                        row.fill(" ".to_string());
                    }
                }
                'K' => {
                    let (x, y) = self.at;
                    if let Some(row) = self.rows.get_mut(y) {
                        for cell in row.iter_mut().skip(x) {
                            *cell = " ".to_string();
                        }
                    }
                }
                _ => {}
            }
        }

        fn lines(&self) -> Vec<String> {
            self.rows.iter().map(|row| row.concat()).collect()
        }
    }

    /// The lines a buffer shows, with a wide character's second cell left
    /// out, as [`Grid::lines`] has them.
    #[cfg(unix)]
    fn buffer_lines(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        let area = buffer.area;
        (area.top()..area.bottom())
            .map(|y| {
                let mut line = String::new();
                let mut skip = 0;
                for x in area.left()..area.right() {
                    let symbol = buffer[(x, y)].symbol();
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    skip = unicode_width::UnicodeWidthStr::width(symbol).saturating_sub(1);
                    line.push_str(symbol);
                }
                line
            })
            .collect()
    }

    /// While the terminal doesn't read, the view takes keys and the feed's
    /// updates, and its frames wait without holding it up. A resize comes
    /// meanwhile. Once the terminal reads again, it takes the waiting frame,
    /// and the next frame leaves the screen as the view shows it now.
    #[cfg(unix)]
    #[test]
    fn the_view_goes_on_while_the_terminal_doesnt_read_and_catches_up() {
        use crate::remote::{Activity, Item, Kind, Update};
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::{TerminalOptions, Viewport};

        let temp = tempfile::tempdir().unwrap();
        let (mut read, write, filled) = writer::tests::full_pipe();
        // What the writer sent the terminal, so the test waits until the
        // terminal read all of it.
        struct Counted(std::fs::File, Arc<std::sync::atomic::AtomicUsize>);
        impl Write for Counted {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let count = self.0.write(bytes)?;
                self.1.fetch_add(count, std::sync::atomic::Ordering::SeqCst);
                Ok(count)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.flush()
            }
        }
        let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writer = Writer::start(
            Counted(write, sent.clone()),
            temp.path().join("arko.log"),
            Vec::new(),
        )
        .unwrap();
        let screen = writer.handle();
        let small = Rect::new(0, 0, 80, 24);
        let mut terminal = Terminal::with_options(
            CursorBackend::new(writer),
            TerminalOptions {
                viewport: Viewport::Fixed(small),
            },
        )
        .unwrap();
        let mut app = app::tests::app();
        let mut title = String::new();
        // The first frame goes to the writer, which waits for the terminal.
        assert!(draw_when_written(
            &mut terminal,
            &screen,
            &mut app,
            &mut title
        ));
        let started = Instant::now();
        for c in "typed while it waits".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            assert!(!draw_when_written(
                &mut terminal,
                &screen,
                &mut app,
                &mut title
            ));
        }
        for id in 1..=5 {
            let mut item = Item::new(Kind::Text, format!("Update {id} from the feed"));
            item.id = id;
            app.update(Update::Item(item), Activity::default);
            assert!(!draw_when_written(
                &mut terminal,
                &screen,
                &mut app,
                &mut title
            ));
        }
        let large = Rect::new(0, 0, 100, 30);
        terminal.resize(large).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(app.editor.text(), "typed while it waits");
        assert!(screen.busy());

        // The terminal reads again, and takes the first frame.
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reading = received.clone();
        std::thread::spawn(move || {
            let mut chunk = [0; 65536];
            while let Ok(count) = std::io::Read::read(&mut read, &mut chunk) {
                if count == 0 {
                    break;
                }
                reading.lock().unwrap().extend_from_slice(&chunk[..count]);
            }
        });
        let taken = || {
            let count = filled + sent.load(std::sync::atomic::Ordering::SeqCst);
            let started = Instant::now();
            while received.lock().unwrap().len() < count {
                assert!(started.elapsed() < Duration::from_secs(5), "read");
                std::thread::sleep(Duration::from_millis(10));
            }
            count
        };
        assert!(screen.wait_written(Duration::from_secs(5)));
        let first = taken();
        assert!(draw_when_written(
            &mut terminal,
            &screen,
            &mut app,
            &mut title
        ));
        assert!(screen.wait_written(Duration::from_secs(5)));
        taken();

        let bytes = received.lock().unwrap().clone();
        let mut grid = Grid::new(80, 24);
        grid.feed(&bytes[filled..first]);
        grid.resize(100, 30);
        grid.feed(&bytes[first..]);
        let mut reference = Terminal::new(TestBackend::new(100, 30)).unwrap();
        reference
            .draw(|frame| screen::draw(frame, &mut app))
            .unwrap();
        let expected = buffer_lines(reference.backend().buffer());
        assert_eq!(grid.lines(), expected);
        assert!(
            expected
                .iter()
                .any(|line| line.contains("Update 5 from the feed"))
        );
        assert!(
            expected
                .iter()
                .any(|line| line.contains("typed while it waits"))
        );
    }

    /// A frame that panics after it began leaves its first bytes in the
    /// backend, which flushes them as the panic unwinds, after the panic hook
    /// gave the terminal back. They are dropped, so the reset is the last
    /// thing the terminal gets.
    #[cfg(unix)]
    #[test]
    fn a_frame_cut_by_a_panic_writes_nothing_after_the_reset() {
        use ratatui::layout::Rect;
        use ratatui::{TerminalOptions, Viewport};
        let gate = writer::tests::Gate::default();
        let mut reset = Vec::new();
        write_close(&mut reset, true);
        let writer = Writer::start(gate.clone(), PathBuf::new(), reset.clone()).unwrap();
        let hook = writer.handle();
        let mut terminal = Terminal::with_options(
            CursorBackend::new(writer),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 20, 5)),
            },
        )
        .unwrap();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = queue!(terminal.backend_mut(), BeginSynchronizedUpdate);
            let _ = terminal.draw(|_| {
                // What the panic hook does, before the panic unwinds.
                let (ticket, _) = hook.finish(&[]);
                assert_eq!(
                    hook.wait(ticket, Duration::from_secs(5)),
                    writer::Finish::Written
                );
                panic!("a frame that breaks");
            });
        }));
        assert!(panicked.is_err());
        drop(terminal);
        assert!(hook.wait_written(Duration::from_secs(5)));
        assert_eq!(gate.taken(), reset);
    }

    /// The window shrinks and comes back to its size while a frame waits
    /// for the terminal, which cuts the cells outside the smaller size. The
    /// next frame is drawn whole, so the screen is right again, although the
    /// size is the one the last frame had.
    #[cfg(unix)]
    #[test]
    fn a_resize_while_a_frame_waits_draws_the_next_whole() {
        use crate::remote::{Activity, Item, Kind, Update};
        use ratatui::backend::TestBackend;
        use ratatui::layout::Size;
        let gate = writer::tests::Gate::default();
        let writer = Writer::start(gate.clone(), PathBuf::new(), Vec::new()).unwrap();
        let screen = writer.handle();
        let mut backend = CursorBackend::new(writer);
        backend.size = Some(Size::new(40, 8));
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app::tests::app();
        let mut title = String::new();
        let (mut enter, mut burst) = (None, None);
        let item = |id: u64| {
            let mut item = Item::new(
                Kind::Text,
                format!("Line {id} of what the agent said, long enough to reach the edge"),
            );
            item.id = id;
            Update::Item(item)
        };
        for id in 1..=3 {
            app.update(item(id), Activity::default);
        }
        assert!(draw_when_written(
            &mut terminal,
            &screen,
            &mut app,
            &mut title
        ));
        assert!(screen.wait_written(Duration::from_secs(5)));
        let mut grid = Grid::new(40, 8);
        let first = gate.taken().len();
        grid.feed(&gate.taken());
        // The terminal stops taking output, and a frame waits.
        gate.hold(true);
        app.update(item(4), Activity::default);
        assert!(draw_when_written(
            &mut terminal,
            &screen,
            &mut app,
            &mut title
        ));
        for (width, height) in [(25, 4), (40, 8)] {
            terminal.backend_mut().size = Some(Size::new(width, height));
            handle(
                &mut app,
                vec![Event::Resize(width, height)],
                &mut enter,
                &mut burst,
            );
            assert!(!draw_when_written(
                &mut terminal,
                &screen,
                &mut app,
                &mut title
            ));
            grid.resize(usize::from(width), usize::from(height));
        }
        // It takes output again, the waiting frame first.
        gate.hold(false);
        assert!(screen.wait_written(Duration::from_secs(5)));
        assert!(draw_when_written(
            &mut terminal,
            &screen,
            &mut app,
            &mut title
        ));
        assert!(screen.wait_written(Duration::from_secs(5)));
        grid.feed(&gate.taken()[first..]);
        let mut reference = Terminal::new(TestBackend::new(40, 8)).unwrap();
        reference
            .draw(|frame| screen::draw(frame, &mut app))
            .unwrap();
        assert_eq!(grid.lines(), buffer_lines(reference.backend().buffer()));
    }
}

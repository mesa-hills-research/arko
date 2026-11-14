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
//!
//! termelon has the terminal while the view runs. Its threads read and
//! write it, and its writer sends the terminal everything.

// While it has the terminal, only termelon's writer writes to it (clippy.toml).
#![cfg_attr(not(test), deny(clippy::disallowed_methods, clippy::disallowed_macros))]

mod app;
mod approvals;
mod cache;
mod clipboard;
#[cfg(feature = "dbg-snapshot")]
mod dbg_snapshot;
mod dictation;
#[cfg(any(test, feature = "dbg-snapshot"))]
mod golden;
mod input;
#[cfg(test)]
mod inventory_tests;
mod keys_menu;
mod markdown;
mod model_picker;
mod motion;
mod names_menu;
mod openrouter_menu;
mod overlay;
mod prefs;
mod questions;
#[cfg(test)]
mod recorder_tests;
mod resume_picker;
mod screen;
#[cfg(test)]
mod scroll_tests;
mod select;
#[cfg(test)]
mod shift_tests;
pub(crate) mod theme;
mod transcript;
mod working;
mod wrap;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use termelon::caps::KeyboardSupport;
use termelon::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use termelon::{Drawn, MouseMode, Options, RestoreHandle, Terminal};
use tokio::sync::{mpsc, oneshot};

use crate::glyphs;
use crate::remote;
use crate::ui::{self, Input, Output};
use app::{App, Effect, Info, Sending};
pub use transcript::Banner;

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
    /// Ends once the view gave the terminal back, with the last reply after
    /// it.
    closed: oneshot::Receiver<()>,
    output: Arc<Output>,
}

/// Whether the full-screen view can run, which it does when standard input and output are a
/// terminal that takes escape codes (not TERM=dumb). The classic Windows console without them
/// gets the line prompt when [`start`] can't open it.
#[allow(
    clippy::disallowed_methods,
    reason = "before the view has the terminal"
)]
pub fn available() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var("TERM").map_or(true, |term| term != "dumb")
}

/// Takes the screen, and shows the session there from now on. `output`'s
/// lines go to the feed. When the terminal can't be set up, the line
/// prompt reads the input instead, and `None` is returned.
pub fn start(setup: Setup, output: &Arc<Output>) -> Option<Screen> {
    let _opening = Opening::new();
    let history = setup.root.join("history.txt");
    // Settled first, so its warning, if any, prints before the screen is taken.
    glyphs::symbols();
    let catalog = theme::Catalog::load(Some(&setup.root.join("themes")));
    let mut terminal = match open() {
        Ok(terminal) => terminal,
        Err(error) => {
            // (The classic Windows console without escape codes takes the
            // line prompt without a word, as it always has.)
            if !matches!(error, termelon::Error::NoVirtualTerminal) {
                say_it_didnt_start(&error);
            }
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
    let depth = theme::depth(terminal.capabilities().colors);
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
    app.shift_enter = shift_enter(terminal.capabilities().keyboard);
    if !app.mouse {
        let _ = terminal.set_mouse(MouseMode::Off);
    }
    let (stop, stopped) = oneshot::channel();
    let input = setup.input.clone();
    let ended = input.clone();
    let session = tokio::runtime::Handle::current();
    let view = move || async move {
        let running =
            std::panic::AssertUnwindSafe(run(terminal, app, setup, updates, stopped, session));
        if running.catch_unwind().await.is_err() {
            // The panic hook gave the terminal back and said why. The
            // session ends as Ctrl-D ends it (it is saved).
            let _ = ended.send(Input::Eof);
        }
    };
    let closed = match on_a_thread(crate::context::handle(), view) {
        Ok(closed) => closed,
        Err(error) => {
            // (The terminal went back as the view, which had it, was dropped.)
            say_its_thread_didnt_start(&error);
            let _ = input.send(Input::Eof);
            oneshot::channel().1
        }
    };
    Some(Screen {
        stop,
        closed,
        output: output.clone(),
    })
}

/// Says why the full screen didn't start, on standard error, once the
/// terminal is as it was.
#[allow(clippy::disallowed_macros, reason = "the terminal is as it was")]
fn say_it_didnt_start(error: &termelon::Error) {
    let why = match std::error::Error::source(error) {
        Some(source) => format!("{error}: {source}"),
        None => error.to_string(),
    };
    eprintln!("arko: the full-screen view couldn't start ({why}), so Arko uses the line prompt");
}

/// Says why the view's thread didn't start, on standard error, once the
/// terminal is as it was.
#[allow(clippy::disallowed_macros, reason = "the terminal is as it was")]
fn say_its_thread_didnt_start(error: &std::io::Error) {
    eprintln!("arko: the full-screen view couldn't start ({error})");
}

/// Whether the terminal tells Shift+Enter from Enter, as it does with the
/// first flag of the kitty keyboard protocol, which termelon asks for, and
/// as Windows' console does, whose key records carry Shift with Enter.
fn shift_enter(keyboard: KeyboardSupport) -> bool {
    matches!(keyboard, KeyboardSupport::Kitty { flags } if flags & 1 != 0) || cfg!(windows)
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
        // The view gives the terminal back as its loop ends, whatever
        // happened to it.
        let _ = self.closed.await;
        // (What Arko prints next goes straight to the terminal, after this.)
        self.output.detach();
    }
}

/// The terminal made the view's, namely raw mode, the alternate screen, the
/// mouse (buttons, the wheel and drags, not every motion, which would wake
/// the view at every move), pastes and focus reported, and Shift+Enter told
/// apart from Enter where the terminal can (the kitty keyboard protocol).
/// From then on termelon's writer sends it everything. When it fails, the
/// terminal is as it was.
fn open() -> termelon::Result<Terminal> {
    install_panic_hook();
    // (The view's own hook gives the terminal back, by its rules.)
    let terminal = Terminal::open(Options::default().panic_hook(false))?;
    *crate::lock(&TERMINAL) = Some(terminal.restore_handle());
    Ok(terminal)
}

/// Makes a panic that ends the view give the terminal back before its
/// message prints. Another thread's, which dictation's thread recovers from
/// (and the speech engine's workers, whose panics it rethrows there), goes to
/// the log and leaves the screen.
fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let opening = OPENING.with(std::cell::Cell::get);
            let writer = termelon::is_writer_thread();
            if !gives_back(std::thread::current().name(), opening, writer) {
                log::error!("{info}");
                return;
            }
            give_back_terminal_at_once(CLOSE_WAIT);
            previous(info);
        }));
    });
}

/// Whether a panic on the thread named `thread` ends the view, so that it
/// gives the terminal back. The view's thread's, the main thread's (whose
/// panic ends the process), the thread opening the view while it does
/// (`opening`), and termelon's writer's (`writer`), which can't give the
/// terminal back itself. A session's task, whose panic tokio catches, or
/// dictation's thread, which recovers from its own, leave the view as it is.
fn gives_back(thread: Option<&str>, opening: bool, writer: bool) -> bool {
    matches!(thread, Some(THREAD | "main")) || opening || writer
}

thread_local! {
    /// This thread is opening the view ([`start`]).
    static OPENING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// This thread opens the view while it's kept ([`OPENING`]).
struct Opening;

impl Opening {
    fn new() -> Self {
        OPENING.with(|opening| opening.set(true));
        Self
    }
}

impl Drop for Opening {
    fn drop(&mut self) {
        OPENING.with(|opening| opening.set(false));
    }
}

/// What gives back the terminal that the view has or last had. Locked only
/// to take or put a handle.
static TERMINAL: Mutex<Option<RestoreHandle>> = Mutex::new(None);
/// How long giving the terminal back waits for it to take what is written,
/// before what follows (a panic's message, the end of the process).
const CLOSE_WAIT: Duration = Duration::from_secs(1);

/// Gives the terminal back now, as the view does when it closes, for a
/// process about to end without closing it (a signal). It does nothing
/// when the view doesn't have the terminal. Only Unix ends on signals.
#[cfg(unix)]
pub(crate) fn give_back_terminal() {
    give_back_terminal_at_once(CLOSE_WAIT);
}

/// Gives the terminal back now, for a process that ends at once (a panic,
/// a signal, a runtime that is stuck), waiting at most `wait` for it to
/// take the reset. termelon stops its writer and writes the reset from a
/// thread of its own, once however many ends ask, and puts the terminal's
/// modes back. It does nothing when no view had the terminal.
pub(crate) fn give_back_terminal_at_once(wait: Duration) {
    let handle = crate::lock(&TERMINAL).clone();
    if let Some(handle) = handle {
        handle.restore_now(wait);
    }
}

/// Takes the terminal as the view does, for a test of how it's given back.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn take_terminal() -> termelon::Result<impl Sized> {
    open()
}

/// What giving the terminal back undoes, and its sequence, for tests.
#[cfg(all(test, target_os = "linux"))]
pub(crate) const UNDONE: [(&str, &str); 12] = [
    ("synchronized update", "\x1b[?2026l"),
    ("keyboard protocol", "\x1b[<1u"),
    ("bracketed paste", "\x1b[?2004l"),
    ("focus reports", "\x1b[?1004l"),
    ("SGR mouse", "\x1b[?1006l"),
    ("drag reports", "\x1b[?1002l"),
    ("button reports", "\x1b[?1000l"),
    ("colours and attributes", "\x1b[0m"),
    ("autowrap", "\x1b[?7h"),
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

/// The view's loop over terminal events, the feed's updates and the clock,
/// until `stop`. What it starts for the session runs on the session's
/// runtime (`session`). Then it gives the terminal back, with the last reply
/// after it.
async fn run(
    mut terminal: Terminal,
    mut app: App,
    mut setup: Setup,
    mut updates: remote::Following,
    mut stop: oneshot::Receiver<()>,
    session: tokio::runtime::Handle,
) {
    let mut events = terminal.events();
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
    // The animation's clock moved the spinner, the highlight and the status
    // line's timer, which a frame draws alone while nothing else changed.
    let mut ticked = false;
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
    let log = setup.session_dir.join(crate::session::LOG_FILE);
    // The terminal's own, which logs a write it hasn't taken for long.
    let _terminal_watchdog = {
        let terminal = terminal.restore_handle();
        crate::watchdog::Watchdog::terminal(log.clone(), move || terminal.writer_waiting())
    };
    // Its clock turns the loop twice a second, so a loop that doesn't turn
    // for long is stuck, and the log says where. It watches the first read
    // of the status too.
    let watchdog = {
        let feed = setup.feed.clone();
        let terminal = terminal.restore_handle();
        let watched = crate::watchdog::Watched::Loop("The full screen's loop");
        crate::watchdog::Watchdog::start(watched, log, move || {
            // (Its frames wait while the terminal doesn't take them.)
            let waiting = terminal
                .writer_waiting()
                .filter(|waited| waited.as_secs() >= 1);
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
        // The terminal takes no more output, because it closed or the
        // writer's thread ended, so the view ends, as when the terminal
        // closes, and the session with it. (Closing gives the terminal back
        // without the writer.)
        if let Some(failure) = terminal.failure() {
            if failure == std::io::ErrorKind::Other {
                log::error!("The full screen's writer stopped, so Arko ends");
            }
            if events_open {
                give(&setup.requests, &verdict_done, control(Input::Eof));
            }
            break;
        }
        if !held && (draw_now || ticked) {
            watchdog.stage("drawing");
            let drawn = if draw_now || dirty {
                draw_when_written(&mut terminal, &mut app, &mut title)
            } else {
                draw_moving(&mut terminal, &mut app, &mut title)
            };
            // (A frame the terminal holds is drawn once it took the last.)
            held = !drawn;
            if drawn {
                (draw_now, dirty, ticked) = (false, false, false);
            }
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
                let Ok(event) = event else {
                    // The terminal is gone, as Ctrl-D at the prompt.
                    events_open = false;
                    give(&setup.requests, &verdict_done, control(Input::Eof));
                    continue;
                };
                watchdog.stage("taking keys");
                // What came together comes as one batch.
                let mut batch = vec![event];
                batch.extend(events.batch());
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
            _ = frames.tick(), if dirty && !held => draw_now = true,
            () = terminal.written(), if held => held = false,
            _ = animation.tick(), if !app.transcript.drawn.moving.is_empty() && !held => ticked = true,
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
                let batch = events.batch();
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
                let batch = events.batch();
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
            let _ = terminal.bell();
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
                        // The terminal's clipboard, where it takes copies, and
                        // the system's. The status line says "Copied" only when
                        // the system's took it.
                        let sent = matches!(terminal.copy_to_clipboard(&text), Ok(true));
                        app.say(copy_said(clipboard::copy(&text), sent, &what));
                    }
                    Effect::Mouse(on) => {
                        let mode = if on {
                            MouseMode::ButtonsAndDrags
                        } else {
                            MouseMode::Off
                        };
                        let _ = terminal.set_mouse(mode);
                    }
                    // The next frame repaints every cell, also those that
                    // didn't change.
                    Effect::Clear => terminal.clear(),
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
                    #[cfg(feature = "dbg-snapshot")]
                    Effect::Snapshot(snapshot) => {
                        let root = setup.root.clone();
                        let saved = dbg_snapshot::save_off_thread(root, *snapshot).await;
                        app.snapshot_saved(saved);
                    }
                    #[cfg(feature = "dbg-snapshot")]
                    Effect::SnapshotNextFrame(note) => {
                        // Drawn now, once the terminal took the last frame
                        // (a second at most), so the saved one is this frame.
                        let _ = tokio::time::timeout(CLOSE_WAIT, terminal.written()).await;
                        draw(&mut terminal, &mut app);
                        let snapshot = app.snapshot(&note);
                        let root = setup.root.clone();
                        let saved = dbg_snapshot::save_off_thread(root, snapshot).await;
                        app.snapshot_saved(saved);
                    }
                }
                draw_now = true;
            }
            effects = app.take_effects();
        }
    }
    // The terminal as it was, and then the last reply printed where the
    // user can scroll back to it. (This thread waits a second at most for
    // the terminal to take them.)
    let reply = app
        .transcript
        .last_reply()
        .map(|reply| format!("{}\n\n", glyphs::render(reply.trim())))
        .unwrap_or_default();
    let _ = terminal.close_with_tail(reply.as_bytes());
}

/// What the status line says of a copy of `what`, which the system's
/// clipboard took or didn't (`copied`), and which went to the terminal's
/// (`sent`) or didn't.
fn copy_said(copied: clipboard::Copied, sent: bool, what: &str) -> String {
    match copied {
        clipboard::Copied::System => format!("Copied {what}"),
        clipboard::Copied::Terminal if sent => "Sent to the terminal's clipboard".into(),
        clipboard::Copied::Terminal => clipboard::NOT_SENT.into(),
        clipboard::Copied::Failed(why) => why,
    }
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
            Event::FocusGained => app.focus_changed(true),
            Event::FocusLost => app.focus_changed(false),
            // (termelon draws the next frame whole.)
            Event::Resize(..) | Event::Resumed => {}
            // (Arko doesn't ask for colour scheme reports.)
            _ => {}
        }
    }
    if pasting && let Some(burst) = burst {
        // From now, since a long batch takes a while in a debug build.
        burst.until = Instant::now() + BURST_GAP;
    }
}

/// Draws a frame of `app`, with the window's title when it changed from
/// `title`, unless the terminal hasn't taken the last frame yet. Then the
/// frame waits, and the changes go in the next one the terminal gets.
/// Whether it was drawn.
fn draw_when_written(terminal: &mut Terminal, app: &mut App, title: &mut String) -> bool {
    if !draw(terminal, app) {
        return false;
    }
    let now = app.title();
    if now != *title {
        let _ = terminal.set_title(&now);
        *title = now;
    }
    true
}

/// A frame of `app`, as one update where the terminal can (synchronized
/// output), so it doesn't flicker, unless the terminal hasn't taken the
/// last one. Whether it was drawn.
fn draw(terminal: &mut Terminal, app: &mut App) -> bool {
    match terminal.draw(|frame| screen::draw(frame, app)) {
        Ok(Drawn::Held) => false,
        // Kept for Ctrl+S, which saves the frame on the screen without drawing.
        #[cfg(feature = "dbg-snapshot")]
        Ok(Drawn::Written(drawn)) => {
            app.drawn_frame.keep(&drawn);
            true
        }
        // (A terminal that takes no more output ends the loop at its next
        // turn.)
        _ => true,
    }
}

/// Draws over the last frame only what the animation's clock moves
/// ([`screen::moving`]), where nothing else changed, and the whole frame
/// where that can't be drawn alone or the terminal needs a whole one.
/// Whether it was drawn.
fn draw_moving(terminal: &mut Terminal, app: &mut App, title: &mut String) -> bool {
    let Some(regions) = screen::moving(app) else {
        return draw_when_written(terminal, app, title);
    };
    match terminal.draw_regions(&regions, |frame| screen::draw_moving(frame, app)) {
        Ok(Drawn::Held) => false,
        Ok(Drawn::FullFrameDue) => draw_when_written(terminal, app, title),
        #[cfg(feature = "dbg-snapshot")]
        Ok(Drawn::Written(drawn)) => {
            app.drawn_frame.keep(&drawn);
            true
        }
        // (A terminal that takes no more output ends the loop at its next
        // turn.)
        _ => true,
    }
}

/// The lines a buffer shows, one per row at its full width, as the terminal
/// shows them (the cells a wide character covers are empty).
#[cfg(any(test, feature = "dbg-snapshot"))]
fn buffer_lines(buffer: &termelon::Buffer) -> Vec<String> {
    let area = buffer.area();
    (area.top()..area.bottom())
        .map(|y| {
            (area.left()..area.right())
                .map(|x| buffer.symbol((x, y)))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use termelon::event::{MouseEvent, MouseEventKind};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
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
        let switch = app.message_switch().clone();
        // Typed, one key at a time, so the Enter waits, then sends when
        // nothing follows (the loop's timer).
        handle(&mut app, vec![key(KeyCode::Enter)], &mut enter, &mut burst);
        assert!(enter.is_some());
        handle(
            &mut app,
            vec![Event::Mouse(MouseEvent::new(
                MouseEventKind::Moved,
                0,
                0,
                KeyModifiers::NONE,
            ))],
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
                .with_message(Some(switch))
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

    /// Only panics that end the view or the process give the terminal
    /// back, the view's thread's, the main thread's, the opening thread's
    /// while it opens the view, and the writer's, which can't itself. A
    /// panic in a session's task, which tokio catches, leaves the view as it
    /// is, and so do dictation's and the speech engine's, which recover.
    #[test]
    fn only_panics_that_end_the_view_give_the_terminal_back() {
        assert!(gives_back(Some(THREAD), false, false));
        assert!(gives_back(Some("main"), false, false));
        assert!(gives_back(Some("tokio-rt-worker"), true, false));
        assert!(gives_back(None, false, true));
        for other in [
            Some("tokio-rt-worker"),
            Some("arko-dictation"),
            Some("speech-worker-3"),
            Some("arko-microphone"),
            None,
        ] {
            assert!(!gives_back(other, false, false), "{other:?}");
        }
        assert!(!termelon::is_writer_thread());
        // The names the threads have, and what opening says.
        let dictation = std::thread::Builder::new()
            .name("arko-dictation".into())
            .spawn(|| std::thread::current().name().map(str::to_string))
            .unwrap();
        assert_eq!(dictation.join().unwrap().as_deref(), Some("arko-dictation"));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        let worker = runtime
            .block_on(runtime.spawn(async { std::thread::current().name().map(str::to_string) }))
            .unwrap();
        assert!(!gives_back(worker.as_deref(), false, false));
        assert!(gives_back(worker.as_deref(), true, false));
        assert!(!OPENING.with(std::cell::Cell::get));
        {
            let _opening = Opening::new();
            assert!(OPENING.with(std::cell::Cell::get));
        }
        assert!(!OPENING.with(std::cell::Cell::get));
    }

    /// A copy that only the terminal could take says it went there only
    /// when it did.
    #[test]
    fn a_copy_says_where_it_went() {
        use clipboard::Copied;
        assert_eq!(copy_said(Copied::System, false, "a line"), "Copied a line");
        assert_eq!(
            copy_said(Copied::Terminal, true, "a line"),
            "Sent to the terminal's clipboard"
        );
        assert_eq!(
            copy_said(Copied::Terminal, false, "a line"),
            clipboard::NOT_SENT
        );
        let failed = Copied::Failed("Couldn't copy".into());
        assert_eq!(copy_said(failed, true, "a line"), "Couldn't copy");
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
}

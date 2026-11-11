//! Every screen of the full-screen view, drawn on tuiframe's test terminal
//! and compared with the goldens in `tests/screens/`, one file for each
//! state at each size, named `STATE-WIDTHxHEIGHT.styled`. A golden holds the
//! size, the cursor and every row's characters and styles
//! ([`golden`](super::golden)).
//!
//! The default theme is drawn at 40, 60, 80 and 120 columns (24 rows, and 40
//! rows at 120 columns). The `terminal` theme, which keeps the terminal's
//! own 16 colours, is drawn at 80 by 24 and 120 by 40, and a few states
//! also in the 256 colours of a terminal that has no more and in a light
//! theme.
//!
//! The clocks the screen reads stand still, and every time on it is set
//! from the moment the state is built, so a golden doesn't change from one
//! run to the next. `UPDATE_GOLDENS=1 cargo test --lib tui::inventory`
//! writes the files again and removes those no state makes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tuiframe::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tuiframe::testing::TestTerminal;

use super::app::App;
use super::app::tests as app_tests;
use super::cache;
use super::golden;
use super::keys_menu;
use super::motion;
use super::names_menu;
use super::overlay::{Overlay, Picker, SettingsRow};
use super::resume_picker;
use super::screen;
use super::theme::{self, Depth, Theme};
use super::transcript::View;
use crate::glyphs::{Symbols, tests::choose_symbols};
use crate::remote::{
    Act, ActKind, Activity, AgentStatus, CacheStatus, CallStatus, ContentKind, ConversationEntry,
    Delivery, Item, Kind, Level, Models, Page, PlanStatus, PlanWindow, Reading, Source, Update,
    WorkerState, WorkerSummary,
};

/// The sizes the default theme is drawn at.
const SIZES: &[(u16, u16)] = &[(40, 24), (60, 24), (80, 24), (120, 24), (120, 40)];
/// The sizes of the other looks.
const FEW: &[(u16, u16)] = &[(80, 24), (120, 40)];
/// Too small to draw anything, by width and by height.
const TINY: &[(u16, u16)] = &[(15, 24), (40, 5)];

/// The time the spinner and the highlight of "Working…" stand at, in
/// milliseconds. The highlight is halfway across the word.
const FROZEN_MS: u64 = 1_000;

/// How the screen's colours are chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Look {
    /// The default theme, in 24-bit colour.
    Dark,
    /// A light theme, in 24-bit colour.
    Light,
    /// The default theme in xterm's 256 colours.
    Indexed,
    /// The terminal's own colours, as `NO_COLOR` or a terminal of 16 colours
    /// gets them.
    Terminal,
}

impl Look {
    fn suffix(self) -> &'static str {
        match self {
            Look::Dark => "",
            Look::Light => "-light",
            Look::Indexed => "-256",
            Look::Terminal => "-terminal",
        }
    }
}

/// What a state may use to build itself.
struct Ctx<'a> {
    /// A folder of its own, for what the screen reads from files.
    dir: &'a Path,
    /// The size it is drawn at.
    size: (u16, u16),
}

type Build = fn(&mut App, &Ctx);

struct State {
    name: String,
    look: Look,
    sizes: &'static [(u16, u16)],
    build: Build,
}

fn s(name: &'static str, build: Build) -> (&'static str, Build) {
    (name, build)
}

/// The states, each in the default theme at every size, and in the
/// `terminal` theme at two.
fn states() -> Vec<State> {
    let table = vec![
        s("start-banner", |app, _| start(app)),
        s("input-typed", |app, _| {
            start(app);
            type_text(app, "Fix the failing test in src/parser.rs");
        }),
        s("input-multiline", |app, _| {
            start(app);
            app.paste("Do these in order:\n1. Read the failing test\n2. Fix the parser\n3. Run the whole suite\nThen say in a few words what changed.");
        }),
        s("too-small", |app, _| tour(app, Ending::Done)),
        // The three views of a turn under way, with calls that succeeded,
        // failed and run.
        s("tour-full", |app, _| {
            tour_in(app, View::Full, Ending::Running)
        }),
        s("tour-quiet", |app, _| {
            tour_in(app, View::Quiet, Ending::Running)
        }),
        s("tour-messages", |app, _| {
            tour_in(app, View::Messages, Ending::Running)
        }),
        s("tour-full-top", |app, c| top(app, c, View::Full)),
        s("tour-quiet-top", |app, c| top(app, c, View::Quiet)),
        s("tour-messages-top", |app, c| top(app, c, View::Messages)),
        s("tour-full-opened", |app, _| opened(app, View::Full)),
        s("tour-quiet-opened", |app, _| opened(app, View::Quiet)),
        s("tour-messages-opened", |app, _| opened(app, View::Messages)),
        // Working… after a call ended and before the model answers.
        s("working-full", |app, _| {
            tour_in(app, View::Full, Ending::Working)
        }),
        s("working-quiet", |app, _| {
            tour_in(app, View::Quiet, Ending::Working)
        }),
        s("working-messages", |app, _| {
            tour_in(app, View::Messages, Ending::Working)
        }),
        s("turn-done-full", |app, _| {
            tour_in(app, View::Full, Ending::Done)
        }),
        s("turn-done-quiet", |app, _| {
            tour_in(app, View::Quiet, Ending::Done)
        }),
        s("turn-done-messages", |app, _| {
            tour_in(app, View::Messages, Ending::Done)
        }),
        s("reply-streaming", |app, _| {
            tour_in(app, View::Full, Ending::Streaming)
        }),
        s("thinking-streaming-full", |app, _| {
            tour_in(app, View::Full, Ending::Thinking)
        }),
        s("thinking-streaming-quiet", |app, _| {
            tour_in(app, View::Quiet, Ending::Thinking)
        }),
        s("thinking-folded", |app, _| thinking(app, false)),
        s("thinking-open", |app, _| thinking(app, true)),
        s("long-output-folded", |app, c| long_output(app, c, false)),
        s("long-output-open", |app, c| long_output(app, c, true)),
        s("notices-and-notes", |app, _| notices(app)),
        s("wide-characters", |app, _| wide(app)),
        s("scrolled-up-new-lines", scrolled_up),
        s("selection", selection),
        s("row-focus", row_focus),
        // Workers.
        s("agent-list", |app, _| crew(app)),
        s("agent-list-focused", |app, _| {
            crew(app);
            app.list_focus = Some(1);
        }),
        s("agent-list-many", |app, _| {
            crew(app);
            many_workers(app);
        }),
        s("agent-list-many-focused", |app, _| {
            crew(app);
            many_workers(app);
            app.list_focus = Some(7);
        }),
        s("worker-view-full", |app, _| {
            crew(app);
            app.watch(Some("r1".into()));
        }),
        s("worker-view-quiet", |app, _| {
            crew(app);
            app.set_view(View::Quiet);
            app.watch(Some("r1".into()));
        }),
        s("worker-view-finished", |app, _| {
            crew(app);
            app.watch(Some("r2".into()));
        }),
        s("worker-view-nested", |app, _| {
            crew(app);
            app.watch(Some("r4".into()));
        }),
        s("worker-view-failed", |app, _| {
            crew(app);
            app.watch(Some("r3".into()));
        }),
        // The command menu and the completion of an argument.
        s("menu-commands", |app, _| {
            backdrop(app);
            type_text(app, "/");
        }),
        s("menu-filtered", |app, _| {
            backdrop(app);
            type_text(app, "/wa");
        }),
        s("menu-argument", |app, _| {
            crew(app);
            type_text(app, "/watch ");
        }),
        s("menu-argument-typed", |app, _| {
            crew(app);
            type_text(app, "/watch r");
        }),
        s("menu-argument-hint", |app, _| {
            backdrop(app);
            type_text(app, "/view ");
        }),
        // Overlays and pickers.
        s("help", |app, _| {
            backdrop(app);
            app.overlay = Some(Overlay::Help { scroll: 0 });
        }),
        s("help-scrolled", |app, _| {
            backdrop(app);
            app.overlay = Some(Overlay::Help { scroll: 14 });
        }),
        s("settings", |app, _| settings(app, SettingsRow::View)),
        s("settings-handover", |app, _| {
            settings(app, SettingsRow::Handover)
        }),
        s("settings-theme", |app, _| settings(app, SettingsRow::Theme)),
        s("settings-keys", |app, _| settings(app, SettingsRow::Keys)),
        s("theme-picker", |app, _| theme_picker(app, false)),
        s("flash-notice", |app, _| theme_picker(app, true)),
        s("quit", |app, _| quit(app, 0, false)),
        s("quit-choice-no", |app, _| quit(app, 1, false)),
        s("quit-dont-ask", |app, _| quit(app, 2, true)),
        s("model-picker", |app, _| model_picker(app, false)),
        s("model-picker-efforts", |app, _| {
            model_picker(app, false);
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }),
        s("model-picker-workers", |app, _| model_picker(app, true)),
        s("keys-menu", |app, _| {
            backdrop(app);
            app.open_keys("", None);
        }),
        s("keys-menu-add", |app, _| {
            backdrop(app);
            app.open_keys("add voyage", None);
        }),
        s("openrouter-menu", |app, c| {
            backdrop(app);
            app.info.root = Some(folder(c, "root"));
            app.open_openrouter();
        }),
        s("names-menu", names_menu_open),
        s("resume-picker", |app, _| resume(app)),
        s("page", |app, c| page(app, c, true)),
        s("page-starting", |app, c| page(app, c, false)),
        s("download", |app, _| {
            backdrop(app);
            app.overlay = Some(Overlay::Download {
                model: crate::dictation::models::MODELS[0],
                choice: 0,
            });
        }),
        s("ask-off", |app, _| {
            backdrop(app);
            app.overlay = Some(Overlay::AskOff { row: 0, choice: 1 });
        }),
        s("hint", |app, _| {
            backdrop(app);
            app.say("Copied 14 characters");
        }),
        s("hint-long", |app, _| {
            backdrop(app);
            app.say("/quiet isn't a command. Did you mean /view quiet? Start with a space to send it as a message.");
        }),
        // The answer panel and the approval tray.
        s("questions-panel", |app, _| questions(app, Panel::Open)),
        s("questions-panel-long-choices", |app, _| {
            questions(app, Panel::Long)
        }),
        s("questions-panel-written", |app, _| {
            questions(app, Panel::Written)
        }),
        s("questions-panel-summary", |app, _| {
            questions(app, Panel::Summary)
        }),
        s("questions-chip", |app, _| questions(app, Panel::Chip)),
        s("questions-record", |app, _| questions(app, Panel::Record)),
        s("approvals-tray", |app, c| approvals(app, c, Tray::Ready)),
        s("approvals-tray-waiting", |app, c| {
            approvals(app, c, Tray::Waiting)
        }),
        s("approvals-tray-focused", |app, c| {
            approvals(app, c, Tray::Focused)
        }),
        s("approvals-tray-details", |app, c| {
            approvals(app, c, Tray::Details)
        }),
        // The status line.
        s("status-working", |app, _| status_working(app)),
        s("status-stopping", |app, _| {
            status_working(app);
            app.stopping = true;
        }),
        s("status-plan", |app, _| status_plans(app, false)),
        s("status-plans-two", |app, _| status_plans(app, true)),
        s("status-cache-countdown", |app, _| cache_timer(app, 78_000)),
        s("status-cache-last-minute", |app, _| {
            cache_timer(app, 270_000 - 20_000)
        }),
        s("status-cache-expired", |app, _| cache_timer(app, 400_000)),
        s("status-outside-access", |app, _| {
            tour(app, Ending::Done);
            app.reading.settings.outside = true;
            figures(app);
        }),
        s("status-api-prices", |app, _| {
            status_plans(app, false);
            app.prefs.api_prices = true;
            app.reading.api_price = Some(41.20);
        }),
    ];
    let mut all = Vec::new();
    for (name, build) in &table {
        for (look, sizes) in [(Look::Dark, SIZES), (Look::Terminal, FEW)] {
            all.push(State {
                name: format!("{name}{}", look.suffix()),
                look,
                sizes: if *name == "too-small" { TINY } else { sizes },
                build: *build,
            });
        }
    }
    // A few states in xterm's 256 colours and in a light theme, which have
    // their own colours to check.
    for name in ["tour-full", "tour-quiet", "agent-list", "settings"] {
        let (_, build) = table.iter().find(|(other, _)| *other == name).unwrap();
        for look in [Look::Indexed, Look::Light] {
            all.push(State {
                name: format!("{name}{}", look.suffix()),
                look,
                sizes: FEW,
                build: *build,
            });
        }
    }
    all
}

// ----- drawing -----

pub(super) fn draw(app: &mut App, (width, height): (u16, u16)) -> TestTerminal {
    let mut terminal = golden::terminal(width, height);
    terminal.draw(|frame| screen::draw(frame, app)).unwrap();
    terminal
}

/// The rows of the screen as text, to find what the keys and the mouse act
/// on.
pub(super) fn rows(app: &mut App, size: (u16, u16)) -> Vec<String> {
    draw(app, size).screen().rows()
}

/// An app with nothing in it, drawn in `look`, that reads nothing from the
/// machine it runs on.
pub(super) fn fresh(look: Look) -> App {
    let mut app = app_tests::app();
    app.key_states = keys_menu::tests::states;
    app.login_paths = Arc::new(Vec::<PathBuf>::new);
    app.model_needs = |reachable, name| {
        if name.starts_with("cc:") {
            Some("Claude Code signed in".into())
        } else {
            super::model_picker::needs(reachable, name)
        }
    };
    let (name, depth) = match look {
        Look::Dark => (theme::DEFAULT, Depth::Rgb),
        Look::Light => ("macOS Classic Light", Depth::Rgb),
        Look::Indexed => (theme::DEFAULT, Depth::Ansi256),
        Look::Terminal => ("terminal", Depth::Ansi16),
    };
    app.depth = depth;
    app.theme = if look == Look::Terminal {
        Theme::terminal()
    } else {
        app.catalog.theme(name, depth)
    };
    assert!(
        app.theme.name.eq_ignore_ascii_case(name),
        "{} is not {name}",
        app.theme.name
    );
    app
}

/// Holds the clocks of a screen still until it goes.
pub(super) struct Frozen;

impl Frozen {
    pub(super) fn new() -> Self {
        motion::CHOSEN.set(Some(FROZEN_MS));
        Frozen
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        motion::CHOSEN.set(None);
        cache::tests::NOW.set(None);
    }
}

fn render(state: &State, size: (u16, u16), dir: &Path) -> String {
    let dir = dir.join(format!("{}-{}x{}", state.name, size.0, size.1));
    std::fs::create_dir_all(&dir).unwrap();
    let _frozen = Frozen::new();
    let mut app = fresh(state.look);
    (state.build)(&mut app, &Ctx { dir: &dir, size });
    let mut terminal = golden::terminal(size.0, size.1);
    let drawn = terminal
        .draw(|frame| screen::draw(frame, &mut app))
        .unwrap();
    let buffer = drawn.buffer().clone();
    // The terminal shows the text the frame drew.
    let drew: Vec<String> = super::buffer_lines(&buffer)
        .iter()
        .map(|row| row.trim_end().to_string())
        .collect();
    assert_eq!(terminal.screen().rows(), drew, "{dir:?}");
    golden::styled(&buffer, terminal.cursor().map(|cursor| cursor.position))
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/screens")
}

/// The first line where `old` and `new` differ.
fn first_difference(old: &str, new: &str) -> String {
    let mut old = old.lines();
    let mut new = new.lines();
    let mut at = 1;
    loop {
        match (old.next(), new.next()) {
            (Some(a), Some(b)) if a == b => at += 1,
            (a, b) => {
                return format!(
                    "line {at}: golden {:?}, now {:?}",
                    a.unwrap_or("(none)"),
                    b.unwrap_or("(none)")
                );
            }
        }
    }
}

#[test]
fn every_screen_matches_its_golden() {
    let _symbols = choose_symbols(Symbols::Full);
    let update = std::env::var_os("UPDATE_GOLDENS").is_some();
    let dir = golden_dir();
    let temp = tempfile::tempdir().unwrap();
    let mut seen = HashSet::new();
    let mut wrong = Vec::new();
    for state in states() {
        for &size in state.sizes {
            let file = format!("{}-{}x{}.styled", state.name, size.0, size.1);
            assert!(seen.insert(file.clone()), "{file} is made twice");
            let text = render(&state, size, temp.path());
            if text != render(&state, size, temp.path()) {
                wrong.push(format!("{file} differs between two draws"));
                continue;
            }
            let path = dir.join(&file);
            if update {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, &text).unwrap();
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(golden) if golden == text => {}
                Ok(golden) => wrong.push(format!("{file}, {}", first_difference(&golden, &text))),
                Err(_) => wrong.push(format!("{file} has no golden")),
            }
        }
    }
    let mut stale = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".styled") && !seen.contains(&name) {
            stale.push(name);
        }
    }
    stale.sort();
    if update {
        for name in &stale {
            std::fs::remove_file(dir.join(name)).unwrap();
        }
        return;
    }
    wrong.extend(stale.iter().map(|name| format!("{name} is no state's")));
    assert!(
        wrong.is_empty(),
        "{} goldens differ (UPDATE_GOLDENS=1 writes them again):\n{}",
        wrong.len(),
        wrong
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

// ----- keys and the mouse -----

pub(super) fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.key(KeyEvent::new(code, modifiers));
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, KeyCode::Char(c), KeyModifiers::NONE);
    }
}

fn mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
    app.mouse(MouseEvent::new(kind, column, row, KeyModifiers::NONE));
}

// ----- items -----

pub(super) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Items for the feed, numbered from 1 in the order they are added.
pub(super) struct Items {
    pub(super) list: Vec<Item>,
    pub(super) turn: u64,
    now: i64,
}

impl Items {
    pub(super) fn new(now: i64) -> Self {
        Self {
            list: Vec::new(),
            turn: 1,
            now,
        }
    }

    /// Adds `item` and returns its ID. An item without a time came a while
    /// ago, each half a second after the one before.
    pub(super) fn add(&mut self, mut item: Item) -> u64 {
        let id = self.list.len() as u64 + 1;
        item.id = id;
        item.turn = Some(self.turn);
        if item.at == 0 {
            item.at = self.now - 60_000 + 500 * id as i64;
        }
        self.list.push(item);
        id
    }

    pub(super) fn play(self, app: &mut App) {
        for item in self.list {
            app.update(Update::Item(item), Activity::default);
        }
    }
}

pub(super) fn user(text: &str) -> Item {
    let mut item = Item::new(Kind::User, text);
    item.delivery = Some(Delivery::Turn);
    item
}

pub(super) fn act(kind: ActKind, target: &str) -> Act {
    Act {
        kind,
        target: target.into(),
        ..Act::default()
    }
}

pub(super) fn call(text: &str, status: CallStatus, outcome: Option<&str>, act: Act) -> Item {
    let mut item = Item::new(Kind::Call, text);
    item.status = Some(status);
    item.outcome = outcome.map(str::to_string);
    item.act = Some(act);
    item
}

/// The result of the call `call`, shown under it as `content`.
pub(super) fn content(call: u64, kind: ContentKind, text: &str) -> Item {
    let mut item = Item::new(Kind::Result, text);
    item.content = Some(kind);
    item.call = Some(call);
    item
}

/// A command that ran.
pub(super) fn command(text: &str, status: CallStatus, outcome: Option<&str>, news: &str) -> Item {
    call(
        text,
        status,
        outcome,
        Act {
            news: news.into(),
            ran: true,
            alone: true,
            ..act(ActKind::Command, text.trim_start_matches("$ "))
        },
    )
}

/// The lines of a source file, numbered as a tool numbers them.
fn source_lines(range: std::ops::RangeInclusive<usize>) -> String {
    const CODE: [&str; 9] = [
        "use std::collections::HashMap;",
        "",
        "/// Reads lines until the end of the input.",
        "pub fn parse(input: &str) -> Vec<Token> {",
        "    let mut tokens = Vec::new();",
        "    for line in input.lines() {",
        "        tokens.extend(split(line));",
        "    }",
        "    tokens",
    ];
    range
        .map(|n| format!("{n}\t{}", CODE[n % CODE.len()]))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A file read whose preview is its first 16 lines of 54.
fn long_read(items: &mut Items) {
    let read = items.add(call(
        "read src/parser.rs",
        CallStatus::Ok,
        None,
        act(ActKind::Read, "src/parser.rs"),
    ));
    let mut result = content(read, ContentKind::Lines, &source_lines(1..=16));
    result.language = Some("rs".into());
    result.more = Some("… 38 more lines".into());
    result.whole = Some(Arc::from(source_lines(1..=54)));
    items.add(result);
}

/// The turn that most states show, the user's message, the agent's thinking
/// and words, a file read, an edit, a test run that failed and one that runs
/// (or what `ending` says).
fn tour_items(ending: Ending) -> Items {
    let now = now_ms();
    let mut items = Items::new(now);
    items.add(user("Fix the parser's failing tests"));
    items.add(Item::new(
        Kind::Thinking,
        "**Planning the fix**\nRead the test first, then the parser.\nRun the tests again after the edit.",
    ));
    items.add(Item::new(
        Kind::Text,
        "I'll read the failing test and the parser first.",
    ));
    long_read(&mut items);
    let edit = items.add(call(
        "edit src/parser.rs",
        CallStatus::Ok,
        None,
        act(ActKind::Edit, "src/parser.rs"),
    ));
    let mut diff = content(
        edit,
        ContentKind::Diff,
        " 41\t    let mut end = start;\n-42\t    while self.peek() != '\\n' {\n+42\t    while self.peek().is_some_and(|c| c != '\\n') {\n 43\t        end += 1;",
    );
    diff.language = Some("rs".into());
    items.add(diff);
    let test = items.add(command(
        "$ cargo test --lib parser",
        CallStatus::Failed,
        Some("exit 101 · 1 failed, 3 passed"),
        "1 failed, 3 passed",
    ));
    items.add(content(
        test,
        ContentKind::Plain,
        "running 4 tests\ntest parser::tests::reads ... ok\ntest parser::tests::skips_blank ... FAILED\ntest parser::tests::quotes ... ok\ntest parser::tests::empty ... ok\n\ntest result: FAILED. 3 passed; 1 failed",
    ));
    items.add(Item::new(Kind::Notice, "retrying in 2 s").level(Level::Warn));
    let words = "The parser skipped the blank line at the end of a file. I changed the loop, and now I run the tests again.";
    match ending {
        Ending::Running => {
            items.add(Item::new(Kind::Text, words));
            let mut running = command("$ cargo test", CallStatus::Running, None, "");
            running.at = now - 41_300;
            items.add(running);
        }
        Ending::Working | Ending::Thinking => {
            items.add(Item::new(Kind::Text, words));
            let again = items.add(command(
                "$ cargo test",
                CallStatus::Ok,
                Some("exit 0 · 4 passed"),
                "4 passed",
            ));
            items.add(content(
                again,
                ContentKind::Plain,
                "running 4 tests\ntest result: ok. 4 passed; 0 failed",
            ));
            if ending == Ending::Thinking {
                items.add(Item::new(
                    Kind::Thinking,
                    "**Checking the result**\nAll four pass, so the change holds.",
                ));
            }
        }
        Ending::Streaming => {
            items.add(Item::new(
                Kind::Text,
                "The parser skipped the blank line at the end of a file, so the last test failed. I changed",
            ));
        }
        Ending::Done => {
            items.add(Item::new(Kind::Text, words));
            let again = items.add(command(
                "$ cargo test",
                CallStatus::Ok,
                Some("exit 0 · 4 passed"),
                "4 passed",
            ));
            items.add(content(
                again,
                ContentKind::Plain,
                "running 4 tests\ntest result: ok. 4 passed; 0 failed",
            ));
            items.add(Item::new(
                Kind::Reply,
                "Fixed. The parser keeps the blank line at the end now.\n\n- All **4** tests pass.\n- The edit is in `src/parser.rs`.\n\n```rust\nwhile self.peek().is_some_and(|c| c != '\\n') {\n    end += 1;\n}\n```",
            ));
            items.add(Item::new(Kind::Turn, "2m10s · $0.04"));
        }
    }
    items
}

/// How the turn of [`tour_items`] stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// A call runs.
    Running,
    /// The calls ended, and the model has not answered yet.
    Working,
    /// The reply is being written.
    Streaming,
    /// The agent is thinking again.
    Thinking,
    /// The turn is over.
    Done,
}

/// The agent works, since a minute, as the status line counts it.
pub(super) fn work(app: &mut App, since: i64) {
    app.set_activity(Activity {
        reporting: false,
        working: true,
        since: Some(since),
        question: None,
    });
    app.working_since = Some(Instant::now() - Duration::from_millis(60_300));
}

/// The figures the status line shows once the agent has used the model.
pub(super) fn figures(app: &mut App) {
    app.reading.context_tokens = Some(142_000);
    app.reading.cost = 3.2;
}

fn tour(app: &mut App, ending: Ending) {
    let items = tour_items(ending);
    let since = items.list[0].at;
    let streaming = ending == Ending::Streaming;
    let last = items.list.len() as u64;
    items.play(app);
    if streaming {
        app.update(
            Update::Append {
                id: last,
                text: " the loop to".into(),
            },
            Activity::default,
        );
    }
    if ending == Ending::Done {
        app.last_turn = Some(Duration::from_secs(130));
    } else {
        work(app, since);
    }
    figures(app);
    app.transcript.banner = Some(banner());
}

fn tour_in(app: &mut App, view: View, ending: Ending) {
    tour(app, ending);
    app.set_view(view);
}

fn banner() -> super::Banner {
    super::Banner {
        model: "gpt-6-luna (high effort)".into(),
        workers: "parallel workers".into(),
        workspace: "/home/ada/proj".into(),
        session: "20261110-101010-abcd".into(),
        project: Some("AGENTS.md".into()),
        tools: vec!["2 skills".into(), "1 MCP server".into()],
        model_note: None,
        notes: Vec::new(),
    }
}

/// The empty start, the banner over an empty transcript.
fn start(app: &mut App) {
    app.transcript.banner = Some(banner());
}

/// A short finished conversation for what opens over the transcript.
fn backdrop(app: &mut App) {
    start(app);
    let mut items = Items::new(now_ms());
    items.add(user("Add a README with a usage section"));
    items.add(Item::new(
        Kind::Reply,
        "Done. The README has an install section and a usage example.",
    ));
    items.add(Item::new(Kind::Turn, "34s · $0.01"));
    items.play(app);
    app.last_turn = Some(Duration::from_secs(34));
}

/// The first lines of the tour, after scrolling to the top.
fn top(app: &mut App, ctx: &Ctx, view: View) {
    tour_in(app, view, Ending::Running);
    draw(app, ctx.size);
    app.transcript.top();
}

/// Everything the view can open is open.
fn opened(app: &mut App, view: View) {
    tour_in(app, view, Ending::Running);
    app.transcript.open_for_renderings(view);
}

fn thinking(app: &mut App, open: bool) {
    start(app);
    let mut items = Items::new(now_ms());
    items.add(user("Why does the last test fail?"));
    items.add(Item::new(
        Kind::Thinking,
        "**Reading the failure**\nThe last test expects a blank line at the end of the file.\nThe loop stops at the first newline, so the blank line never counts.\n\nThe fix is in the loop's condition.",
    ));
    items.add(Item::new(
        Kind::Text,
        "The loop in `parse` stops at the first newline, so the blank line at the end is lost.",
    ));
    items.add(Item::new(Kind::Reply, "I can change the loop if you like."));
    items.play(app);
    app.transcript.set_open_thinking(open);
}

fn long_output(app: &mut App, ctx: &Ctx, open: bool) {
    start(app);
    let mut items = Items::new(now_ms());
    items.add(user("Show me the parser, then build it"));
    long_read(&mut items);
    let build = items.add(command("$ cargo build", CallStatus::Ok, Some("exit 0"), ""));
    let lines = |count: usize| {
        (1..=count)
            .map(|n| format!("   Compiling crate{n} v0.1.0"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut output = content(build, ContentKind::Plain, &lines(16));
    output.more = Some("… 97 more lines".into());
    output.whole = Some(Arc::from(lines(113)));
    items.add(output);
    items.play(app);
    if open {
        // The door under the output opens with a click on it.
        let rows = rows(app, ctx.size);
        let y = rows
            .iter()
            .rposition(|row| row.contains("97 more lines"))
            .expect("the door under the output") as u16;
        mouse(app, MouseEventKind::Down(MouseButton::Left), 6, y);
        mouse(app, MouseEventKind::Up(MouseButton::Left), 6, y);
    }
}

fn notices(app: &mut App) {
    start(app);
    let mut items = Items::new(now_ms());
    items.add(user("Update the dependencies"));
    items.add(Item::new(Kind::Text, "Checking which crates are old."));
    items.add(Item::new(Kind::Notice, "handed over to a fresh conversation").level(Level::Info));
    items.add(Item::new(Kind::Notice, "retrying in 2 s").level(Level::Warn));
    items.add(
        Item::new(
            Kind::Notice,
            "The model server answered 500 three times in a row",
        )
        .level(Level::Error),
    );
    items.add(Item::new(
        Kind::Output,
        "make: Nothing to be done for 'all'.",
    ));
    items.add(Item::new(
        Kind::Message,
        "Halfway there. Two crates are left.",
    ));
    let mut note = Item::new(Kind::User, "Keep the old name for the config file");
    note.delivery = Some(Delivery::Note);
    note.source = Some(Source::Web);
    note.pending = true;
    items.add(note);
    let mut queued = Item::new(Kind::User, "Then update the lock file");
    queued.delivery = Some(Delivery::Queued);
    items.add(queued);
    items.play(app);
}

/// Characters that take two columns, cluster of several characters and
/// accents, in the transcript and in the message box.
fn wide(app: &mut App) {
    start(app);
    let mut items = Items::new(now_ms());
    items.add(user("Fix the parser 修复解析器的测试 🎉"));
    items.add(Item::new(
        Kind::Text,
        "日本語のテキストは二列ずつ。A flag 🇯🇵, a heart ❤\u{fe0f}, a family 👨\u{200d}👩\u{200d}👧, a coder 👩\u{200d}💻, an accent e\u{301} and café.",
    ));
    items.add(Item::new(
        Kind::Reply,
        "- 漢字とかなが行の終わりにかかると、次の行に移ります。漢字とかな漢字とかな漢字とかな\n- `日本語` in code",
    ));
    items.play(app);
    app.last_turn = Some(Duration::from_secs(4));
    type_text(app, "漢字 and 日本語 🎉");
}

fn scrolled_up(app: &mut App, ctx: &Ctx) {
    tour(app, Ending::Done);
    draw(app, ctx.size);
    key(app, KeyCode::PageUp, KeyModifiers::NONE);
    draw(app, ctx.size);
    // Lines that come while the reader is up.
    for id in 900..903 {
        let mut item = Item::new(
            Kind::Message,
            format!("A line that came later, number {id}."),
        );
        item.id = id;
        item.at = now_ms();
        app.update(Update::Item(item), Activity::default);
    }
}

fn selection(app: &mut App, ctx: &Ctx) {
    tour(app, Ending::Done);
    let rows = rows(app, ctx.size);
    let y = rows
        .iter()
        .rposition(|row| row.trim_start().starts_with("Fixed."))
        .expect("the reply") as u16;
    let (from, to) = (4, (ctx.size.0 - 4).min(30));
    mouse(app, MouseEventKind::Down(MouseButton::Left), from, y);
    mouse(app, MouseEventKind::Drag(MouseButton::Left), to, y);
    draw(app, ctx.size);
    mouse(app, MouseEventKind::Drag(MouseButton::Left), to, y + 1);
    mouse(app, MouseEventKind::Up(MouseButton::Left), to, y + 1);
}

fn row_focus(app: &mut App, ctx: &Ctx) {
    tour(app, Ending::Done);
    draw(app, ctx.size);
    key(app, KeyCode::Char('g'), KeyModifiers::CONTROL);
    key(app, KeyCode::Up, KeyModifiers::NONE);
}

// ----- workers -----

fn worker(id: &str, state: WorkerState, title: &str, seconds: i64) -> WorkerSummary {
    let now = now_ms();
    let mut worker = WorkerSummary::new(id);
    worker.title = title.into();
    worker.state = state;
    worker.tool_calls = 3 + seconds as u32 / 20;
    // Times end in .3 seconds, so a draw a little later shows the same.
    let began = now - seconds * 1000 - 300;
    worker.started_at = Some(began);
    if state.is_active() {
        worker.running_since = Some(began);
    } else {
        worker.ran_ms = seconds as u64 * 1000;
        worker.ended_at = Some(now - 20_000);
    }
    worker
}

/// The main agent with four workers, r1 that runs and started r4, r2 that
/// finished and r3 that failed, and their lines.
fn crew(app: &mut App) {
    start(app);
    let now = now_ms();
    let mut items = Items::new(now);
    items.add(user("Check the quoting and the lexer, and update the docs"));
    items.add(Item::new(Kind::Text, "I'll start three workers."));
    let line = |worker: &str, title: &str| {
        let mut line = Item::new(Kind::WorkerLine, title);
        line.worker = Some(worker.into());
        line
    };
    items.add(line("r1", "Check the quoting tests"));
    items.add(line("r2", "Read the lexer"));
    items.add(line("r3", "Update the docs"));
    // r1's own turn.
    items.add(Item::new(Kind::Text, "Reading tests/quoting.rs.").agent("r1"));
    let read = items.add(
        call(
            "read tests/quoting.rs",
            CallStatus::Ok,
            None,
            act(ActKind::Read, "tests/quoting.rs"),
        )
        .agent("r1"),
    );
    let mut lines = content(read, ContentKind::Lines, &source_lines(1..=6));
    lines.agent = "r1".into();
    lines.language = Some("rs".into());
    items.add(lines);
    let mut running =
        command("$ cargo test --test quoting", CallStatus::Running, None, "").agent("r1");
    running.at = now - 12_300;
    items.add(running);
    items.add(Item::new(Kind::Thinking, "**Which test**\nThe quoting test first.").agent("r1"));
    // r2's report.
    items.add(
        Item::new(
            Kind::Reply,
            "The lexer splits on spaces only. Quotes are the parser's job.",
        )
        .agent("r2"),
    );
    // r3 failed.
    items.add(
        Item::new(Kind::Notice, "The model server refused the request")
            .level(Level::Error)
            .agent("r3"),
    );
    let since = items.list[0].at;
    items.play(app);
    let mut r1 = worker("r1", WorkerState::Running, "Check the quoting tests", 65);
    r1.last = Some("cargo test --test quoting".into());
    r1.cost_billed = 0.12;
    r1.cost_list = 0.31;
    let mut r2 = worker("r2", WorkerState::Finished, "Read the lexer", 48);
    r2.report = Some("The lexer splits on spaces only. Quotes are the parser's job.".into());
    r2.model = Some("gpt-6-astra".into());
    r2.effort = Some("high".into());
    let mut r3 = worker("r3", WorkerState::Failed, "Update the docs", 9);
    r3.error = Some("The model server refused the request".into());
    r3.last = Some("The model server refused the request".into());
    let mut r4 = worker("r4", WorkerState::Running, "Run the quoting test", 20);
    r4.parent = Some("r1".into());
    r4.depth = 2;
    r4.last = Some("cargo test".into());
    for summary in [r1, r2, r3, r4] {
        app.update(Update::Worker(summary), Activity::default);
    }
    app.reading.agents = vec![
        AgentStatus {
            id: "r1".into(),
            context_tokens: Some(171_000),
            context_window: Some(1_000_000),
            handover_at: Some(190_000),
            compacts: false,
        },
        AgentStatus {
            id: "r4".into(),
            context_tokens: Some(12_000),
            context_window: Some(1_000_000),
            handover_at: Some(190_000),
            compacts: false,
        },
    ];
    work(app, since);
    figures(app);
}

/// Six more workers, so the list counts those it doesn't show.
fn many_workers(app: &mut App) {
    for n in 5..=10 {
        let state = if n % 3 == 0 {
            WorkerState::Finished
        } else {
            WorkerState::Running
        };
        let summary = worker(
            &format!("r{n}"),
            state,
            &format!("Check crate number {n}"),
            30 + n as i64,
        );
        app.update(Update::Worker(summary), Activity::default);
    }
}

// ----- overlays -----

fn folder(ctx: &Ctx, name: &str) -> PathBuf {
    let dir = ctx.dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn settings(app: &mut App, row: SettingsRow) {
    backdrop(app);
    let at = app
        .settings_rows()
        .iter()
        .position(|other| *other == row)
        .expect("the row");
    app.overlay = Some(Overlay::Settings {
        row: at,
        picker: None,
    });
}

fn theme_picker(app: &mut App, notice: bool) {
    backdrop(app);
    let row = app
        .settings_rows()
        .iter()
        .position(|row| *row == SettingsRow::Theme)
        .unwrap();
    let names = app.catalog.names();
    let selected = names
        .iter()
        .position(|(_, name)| name.eq_ignore_ascii_case(&app.theme.name))
        .unwrap_or(0);
    app.overlay = Some(Overlay::Settings {
        row,
        picker: Some(Picker {
            names,
            selected,
            before: app.theme.name.clone(),
            notice: notice.then_some(0),
            moved: None,
        }),
    });
}

fn quit(app: &mut App, choice: usize, dont_ask: bool) {
    crew(app);
    app.ask_quit(vec![
        app_tests::job("cargo watch -x test", Duration::from_millis(150_300)),
        app_tests::job("npm run dev", Duration::from_millis(3_700_300)),
    ]);
    app.overlay = Some(Overlay::Quit { choice, dont_ask });
}

fn model_picker(app: &mut App, workers: bool) {
    backdrop(app);
    app.set_reading(Reading {
        models: Models {
            main: "gpt-6-luna".into(),
            main_effort: "high".into(),
            workers: "gpt-6-luna".into(),
            workers_effort: "high".into(),
            workers_follow: true,
            banner_model: "gpt-6-luna (high effort) via OpenAI API key".into(),
            banner_workers: "parallel workers".into(),
            reachable: vec!["openai".into()],
            openai_auth: Some("OpenAI API key".into()),
            has_workers: true,
            ..Models::default()
        },
        ..Reading::default()
    });
    app.open_model(if workers {
        crate::model_command::Target::Workers
    } else {
        crate::model_command::Target::Main
    });
}

fn names_menu_open(app: &mut App, ctx: &Ctx) {
    use crate::session_names::Way;
    backdrop(app);
    let root = folder(ctx, "root");
    crate::session_names::save(&root, &[Way::ChatGptPlan, Way::ClaudePlan]).unwrap();
    app.info.root = Some(root);
    app.open_names();
    let check = app.names_check().unwrap();
    app.names_checked(&check, names_menu::tests::missing());
}

fn resume(app: &mut App) {
    backdrop(app);
    let at = |hours: i64| {
        resume_picker::tests::noon().with_timezone(&chrono::Utc) - chrono::Duration::hours(hours)
    };
    let entry = |number: usize, title: &str, hours: i64, current: bool| ConversationEntry {
        number,
        title: title.into(),
        last_used: Some(at(hours)),
        current,
        ..ConversationEntry::default()
    };
    let mut picker = resume_picker::ResumePicker::open(vec![
        entry(
            4,
            "Make the login page remember the user between visits, with a cookie that lasts a month",
            0,
            true,
        ),
        entry(3, "Add a README with a usage section", 2, false),
        entry(2, "Fix the build on Windows", 3, false),
        entry(1, "Rename the config file", 6, false),
    ]);
    picker.now = resume_picker::tests::noon();
    app.overlay = Some(Overlay::Resume(picker));
}

fn page(app: &mut App, ctx: &Ctx, running: bool) {
    backdrop(app);
    let dir = folder(ctx, "session");
    std::fs::write(
        dir.join(crate::remote::KEY_FILE),
        "Kq3xv9TnWb2LmzR8cYd5Hs\n",
    )
    .unwrap();
    app.info.session_dir = dir;
    if running {
        app.reading.page = Some(Page {
            addr: "192.168.1.20:8765".parse().unwrap(),
            links: Vec::new(),
            note: None,
        });
    }
    app.overlay = Some(Overlay::Remote {
        link: 0,
        since: Instant::now(),
    });
}

// ----- the answer panel and the approval tray -----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Panel {
    Open,
    Long,
    Written,
    Summary,
    Chip,
    Record,
}

fn question(
    id: &str,
    text: &str,
    choices: &[&str],
    multiple: bool,
) -> crate::questions::AgentQuestion {
    crate::questions::AgentQuestion {
        id: id.into(),
        question: text.into(),
        choices: choices.iter().map(|choice| choice.to_string()).collect(),
        multiple,
        default: Some("Spaces".into()),
        state: crate::questions::QuestionState::Open,
        waits: false,
        item: 7,
        number: None,
    }
}

/// Four questions the main agent asked, three it waits for.
fn questions(app: &mut App, panel: Panel) {
    use crate::questions::QuestionState;
    start(app);
    let feed = crate::remote::Feed::new(crate::agents::MAIN);
    let asked = [
        question("q1", "Tabs or spaces?", &["Spaces", "Tabs"], false),
        question(
            "q2",
            "Which checks?",
            &[
                "Unit tests",
                "Integration tests",
                "Clippy",
                "Formatting",
                "Linux",
                "Windows",
            ],
            true,
        ),
        question(
            "q3",
            "How should I update the configuration?",
            &[
                "Keep the current configuration and preserve all existing comments and ordering",
                "Write a separate example and let each project choose when to migrate",
                "Migrate automatically after making a backup of the original configuration",
            ],
            false,
        ),
        question("q4", "Anything else I should know?", &[], false),
    ];
    feed.ask_questions(
        &asked[..3].iter().map(|q| q.asked()).collect::<Vec<_>>(),
        true,
    );
    feed.ask_questions(&[asked[3].asked()], false);
    if panel == Panel::Record {
        feed.set_question_state(
            "q1",
            QuestionState::Answered {
                answer: "Spaces".into(),
            },
        );
        feed.set_question_state("q2", QuestionState::AnsweredInMessage);
        feed.set_question_state(
            "q3",
            QuestionState::Closed {
                why: Some("The user said to leave it".into()),
            },
        );
        feed.set_question_state("q4", QuestionState::Dismissed);
    }
    app.transcript.reset(feed.subscribe().0);
    app.set_reading(Reading {
        questions: feed.open_questions(),
        ..Reading::default()
    });
    match panel {
        Panel::Open | Panel::Record => {}
        Panel::Long => app.focus_on(2),
        Panel::Written | Panel::Summary => {
            app.focus_on(3);
            app.paste("Keep the branch local until the tests pass on Linux and Windows. Preserve the configuration comments and show the migration diff before writing any files.");
            if panel == Panel::Summary {
                key(app, KeyCode::Enter, KeyModifiers::NONE);
            }
        }
        Panel::Chip => key(app, KeyCode::Esc, KeyModifiers::NONE),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tray {
    /// Shown long enough to answer, the message box has the keys.
    Ready,
    /// Just shown and with the tray's keys, so answering waits half a second.
    Waiting,
    /// Ready, with the tray's keys.
    Focused,
    /// Focused, with the first request's details open.
    Details,
}

fn approvals(app: &mut App, ctx: &Ctx, tray: Tray) {
    use crate::approvals::{
        Approval, ApprovalRequest, BillingApproval, ProfileApproval, Scope, ServerApproval,
        SkillApproval,
    };
    backdrop(app);
    let request = |id: &str, approval: Approval| ApprovalRequest {
        id: id.into(),
        batch: 1,
        approval,
    };
    app.set_reading(Reading {
        approvals: vec![
            request(
                "a1",
                Approval::Skill(SkillApproval {
                    name: "release-notes".into(),
                    scope: Scope::User,
                    description: "How this project writes release notes.".into(),
                    why: "The checklist changed.".into(),
                    workers: true,
                    ..SkillApproval::default()
                }),
            ),
            request(
                "a2",
                Approval::McpServer(ServerApproval {
                    name: "github".into(),
                    scope: Scope::Project,
                    command: "npx -y @modelcontextprotocol/server-github".into(),
                    why: "Read the issues.".into(),
                    workers: true,
                    ..ServerApproval::default()
                }),
            ),
            request(
                "a3",
                Approval::Profile(ProfileApproval {
                    name: "3d-judge".into(),
                    model: "sol".into(),
                    effort: Some("xhigh".into()),
                    scope: "every project".into(),
                    description: "Judges 3D results".into(),
                    limits: vec!["Workers with it get only the skill ponytail.".into()],
                    folder: "Arko's profiles folder".into(),
                    instructions: String::new(),
                    ..ProfileApproval::default()
                }),
            ),
            request(
                "a4",
                Approval::WorkerBilling(BillingApproval {
                    model: "Opus 5.5".into(),
                    input_price: Some(5.0),
                    output_price: Some(25.0),
                    spent: Some(3.2),
                    budget: None,
                    task: "Review the parser".into(),
                }),
            ),
        ],
        ..Reading::default()
    });
    if matches!(tray, Tray::Waiting | Tray::Focused | Tray::Details) {
        key(app, KeyCode::Char('a'), KeyModifiers::ALT);
    }
    if tray == Tray::Waiting {
        return;
    }
    // Drawn once, so the rows count as shown, and then a few seconds later.
    draw(app, ctx.size);
    app.tray.age(5_000);
    if tray == Tray::Details {
        key(app, KeyCode::Char('v'), KeyModifiers::NONE);
    }
}

// ----- the status line -----

fn status_working(app: &mut App) {
    tour(app, Ending::Running);
    app.reading.context_tokens = Some(231_000);
    app.reading.cost = 9.60;
    app.reading.costs.workers.billed = 8.40;
    app.reading.commands = vec!["main".into()];
}

fn plan(name: &str, short: f64, long: f64) -> PlanStatus {
    PlanStatus {
        plan: name.into(),
        windows: [("5 h", short), ("7 d", long)]
            .into_iter()
            .map(|(label, used_percent)| PlanWindow {
                label: label.into(),
                used_percent,
                resets_at: None,
            })
            .collect(),
        credits: None,
        set_aside: 0,
        extra_usage: None,
        text: "not the status line".into(),
    }
}

fn status_plans(app: &mut App, two: bool) {
    tour(app, Ending::Done);
    app.reading.cost = 9.60;
    app.reading.costs.workers.billed = 8.40;
    app.reading.api_price = Some(41.20);
    app.reading.plans = vec![plan("Claude plan", 12.0, if two { 98.0 } else { 23.0 })];
    app.reading.main_plan = Some("Claude plan");
    if two {
        let mut chatgpt = plan("ChatGPT plan", 12.0, 40.0);
        chatgpt.credits = Some(arko_ai::PlanCredits {
            has_credits: true,
            unlimited: false,
            balance: Some("12.503417".into()),
        });
        app.reading.plans.push(chatgpt);
    }
}

/// The idle session whose cache was last used `after` milliseconds before
/// the clock the timer counts from, with a lifetime of five minutes.
fn cache_timer(app: &mut App, after: i64) {
    const AT: i64 = 1_800_000_000_000;
    tour(app, Ending::Done);
    app.reading.cache = CacheStatus {
        lifetime: Some(Duration::from_secs(300)),
        refreshed_at: Some(AT),
        tokens: Some(142_000),
        ..CacheStatus::default()
    };
    cache::tests::NOW.set(Some(AT + after));
}

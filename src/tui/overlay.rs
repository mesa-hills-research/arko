//! What opens over the transcript for a moment: the help, the settings
//! (with the theme picker), the API keys, the web page's QR code, the
//! worker switcher, and the conversations to go back to. Rare actions
//! don't get permanent room; each closes with Esc.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear};

use super::app::App;
use super::theme::{Depth, Theme};
use super::transcript::View;
use super::wrap::{self, width as text_width};
use crate::agents::ExtraRules;
use crate::commands;
use crate::extension_prefs;
use crate::glyphs;
use crate::project;
use crate::remote;

/// What is open over the transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    /// The keys and commands; scrolled by `scroll` lines.
    Help { scroll: usize },
    /// The settings; `row` is the one selected.
    Settings { row: usize, picker: Option<Picker> },
    /// The web page's links as QR codes; `link` is the one shown.
    Remote { link: usize, since: Instant },
    /// `/model`'s picker.
    Model(super::model_picker::ModelPicker),
    /// `/model` once the conversation has started: the main agent keeps
    /// its model, and (`workers`) the workers' picker is offered; `choice`
    /// is the button on.
    ModelNotice {
        text: String,
        workers: bool,
        choice: usize,
    },
    /// The API keys (`/keys`).
    Keys(super::keys_menu::KeysMenu),
    /// OpenRouter's settings (`/openrouter`).
    OpenRouter(super::openrouter_menu::OpenRouterMenu),
    /// How sessions are named (`/session-names`).
    Names(super::names_menu::NamesMenu),
    /// The conversations to go back to (`/resume`).
    Resume(super::resume_picker::ResumePicker),
    /// Quitting while background jobs run, which stop when Arko exits:
    /// `choice` is what is on (0: Yes, 1: No, 2: the box), and `dont_ask`
    /// whether the box is ticked.
    Quit { choice: usize, dont_ask: bool },
    /// Turning off "Ask before adding MCP servers" in the settings, which
    /// asks first: `row` is the settings row to go back to and `choice` the
    /// button on (0: Turn off, 1: Cancel).
    AskOff { row: usize, choice: usize },
}

/// A setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    View,
    Theme,
    Previews,
    Mouse,
    ConfirmQuit,
    ApiPrices,
    Outside,
    Background,
    /// How long after the user's last message monitors' warnings wake the
    /// agent.
    MonitorWakes,
    Replies,
    Rules,
    Page,
    /// The main agent's handover size.
    Handover,
    /// The workers' handover size.
    WorkersHandover,
    Compact,
    ProjectFiles,
    ProjectWorkers,
    /// Whether the agents get the built-in skills at all.
    BuiltinSkills,
    /// One built-in skill, by its place in the session's list of them.
    BuiltinSkill(usize),
    /// Who chooses the workers' models.
    WorkerModels,
    /// Whether workers may run on models billed per token.
    WorkerBilling,
    /// The daily API budget.
    ApiBudget,
    /// How many levels of workers there can be.
    WorkerDepth,
    /// Whether a worker fails after a number of failed model requests in a
    /// row.
    WorkerRetryCap,
    /// Whether the agent that started a worker gets a copy of the user's
    /// messages to it.
    MessageCopies,
    /// Whether agents ask before they add an MCP server.
    AskBeforeMcp,
    /// Whether agents ask before they save a skill in the project too.
    ReviewProjectSkills,
    /// Whether the files agents save in a project's `.arko` folder are kept
    /// out of git.
    KeepProfilesOutOfGit,
    /// Whether profiles are also read from the git repo's root.
    RepoRootProfiles,
    Keys,
    /// The ChatGPT logins, which `/chatgpt` changes.
    Logins,
}

/// The handover sizes ← and → step through, in tokens.
pub const HANDOVER_STEPS: [usize; 9] = [
    100_000, 150_000, 200_000, 250_000, 300_000, 400_000, 500_000, 750_000, 1_000_000,
];

/// The times ← and → step through for when shell commands move to the
/// background, in seconds (0: never).
pub const BACKGROUND_STEPS: [u64; 9] = [0, 60, 120, 300, 600, 900, 1_200, 1_800, 3_600];

/// The theme picker, open in the settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Picker {
    /// Every theme, as (family, name).
    pub names: Vec<(String, String)>,
    pub selected: usize,
    /// The theme when the picker opened: Esc goes back to it.
    pub before: String,
    /// The notice about flashing, the first time the picker opens: the
    /// choice it is on (0: previews, 1: none).
    pub notice: Option<usize>,
    /// When the selection last moved, while its preview waits for it to
    /// rest.
    pub moved: Option<std::time::Instant>,
}

/// A key, as one keycap: ` Ctrl+O `.
pub fn cap(theme: &Theme, key: &str) -> Span<'static> {
    Span::styled(format!(" {key} "), theme.key())
}

/// Keys and what they do, as `cap what  cap what`.
pub fn hints(theme: &Theme, pairs: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (n, (key, what)) in pairs.iter().enumerate() {
        if n > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(cap(theme, key));
        spans.push(Span::styled(format!(" {what}"), theme.dim()));
    }
    spans
}

/// A bordered box in `area`, on the overlay's surface, titled `title`.
pub(super) fn frame_box(frame: &mut Frame, area: Rect, theme: &Theme, title: &str) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.borders())
        .border_style(Style::new().fg(if theme.is_terminal() {
            Color::Reset
        } else {
            theme.border
        }))
        .style(Style::new().bg(theme.raised).fg(theme.text))
        .title(Line::from(Span::styled(
            format!(" {title} "),
            theme.text().add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// A box `width` by `height`, centred in `area` (as much as fits).
pub(super) fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Draws `lines` in `area`, from `scroll` on.
pub(super) fn put(frame: &mut Frame, area: Rect, lines: &[Line<'static>], scroll: usize) {
    for (row, line) in lines
        .iter()
        .skip(scroll)
        .take(area.height as usize)
        .enumerate()
    {
        frame
            .buffer_mut()
            .set_line(area.x, area.y + row as u16, line, area.width);
    }
}

/// Draws the overlay open, over `area` (the transcript's), or over the
/// whole `screen` for the web page's code, which needs the height.
pub fn draw(frame: &mut Frame, app: &App, area: Rect, screen: Rect) {
    let Some(overlay) = &app.overlay else {
        return;
    };
    match overlay {
        Overlay::Help { scroll } => help(frame, app, area, *scroll),
        Overlay::Settings { row, picker } => match picker {
            Some(picker) => theme_picker(frame, app, area, picker),
            None => settings(frame, app, area, *row),
        },
        Overlay::Remote { link, since } => page(frame, app, screen, *link, *since),
        Overlay::Model(picker) => super::model_picker::draw(frame, &app.theme, area, picker),
        Overlay::ModelNotice {
            text,
            workers,
            choice,
        } => super::model_picker::draw_notice(frame, &app.theme, area, text, *choice, *workers),
        Overlay::Keys(menu) => super::keys_menu::draw(frame, &app.theme, area, menu),
        Overlay::OpenRouter(menu) => super::openrouter_menu::draw(frame, &app.theme, area, menu),
        Overlay::Names(menu) => super::names_menu::draw(frame, &app.theme, area, menu),
        Overlay::Resume(picker) => super::resume_picker::draw(frame, &app.theme, area, picker),
        Overlay::Quit { choice, dont_ask } => quit(frame, app, area, *choice, *dont_ask),
        Overlay::AskOff { choice, .. } => ask_off(frame, app, area, *choice),
    }
}

/// The question before "Ask before adding MCP servers" goes off.
fn ask_off(frame: &mut Frame, app: &App, area: Rect, choice: usize) {
    let theme = &app.theme;
    let width = 60.min(area.width.saturating_sub(4)).max(20);
    let inner = width.saturating_sub(4) as usize;
    let mut lines: Vec<Line<'static>> = wrap::wrap(
        &[Span::styled(
            extension_prefs::ASK_MCP_OFF.to_string(),
            theme.text(),
        )],
        inner,
        0,
    )
    .into_iter()
    .map(Line::from)
    .collect();
    lines.push(Line::default());
    let mut buttons = Vec::new();
    for (n, option) in [extension_prefs::TURN_OFF, extension_prefs::CANCEL]
        .into_iter()
        .enumerate()
    {
        if n > 0 {
            buttons.push(Span::raw("  "));
        }
        let style = if n == choice {
            theme.chosen()
        } else {
            theme.key()
        };
        buttons.push(Span::styled(format!(" {option} "), style));
    }
    lines.push(Line::from(buttons));
    let height = (lines.len() as u16 + 4).min(area.height);
    let rect = centred(area, width, height);
    let inner_rect = frame_box(frame, rect, theme, extension_prefs::ASK_MCP_NAME);
    let body = Rect::new(
        inner_rect.x + 1,
        inner_rect.y + 1,
        inner_rect.width.saturating_sub(2),
        inner_rect.height.saturating_sub(2),
    );
    put(frame, body, &lines, 0);
    footer(
        frame,
        rect,
        theme,
        &[("← →", "choose"), ("Enter", "go on"), ("Esc", "cancel")],
    );
}

/// The background jobs listed in the quit popup: more are counted.
const QUIT_JOBS: usize = 5;

/// The popup that asks before Arko quits with background jobs running: what
/// stops (the jobs as the session reads them now, and the workers, which a
/// resumed session continues), Yes, No and the box.
fn quit(frame: &mut Frame, app: &App, area: Rect, choice: usize, dont_ask: bool) {
    let theme = &app.theme;
    let width = 60.min(area.width.saturating_sub(4)).max(20);
    let inner = width.saturating_sub(4) as usize;
    let text = |text: String| {
        wrap::wrap(&[Span::styled(text, theme.text())], inner, 0)
            .into_iter()
            .map(Line::from)
    };
    let jobs = &app.reading.jobs;
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.extend(text(match jobs.len() {
        0 => "The background jobs have finished.".to_string(),
        1 => "1 background job is running. It stops when Arko exits.".to_string(),
        count => format!("{count} background jobs are running. They stop when Arko exits."),
    }));
    if !jobs.is_empty() {
        lines.push(Line::default());
    }
    for job in jobs.iter().take(QUIT_JOBS) {
        let ran = super::transcript::duration(job.started.elapsed().as_millis() as i64);
        let room = inner.saturating_sub(text_width(&ran) + 4);
        let command = job.command.trim().lines().next().unwrap_or_default();
        let command = super::transcript::cut_end(command, room);
        let pad = room.saturating_sub(text_width(&command));
        lines.push(Line::from(vec![
            Span::styled(format!("  {command}"), theme.text()),
            Span::raw(" ".repeat(pad + 2)),
            Span::styled(ran, theme.dim()),
        ]));
    }
    if jobs.len() > QUIT_JOBS {
        lines.push(Line::from(Span::styled(
            format!("  and {} more", jobs.len() - QUIT_JOBS),
            theme.dim(),
        )));
    }
    lines.push(Line::default());
    let workers = app.reading.workers_running.len();
    if workers > 0 {
        lines.extend(text(if workers == 1 {
            "1 worker is running too. It stops as well, and resuming the session lets the agent continue it.".to_string()
        } else {
            format!("{workers} workers are running too. They stop as well, and resuming the session lets the agent continue them.")
        }));
        lines.push(Line::default());
    }
    lines.extend(text("Quit now?".to_string()));
    lines.push(Line::default());
    let style = |n: usize, other: Style| if n == choice { theme.chosen() } else { other };
    let tick = if dont_ask { "[x]" } else { "[ ]" };
    lines.push(Line::from(vec![
        Span::styled(" Yes ", style(0, theme.key())),
        Span::raw("  "),
        Span::styled(" No ", style(1, theme.key())),
        Span::raw("    "),
        Span::styled(format!(" {tick} Don't ask again "), style(2, theme.text())),
    ]));
    let height = (lines.len() as u16 + 4).min(area.height);
    let rect = centred(area, width, height);
    let inner_rect = frame_box(frame, rect, theme, "Quit Arko");
    let body = Rect::new(
        inner_rect.x + 1,
        inner_rect.y + 1,
        inner_rect.width.saturating_sub(2),
        inner_rect.height.saturating_sub(2),
    );
    put(frame, body, &lines, 0);
    footer(
        frame,
        rect,
        theme,
        &[("← →", "move"), ("Enter", "choose"), ("Esc", "stay")],
    );
}

/// What Alt+Q does. Mac terminals send Option as Alt only when set to.
#[cfg(target_os = "macos")]
const ALT_Q: &str = "open the answer panel for the agent's questions. Option+Q needs Option to work as Alt. In the Terminal app, turn on Use Option as Meta key.";
#[cfg(not(target_os = "macos"))]
const ALT_Q: &str = "open the answer panel for the agent's questions";

/// What Alt+A does.
#[cfg(target_os = "macos")]
const ALT_A: &str = "the approval tray, where Space selects, Enter approves the selection, R rejects all and V opens a row's details. Option+A needs Option to work as Alt. In the Terminal app, turn on Use Option as Meta key.";
#[cfg(not(target_os = "macos"))]
const ALT_A: &str = "the approval tray, where Space selects, Enter approves the selection, R rejects all and V opens a row's details";

/// The keys, with why each is what it is in a word, and the commands.
pub const KEYS: &[(&str, &str)] = &[
    (
        "Enter",
        "send (a note while the agent works, else a new turn)",
    ),
    (
        "Shift+Enter",
        "new line, where the terminal tells it from Enter",
    ),
    (
        "Ctrl+J",
        "new line, in every terminal (Alt+Enter too where Alt works)",
    ),
    (
        "↑ ↓",
        "earlier messages from the first or last line, then ↓ goes to the agent list",
    ),
    (
        "Tab",
        "complete a command in the menu, or its argument (↑ ↓ show the others, → takes the one shown), else go to the agent list",
    ),
    ("Ctrl+A E B F", "line start, end, back, forward (readline)"),
    (
        "Ctrl+K U W Y",
        "cut to the end, to the start or a word back, and paste the cut",
    ),
    (
        "PgUp PgDn",
        "scroll the transcript (the wheel and Shift+↑ ↓ too)",
    ),
    (
        "End",
        "back to the bottom, when the box is empty (Ctrl+End always)",
    ),
    (
        "Ctrl+B",
        "move the agent's running call to the background, else back a character",
    ),
    ("Ctrl+O", "the next view (full, quiet, messages)"),
    (
        "Ctrl+T",
        "the agent list, where Tab and Shift+Tab move through it and Enter shows a worker's own work",
    ),
    ("Alt+Q", ALT_Q),
    ("Alt+A", ALT_A),
    (
        "Esc",
        "close, go back, or leave the answer panel to answer in a message",
    ),
    ("Ctrl+C", "stop the turn, else clear the box"),
    ("Ctrl+D", "quit, when the box is empty (as /exit)"),
    ("Ctrl+L", "draw the screen again"),
    ("Drag", "select text, and it is copied when you let go"),
    (
        "Double-click",
        "select a word and copy it (a triple click a line)",
    ),
    (
        "Click",
        "open a worker's line, a row of the agent list, a count of calls, a command's whole output at its \"more lines\", or an agent's thinking at its Thinking line",
    ),
];

fn help(frame: &mut Frame, app: &App, area: Rect, scroll: usize) {
    let theme = &app.theme;
    let width = 78.min(area.width.saturating_sub(2));
    let inner_width = width.saturating_sub(4) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let heading = |text: &str| {
        Line::from(Span::styled(
            text.to_string(),
            theme.accent().add_modifier(Modifier::BOLD),
        ))
    };
    lines.push(heading("Keys"));
    let key_width = KEYS
        .iter()
        .map(|(key, _)| text_width(&glyphs::render(key)) + 2)
        .max()
        .unwrap_or(0);
    for (key, what) in KEYS {
        let key = glyphs::render(key).into_owned();
        let pad = key_width.saturating_sub(text_width(&key) + 2);
        let spans = vec![
            cap(theme, &key),
            Span::raw(" ".repeat(pad + 1)),
            Span::styled(glyphs::render(what).into_owned(), theme.text()),
        ];
        for line in wrap::wrap(&spans, inner_width, key_width + 1) {
            lines.push(Line::from(line));
        }
    }
    lines.push(Line::default());
    lines.push(heading("Commands"));
    for command in commands::COMMANDS {
        let mut spans = vec![Span::styled(
            format!("{:<14}", command.name),
            theme.text().add_modifier(Modifier::BOLD),
        )];
        spans.push(Span::styled(
            glyphs::render(command.about).into_owned(),
            theme.text(),
        ));
        // What may follow it, as the menu shows it on the row chosen.
        if !command.args.is_empty() {
            spans.push(Span::styled(format!("  {}", command.args), theme.dim()));
        }
        for line in wrap::wrap(&spans, inner_width, 14) {
            lines.push(Line::from(line));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Shift-drag uses the terminal's own selection in most terminals. /settings turns the mouse off.",
        theme.dim(),
    )));
    let height = (lines.len() as u16 + 4).min(area.height);
    let rect = centred(area, width, height);
    let inner = frame_box(frame, rect, theme, "Help");
    let body = Rect::new(
        inner.x + 1,
        inner.y + 1,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );
    let wrapped: Vec<Line<'static>> = lines;
    let scroll = scroll.min(wrapped.len().saturating_sub(body.height as usize));
    put(frame, body, &wrapped, scroll);
    footer(frame, rect, theme, &[("Esc", "close"), ("↑ ↓", "scroll")]);
}

/// Keys along the bottom border of `rect`, on the right.
pub(super) fn footer(frame: &mut Frame, rect: Rect, theme: &Theme, pairs: &[(&str, &str)]) {
    let pairs: Vec<(String, &str)> = pairs
        .iter()
        .map(|(key, what)| (glyphs::render(key).into_owned(), *what))
        .collect();
    let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, w)| (k.as_str(), *w)).collect();
    let mut spans = hints(theme, &pairs);
    spans.push(Span::raw(" "));
    let line = Line::from(spans);
    let width = line.width() as u16;
    if width + 4 <= rect.width && rect.height > 1 {
        let x = rect.right() - width - 2;
        frame
            .buffer_mut()
            .set_line(x, rect.bottom() - 1, &line, width);
    }
}

/// A row of choices, the current one marked.
fn choices(theme: &Theme, options: &[&str], current: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (n, option) in options.iter().enumerate() {
        if n > 0 {
            spans.push(Span::raw(" "));
        }
        if n == current {
            spans.push(Span::styled(format!(" {option} "), theme.chosen()));
        } else {
            spans.push(Span::styled(format!(" {option} "), theme.dim()));
        }
    }
    spans
}

/// The width of the label column for a name too long for the usual one.
const WIDE_LABEL: usize = 38;

fn settings(frame: &mut Frame, app: &App, area: Rect, selected: usize) {
    let theme = &app.theme;
    let rows = app.settings_rows();
    let selected = selected.min(rows.len() - 1);
    let page_on = app.reading.page.is_some();
    let label_width = 18;
    let mut lines: Vec<Line<'static>> = Vec::new();
    // The last line of the chosen row's description: kept in view.
    let mut chosen_end = 0;
    let mut section = "";
    for (n, row) in rows.iter().enumerate() {
        let (group, label, value, about): (&str, &str, Vec<Span<'static>>, String) = match row {
            SettingsRow::View => (
                "Transcript",
                "View",
                choices(
                    theme,
                    &["full", "quiet", "messages"],
                    View::ALL.iter().position(|v| *v == app.view).unwrap_or(0),
                ),
                match app.view {
                    View::Full => "Everything, with the workers' calls inline.",
                    View::Quiet => "Each worker as one line that updates in place.",
                    View::Messages => "Just the messages, with a count of each turn's tool calls.",
                }
                .to_string(),
            ),
            SettingsRow::Theme => (
                "Look",
                "Theme",
                vec![
                    Span::styled(format!(" {} ", theme.name), theme.text()),
                    Span::styled(glyphs::render("›").into_owned(), theme.dim()),
                ],
                if app.depth == Depth::Ansi16 {
                    "This terminal shows 16 colours, so its own are used.".to_string()
                } else if app.previews() {
                    "Enter opens the themes, and resting on one previews it.".to_string()
                } else {
                    "Enter opens the themes. A theme shows when you choose it.".to_string()
                },
            ),
            SettingsRow::Previews => (
                "Look",
                "Previews",
                choices(theme, &["on", "off"], usize::from(!app.previews())),
                if app.previews() {
                    "Resting on a theme in the list shows it. Moving between light and dark themes makes the screen flash.".to_string()
                } else {
                    "The themes list changes nothing until you choose a theme with Enter.".to_string()
                },
            ),
            SettingsRow::Mouse => (
                "Look",
                "Mouse",
                choices(theme, &["on", "off"], usize::from(!app.mouse)),
                if app.mouse {
                    "The wheel scrolls, and a drag selects text and copies it.".to_string()
                } else {
                    "The terminal's own selection applies, and PgUp and PgDn scroll.".to_string()
                },
            ),
            SettingsRow::ConfirmQuit => (
                "Quitting",
                "Ask first",
                choices(theme, &["on", "off"], usize::from(!app.confirms_quit())),
                if app.confirms_quit() {
                    "With background jobs running, quitting asks first, since they stop when Arko exits."
                } else {
                    "Quitting doesn't ask about background jobs, which stop when Arko exits."
                }
                .to_string(),
            ),
            SettingsRow::ApiPrices => (
                "Status line",
                "API prices",
                choices(theme, &["on", "off"], usize::from(!app.prefs.api_prices)),
                "Estimate plan-covered calls at API list prices. Billed costs always show."
                    .to_string(),
            ),
            SettingsRow::Outside => (
                "Session",
                "Outside",
                choices(theme, &["on", "off"], usize::from(!app.outside())),
                "Whether the agents may change files outside the working directory.".to_string(),
            ),
            SettingsRow::Handover => (
                "Handover",
                "Main agent",
                vec![
                    Span::styled(format!("{} ", glyphs::render("←")), theme.dim()),
                    Span::styled(format!(" {} ", crate::handover::shown(app.handover_at())), theme.chosen()),
                    Span::styled(format!(" {}", glyphs::render("→")), theme.dim()),
                ],
                if app.cc_compact() {
                    "The input size at which the main agent writes a report of its work and goes on from it, fresh. On cc: models, Claude Code compacts the conversation at that size instead. /handover does it now."
                } else {
                    "The input size at which the main agent writes a report of its work and goes on from it, fresh. /handover does it now."
                }
                .to_string(),
            ),
            SettingsRow::WorkersHandover => (
                "Handover",
                "Workers",
                vec![
                    Span::styled(format!("{} ", glyphs::render("←")), theme.dim()),
                    Span::styled(
                        format!(" {} ", crate::handover::shown(app.workers_handover_at())),
                        theme.chosen(),
                    ),
                    Span::styled(format!(" {}", glyphs::render("→")), theme.dim()),
                ],
                if app.cc_compact() {
                    "The input size the workers hand over at, the running ones from their next step, or Claude Code compacts their conversation on cc: models. A worker on a model with less room goes by the most its model allows."
                } else {
                    "The input size the workers hand over at, the running ones from their next step. A worker on a model with less room goes by the most its model allows."
                }
                .to_string(),
            ),
            SettingsRow::Background => (
                "Session",
                "Background",
                vec![
                    Span::styled(format!("{} ", glyphs::render("←")), theme.dim()),
                    Span::styled(
                        match app.background_after() {
                            0 => " off ".to_string(),
                            after => {
                                format!(" after {} ", crate::shell::shown_background_after(after))
                            }
                        },
                        theme.chosen(),
                    ),
                    Span::styled(format!(" {}", glyphs::render("→")), theme.dim()),
                ],
                match app.background_after() {
                    0 => "Shell commands and other long calls stay in the foreground until they finish. Ctrl+B moves one to the background at once.".to_string(),
                    after => format!(
                        "A shell command or another long call (an MCP tool, index_create) still running after {} moves to the background, where it goes on as a job and the agent goes on with its work. Ctrl+B moves one at once.",
                        crate::shell::background_after_words(after)
                    ),
                },
            ),
            SettingsRow::MonitorWakes => (
                "Session",
                crate::monitors::NAME,
                vec![
                    Span::styled(format!("{} ", glyphs::render("←")), theme.dim()),
                    Span::styled(
                        match app.monitor_wakes() {
                            crate::monitors::Wakes::For(seconds) => {
                                format!(" for {} ", crate::shell::shown_background_after(seconds))
                            }
                            wakes => format!(" {} ", wakes.shown_now(app.monitor_wakes_within())),
                        },
                        theme.chosen(),
                    ),
                    Span::styled(format!(" {}", glyphs::render("→")), theme.dim()),
                ],
                app.monitor_wakes().sentence(app.monitor_wakes_within()),
            ),
            SettingsRow::Replies => (
                "Session",
                "Text replies",
                choices(theme, &["on", "off"], usize::from(!app.text_replies())),
                {
                    let mode = if app.text_replies() {
                        "The agent ends its turn with a plain reply."
                    } else {
                        "The agent ends its turn by calling complete_task."
                    };
                    if app.conversation_started() {
                        format!("{mode} It keeps this for the session.")
                    } else {
                        format!("{mode} It changes before the conversation starts.")
                    }
                },
            ),
            SettingsRow::Rules => (
                "Session",
                "Extra rules",
                choices(
                    theme,
                    &["small", "all", "off"],
                    ExtraRules::ALL
                        .iter()
                        .position(|rules| *rules == app.extra_rules())
                        .unwrap_or(0),
                ),
                {
                    let who = match app.extra_rules() {
                        ExtraRules::Small => {
                            "Agents on small models (Haiku, luna) get a few extra rules in their instructions."
                        }
                        ExtraRules::All => {
                            "Every agent gets the extra rules for small models in its instructions."
                        }
                        ExtraRules::Off => "No agent gets the extra rules for small models.",
                    };
                    if app.conversation_started() {
                        format!("{who} The session keeps this.")
                    } else {
                        format!("{who} It changes before the conversation starts.")
                    }
                },
            ),
            SettingsRow::Compact => (
                "Handover",
                "Claude Code",
                choices(
                    theme,
                    &["compact", "hand over"],
                    usize::from(!app.cc_compact()),
                ),
                {
                    let mode = if app.cc_compact() {
                        "At the handover size, Claude Code compacts the conversation of the agents on cc: models, and they go on in it."
                    } else {
                        "At the handover size, the agents on cc: models write a report and go on from it in a fresh conversation."
                    };
                    if app.conversation_started() {
                        format!("{mode} The session keeps this.")
                    } else {
                        format!("{mode} It changes before the conversation starts.")
                    }
                },
            ),
            SettingsRow::WorkerModels => (
                "Workers",
                crate::workers::MODELS_NAME,
                choices(
                    theme,
                    &crate::agents::ModelChoice::ALL.map(crate::agents::ModelChoice::name),
                    crate::agents::ModelChoice::ALL
                        .iter()
                        .position(|choice| *choice == app.worker_models())
                        .unwrap_or(0),
                ),
                format!(
                    "{} {} {}",
                    crate::workers::MODELS_ABOUT,
                    app.worker_models().about(),
                    crate::workers::MODELS_APPLIES
                ),
            ),
            SettingsRow::WorkerBilling => (
                "Workers",
                crate::workers::BILLING_NAME,
                choices(
                    theme,
                    &crate::workers::WorkerBilling::ALL.map(crate::workers::WorkerBilling::name),
                    crate::workers::WorkerBilling::ALL
                        .iter()
                        .position(|billing| *billing == app.worker_billing())
                        .unwrap_or(0),
                ),
                format!(
                    "{} {} {}",
                    crate::workers::BILLING_ABOUT,
                    app.worker_billing().about(),
                    crate::workers::MODELS_APPLIES
                ),
            ),
            SettingsRow::ApiBudget => (
                "Workers",
                crate::workers::BUDGET_NAME,
                choices(theme, &["on", "off"], usize::from(app.api_budget().is_none())),
                crate::workers::budget_about(app.api_budget()),
            ),
            SettingsRow::WorkerDepth => (
                "Workers",
                crate::workers::DEPTH_NAME,
                vec![
                    Span::styled(format!("{} ", glyphs::render("←")), theme.dim()),
                    Span::styled(format!(" {} ", app.worker_depth()), theme.chosen()),
                    Span::styled(format!(" {}", glyphs::render("→")), theme.dim()),
                ],
                crate::workers::depth_about().to_string(),
            ),
            SettingsRow::WorkerRetryCap => (
                "Workers",
                crate::workers::RETRY_CAP_NAME,
                choices(
                    theme,
                    &["on", "off"],
                    usize::from(app.worker_retry_cap().is_none()),
                ),
                crate::workers::retry_cap_about(app.worker_retry_cap()),
            ),
            SettingsRow::MessageCopies => (
                "Workers",
                crate::workers::COPIES_NAME,
                choices(theme, &["on", "off"], usize::from(!app.message_copies())),
                crate::workers::copies_about(app.message_copies()).to_string(),
            ),
            SettingsRow::AskBeforeMcp => (
                "Approvals",
                extension_prefs::ASK_MCP_NAME,
                choices(
                    theme,
                    &["on", "off"],
                    usize::from(!app.reading.settings.ask_before_adding_mcp),
                ),
                extension_prefs::ASK_MCP_ABOUT.to_string(),
            ),
            SettingsRow::ReviewProjectSkills => (
                "Approvals",
                extension_prefs::REVIEW_SKILLS_NAME,
                choices(
                    theme,
                    &["on", "off"],
                    usize::from(!app.reading.settings.review_project_skills),
                ),
                extension_prefs::REVIEW_SKILLS_ABOUT.to_string(),
            ),
            SettingsRow::KeepProfilesOutOfGit => (
                "Project files",
                extension_prefs::KEEP_OUT_OF_GIT_NAME,
                choices(
                    theme,
                    &["on", "off"],
                    usize::from(!app.reading.settings.keep_agents_files_out_of_git),
                ),
                extension_prefs::KEEP_OUT_OF_GIT_ABOUT.to_string(),
            ),
            SettingsRow::RepoRootProfiles => (
                "Worker profiles",
                "Repo root",
                choices(
                    theme,
                    &["on", "off"],
                    usize::from(!app.profile_prefs.repo_root),
                ),
                if app.profile_prefs.repo_root {
                    "In a subfolder of a git repo, the profiles in the repo root's .arko folder are read too."
                } else {
                    "Only the profiles in the project's own .arko folder are read."
                }
                .to_string(),
            ),
            SettingsRow::Page => (
                "Session",
                "Web page",
                choices(theme, &["on", "off"], usize::from(!page_on)),
                "Follow and drive this session from a browser (/remote shows its code)."
                    .to_string(),
            ),
            SettingsRow::ProjectFiles => (
                "Project instructions",
                "Files",
                choices(
                    theme,
                    &["one", "both", "off"],
                    project::Files::ALL
                        .iter()
                        .position(|files| *files == app.project_instructions())
                        .unwrap_or(0),
                ),
                match app.project_instructions() {
                    project::Files::One => {
                        "The agents get AGENTS.md, or CLAUDE.md when there is none. The main agent gets it once per conversation, in its first message. A change reaches the conversations and workers that start after it."
                    }
                    project::Files::Both => {
                        "The agents get AGENTS.md and CLAUDE.md, each under its own name. The main agent gets them once per conversation, in its first message. A change reaches the conversations and workers that start after it."
                    }
                    project::Files::Off => {
                        "The agents get neither AGENTS.md nor CLAUDE.md. A change reaches the conversations and workers that start after it."
                    }
                }
                .to_string(),
            ),
            SettingsRow::ProjectWorkers => (
                "Project instructions",
                "Workers",
                choices(
                    theme,
                    &["on", "off"],
                    usize::from(!app.worker_project_instructions()),
                ),
                if app.worker_project_instructions() {
                    "Each worker gets the same files once, in its first message."
                } else {
                    "Only the main agent gets the files."
                }
                .to_string(),
            ),
            SettingsRow::BuiltinSkills => (
                "Skills",
                crate::builtin_skills::SETTING_NAME,
                choices(theme, &["on", "off"], usize::from(!app.builtin_skills())),
                if app.conversation_started() {
                    format!(
                        "{} {}",
                        crate::builtin_skills::SETTING_ABOUT,
                        crate::builtin_skills::AFTER_START
                    )
                } else {
                    crate::builtin_skills::SETTING_ABOUT.to_string()
                },
            ),
            SettingsRow::BuiltinSkill(index) => {
                let skill = app.builtin_skill(*index);
                let name = skill.map_or("", |skill| skill.name.as_str());
                (
                    "Skills",
                    name,
                    choices(
                        theme,
                        &["on", "off"],
                        usize::from(skill.is_some_and(|skill| !skill.on)),
                    ),
                    {
                        let about = crate::builtin_skills::find(name)
                            .map_or_else(String::new, |skill| skill.description());
                        if app.conversation_started() {
                            format!("{about} {}", crate::builtin_skills::AFTER_START)
                        } else {
                            about
                        }
                    },
                )
            }
            SettingsRow::Keys => {
                let states = (app.key_states)();
                let set = states.iter().filter(|state| state.masked.is_some()).count();
                (
                    "Providers",
                    "API keys",
                    vec![
                        Span::styled(format!(" {set} of {} set ", states.len()), theme.text()),
                        Span::styled(glyphs::render("›").into_owned(), theme.dim()),
                    ],
                    "The model providers' keys, and Voyage's for the search indexes. Enter opens them."
                        .to_string(),
                )
            }
            SettingsRow::Logins => {
                let logins = (app.login_paths)();
                let list = logins
                    .iter()
                    .enumerate()
                    .map(|(n, path)| format!("{} {}", n + 1, path.display()))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let about = match logins.is_empty() {
                    true => "None. /chatgpt add PATH adds one.".to_string(),
                    false => format!("{list}. /chatgpt adds, removes and orders them."),
                };
                (
                    "Providers",
                    "ChatGPT logins",
                    vec![Span::styled(format!(" {} ", logins.len()), theme.text())],
                    about,
                )
            }
        };
        if group != section {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                group.to_string(),
                theme.dim().add_modifier(Modifier::BOLD),
            )));
            section = group;
        }
        let chosen = n == selected;
        let marker = if chosen {
            Span::styled(format!("{} ", glyphs::render("▸")), theme.accent())
        } else {
            Span::raw("  ")
        };
        let label_style = if chosen {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.text()
        };
        // A name longer than the column (the approvals' sentences) takes the
        // wide one, so its choices line up with those of the rows beside it.
        let column = if text_width(label) > label_width {
            WIDE_LABEL
        } else {
            label_width
        };
        let mut spans = vec![
            marker,
            Span::styled(format!("{label:<column$}"), label_style),
        ];
        spans.extend(value);
        lines.push(Line::from(spans));
        if chosen {
            let room = (area.width.min(72) as usize)
                .saturating_sub(label_width + 8)
                .max(10);
            for line in wrap::wrap(&[Span::styled(about, theme.dim())], room, 0) {
                let mut spans = vec![Span::raw(" ".repeat(label_width + 2))];
                spans.extend(line);
                lines.push(Line::from(spans));
            }
            chosen_end = lines.len() - 1;
        }
    }
    let width = 72.min(area.width.saturating_sub(2));
    let height = (lines.len() as u16 + 4).min(area.height);
    let rect = centred(area, width, height);
    let inner = frame_box(frame, rect, theme, "Settings");
    let body = Rect::new(
        inner.x + 1,
        inner.y + 1,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );
    // A short terminal scrolls the rows, so the chosen one shows whole.
    let scroll = (chosen_end + 1).saturating_sub(body.height as usize);
    put(frame, body, &lines, scroll);
    footer(
        frame,
        rect,
        theme,
        &[("↑ ↓", "choose"), ("← →", "change"), ("Esc", "close")],
    );
}

fn theme_picker(frame: &mut Frame, app: &App, area: Rect, picker: &Picker) {
    let theme = &app.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_line = 0;
    let mut family = "";
    for (n, (group, name)) in picker.names.iter().enumerate() {
        if group != family {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                group.clone(),
                theme.dim().add_modifier(Modifier::BOLD),
            )));
            family = group;
        }
        if n == picker.selected {
            selected_line = lines.len();
            lines.push(Line::from(Span::styled(
                format!(" {name} "),
                theme.chosen(),
            )));
        } else {
            lines.push(Line::from(Span::styled(format!(" {name} "), theme.text())));
        }
    }
    let width = 48.min(area.width.saturating_sub(2));
    let height = area.height.saturating_sub(2).max(5);
    let rect = centred(area, width, height);
    let inner = frame_box(frame, rect, theme, "Theme");
    let body = Rect::new(
        inner.x + 1,
        inner.y + 1,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );
    let visible = body.height as usize;
    let scroll = selected_line
        .saturating_sub(visible / 2)
        .min(lines.len().saturating_sub(visible));
    put(frame, body, &lines, scroll);
    footer(frame, rect, theme, &[("Enter", "keep"), ("Esc", "back")]);
    if let Some(choice) = picker.notice {
        flash_notice(frame, app, area, choice);
    }
}

/// The notice the theme picker opens with, once: previews can flash. It
/// sits in front of the picker (a little wider, so it reads as its own
/// box), comes before any preview, and offers the two ways on.
fn flash_notice(frame: &mut Frame, app: &App, over: Rect, choice: usize) {
    let theme = &app.theme;
    let width = 56.min(over.width.saturating_sub(4)).max(20);
    let inner = width.saturating_sub(4) as usize;
    let text = "Resting on a theme in the list shows it. Moving between light and dark themes makes the whole screen flash. If flashing light affects you, choose No previews. Settings can change this later.";
    let mut lines: Vec<Line<'static>> = wrap::wrap(&[Span::styled(text, theme.text())], inner, 0)
        .into_iter()
        .map(Line::from)
        .collect();
    lines.push(Line::default());
    let options = ["Preview themes", "No previews"];
    let mut row = Vec::new();
    for (n, option) in options.iter().enumerate() {
        if n > 0 {
            row.push(Span::raw("  "));
        }
        let style = if n == choice {
            theme.chosen()
        } else {
            theme.key()
        };
        row.push(Span::styled(format!(" {option} "), style));
    }
    lines.push(Line::from(row));
    let height = (lines.len() as u16 + 4).min(over.height);
    let rect = centred(over, width, height);
    let inner_rect = frame_box(frame, rect, theme, "Theme previews");
    let body = Rect::new(
        inner_rect.x + 1,
        inner_rect.y + 1,
        inner_rect.width.saturating_sub(2),
        inner_rect.height.saturating_sub(2),
    );
    put(frame, body, &lines, 0);
    footer(
        frame,
        rect,
        theme,
        &[("← →", "choose"), ("Enter", "go on"), ("Esc", "back")],
    );
}

/// The web page's links, ranked, while it runs.
pub fn page_links(app: &App) -> Option<Vec<remote::Link>> {
    let addr = app.reading.page.as_ref()?.addr;
    let key = remote::session_key(&app.info.session_dir.join(remote::KEY_FILE)).ok()?;
    Some(remote::links(addr, &key).0)
}

fn page(frame: &mut Frame, app: &App, area: Rect, link: usize, since: Instant) {
    let theme = &app.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let links = page_links(app);
    let warning = Line::from(Span::styled(
        "The link holds the session's key. The page isn't encrypted, so on a network you don't trust, use an SSH tunnel or Tailscale.",
        theme.warning(),
    ));
    match links {
        None => {
            let text = if since.elapsed().as_secs() < 5 {
                "Starting the web page…"
            } else {
                "The web page is off. /remote starts it (what went wrong is in the transcript)."
            };
            lines.push(Line::from(Span::styled(
                glyphs::render(text).into_owned(),
                theme.dim(),
            )));
        }
        Some(links) => {
            let link = link.min(links.len().saturating_sub(1));
            let shown = &links[link];
            let qr = (glyphs::symbols() != glyphs::Symbols::Ascii)
                .then(|| remote::qr_lines(&shown.url))
                .flatten();
            // The code, the link and what it is, in its borders.
            let room = area.height.saturating_sub(4) as usize;
            let mut code_shown = false;
            match qr {
                Some(code)
                    if code.len() <= room && code[0].chars().count() + 4 <= area.width as usize =>
                {
                    // Dark modules on light, whatever the theme: as
                    // scanners read it.
                    let style = if theme.is_terminal() || app.depth != Depth::Rgb {
                        Style::new().fg(Color::White).bg(Color::Black)
                    } else {
                        Style::new()
                            .fg(Color::Rgb(255, 255, 255))
                            .bg(Color::Rgb(0, 0, 0))
                    };
                    for row in code {
                        lines.push(Line::from(Span::styled(row, style)));
                    }
                    code_shown = true;
                }
                Some(_) => lines.push(Line::from(Span::styled(
                    "(A larger window shows the QR code.)",
                    theme.dim(),
                ))),
                None => {}
            }
            lines.push(Line::from(Span::styled(shown.url.clone(), theme.text())));
            // Ranked: the local network first, VPNs last.
            let mut about = vec![Span::styled(
                match &shown.interface {
                    Some(name) if !shown.loopback => {
                        glyphs::render(&format!("{name} · {}", shown.kind.label())).into_owned()
                    }
                    _ => shown.kind.label().to_string(),
                },
                theme.dim(),
            )];
            if links.len() > 1 {
                about.push(Span::styled(
                    glyphs::render(&format!(" · {} of {}", link + 1, links.len())).into_owned(),
                    theme.dim(),
                ));
            }
            lines.push(Line::from(about));
            // The warning, when there is room (the transcript has it too).
            if !code_shown || lines.len() + 2 + 4 <= area.height as usize {
                lines.push(warning);
            }
        }
    }
    // As wide as the code and the link: the warning wraps.
    let content = lines
        .iter()
        .filter(|line| {
            line.spans
                .first()
                .is_none_or(|span| span.style != theme.warning())
        })
        .map(|line| line.width())
        .max()
        .unwrap_or(20) as u16;
    let width = (content + 4).clamp(44.min(area.width), area.width.max(1));
    // A long line wraps.
    let inner_width = width.saturating_sub(4) as usize;
    let lines: Vec<Line<'static>> = lines
        .into_iter()
        .flat_map(|line| {
            if line.width() <= inner_width {
                vec![line]
            } else {
                wrap::wrap(&line.spans, inner_width, 0)
                    .into_iter()
                    .map(Line::from)
                    .collect()
            }
        })
        .collect();
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = centred(area, width, height);
    let inner = frame_box(frame, rect, theme, "Web page");
    let body = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    put(frame, body, &lines, 0);
    footer(
        frame,
        rect,
        theme,
        &[("← →", "other links"), ("Esc", "close")],
    );
}

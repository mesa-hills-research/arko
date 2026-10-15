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
use crate::glyphs;
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
    /// The conversations to go back to (`/resume`).
    Resume(super::resume_picker::ResumePicker),
}

/// A setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    View,
    Theme,
    Previews,
    Mouse,
    Outside,
    Handover,
    Replies,
    Rules,
    Page,
    Keys,
}

/// The handover sizes ← and → step through, in tokens.
pub const HANDOVER_STEPS: [usize; 9] = [
    100_000, 150_000, 200_000, 250_000, 300_000, 400_000, 500_000, 750_000, 1_000_000,
];

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
        Overlay::Resume(picker) => super::resume_picker::draw(frame, &app.theme, area, picker),
    }
}

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
    ("Tab", "complete a command in the menu"),
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
    ("Ctrl+O", "next view: full, quiet, messages"),
    ("Ctrl+T", "the agent list: Enter shows a worker's own work"),
    ("Esc", "close, go back, or put the agent's question off"),
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
        "open a worker's line, a row of the agent list or a count of calls",
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
            Span::styled(what.to_string(), theme.text()),
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
        spans.push(Span::styled(command.about.to_string(), theme.text()));
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

fn settings(frame: &mut Frame, app: &App, area: Rect, selected: usize) {
    let theme = &app.theme;
    let rows = app.settings_rows();
    let selected = selected.min(rows.len() - 1);
    let page_on = super::page().is_some();
    let label_width = 14;
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
                    "This terminal shows 16 colours: its own are used.".to_string()
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
            SettingsRow::Outside => (
                "Session",
                "Outside",
                choices(theme, &["on", "off"], usize::from(!app.outside())),
                "Whether the agents may change files outside the working directory.".to_string(),
            ),
            SettingsRow::Handover => (
                "Session",
                "Handover at",
                vec![
                    Span::styled(format!("{} ", glyphs::render("←")), theme.dim()),
                    Span::styled(format!(" {}k ", app.handover_at() / 1000), theme.chosen()),
                    Span::styled(format!(" {}", glyphs::render("→")), theme.dim()),
                ],
                "The input size the main agent hands over at: it writes a report of its work and goes on from it, fresh. On cc: models, Claude Code compacts the conversation at that size instead. /handover does it now."
                    .to_string(),
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
            SettingsRow::Page => (
                "Session",
                "Web page",
                choices(theme, &["on", "off"], usize::from(!page_on)),
                "Follow and drive this session from a browser (/remote shows its code)."
                    .to_string(),
            ),
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
        let mut spans = vec![
            marker,
            Span::styled(format!("{label:<label_width$}"), label_style),
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
    let addr = super::page()?;
    let key = remote::session_key(&app.info.session_dir.join(remote::KEY_FILE)).ok()?;
    Some(remote::links(addr, &key).0)
}

fn page(frame: &mut Frame, app: &App, area: Rect, link: usize, since: Instant) {
    let theme = &app.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let links = page_links(app);
    let warning = Line::from(Span::styled(
        "The link holds the session's key. No TLS: on a network you don't trust, use an SSH tunnel or Tailscale.",
        theme.warning(),
    ));
    match links {
        None => {
            let text = if since.elapsed().as_secs() < 5 {
                "Starting the web page…"
            } else {
                "The web page is off: /remote starts it (what went wrong is in the transcript)."
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
                    Some(name) if !shown.loopback => format!("{name} · {}", shown.kind.label()),
                    _ => shown.kind.label().to_string(),
                },
                theme.dim(),
            )];
            if links.len() > 1 {
                about.push(Span::styled(
                    format!(" · {} of {}", link + 1, links.len()),
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

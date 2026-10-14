//! What opens over the transcript for a moment: the help, the settings
//! (with the theme picker), the web page's QR code, and the worker
//! switcher. Rare actions don't get permanent room; each closes with Esc.

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
use crate::commands::{self, Runs};
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
}

/// A setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    View,
    Theme,
    Mouse,
    Outside,
    Page,
}

/// The theme picker, open in the settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Picker {
    /// Every theme, as (family, name).
    pub names: Vec<(String, String)>,
    pub selected: usize,
    /// The theme when the picker opened: Esc goes back to it.
    pub before: String,
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
fn frame_box(frame: &mut Frame, area: Rect, theme: &Theme, title: &str) -> Rect {
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
fn centred(area: Rect, width: u16, height: u16) -> Rect {
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
fn put(frame: &mut Frame, area: Rect, lines: &[Line<'static>], scroll: usize) {
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
    ("↑ ↓", "earlier messages, from the first or last line"),
    ("Ctrl+A E B F", "line start, end, back, forward (readline)"),
    (
        "Ctrl+K U W Y",
        "cut to end, to start, word back; paste the cut",
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
    ("Ctrl+T", "the workers: watch one's own work"),
    ("Esc", "close, go back, or put the agent's question off"),
    (
        "Ctrl+C",
        "copy the selected message, else stop the turn, else clear",
    ),
    ("Ctrl+D", "quit, when the box is empty (as /exit)"),
    ("Ctrl+L", "draw the screen again"),
    (
        "Click",
        "select a message to copy; a worker's line opens it",
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
        if command.runs == Runs::Screen {
            spans.push(Span::styled(" (this view)".to_string(), theme.dim()));
        }
        for line in wrap::wrap(&spans, inner_width, 15) {
            lines.push(Line::from(line));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Shift-drag selects text natively in most terminals while the mouse scrolls; /settings turns the mouse off.",
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
fn footer(frame: &mut Frame, rect: Rect, theme: &Theme, pairs: &[(&str, &str)]) {
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
                    View::Full => "Everything, the workers' calls too.",
                    View::Quiet => "The main agent's work; a line for each worker.",
                    View::Messages => "What was said; a line counts each turn's tools.",
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
                } else {
                    "Enter opens the themes; moving through them previews each.".to_string()
                },
            ),
            SettingsRow::Mouse => (
                "Look",
                "Mouse",
                choices(theme, &["on", "off"], usize::from(!app.mouse)),
                if app.mouse {
                    "The wheel scrolls; click selects; Shift-drag selects text.".to_string()
                } else {
                    "The terminal selects text; PgUp and PgDn scroll.".to_string()
                },
            ),
            SettingsRow::Outside => (
                "Session",
                "Outside",
                choices(theme, &["on", "off"], usize::from(!app.outside())),
                "Whether the agents may change files outside the working directory.".to_string(),
            ),
            SettingsRow::Page => (
                "Session",
                "Web page",
                choices(theme, &["on", "off"], usize::from(!page_on)),
                "Follow and drive this session from a browser (/remote shows its code)."
                    .to_string(),
            ),
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
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(label_width + 2)),
                Span::styled(about, theme.dim()),
            ]));
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
    put(frame, body, &lines, 0);
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
}

fn page(frame: &mut Frame, app: &App, area: Rect, link: usize, since: Instant) {
    let theme = &app.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let links = super::page().and_then(|addr| {
        let key = remote::session_key(&app.info.session_dir.join(remote::KEY_FILE)).ok()?;
        Some(remote::links(addr, &key).0)
    });
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
    let content = lines.iter().map(|line| line.width()).max().unwrap_or(20) as u16;
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

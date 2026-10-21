//! The screen's layout and its quiet parts: from the bottom, the input
//! box, the status line above it, and above that what docks there for a
//! while (the agent's question, the command menu, the worker switcher);
//! the transcript takes the rest. The transcript is what the reader is
//! there for: everything else stays small.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear};

use super::app::{App, ListRow};
use super::markdown;
use super::overlay::{self, hints};
use super::select::Docked;
use super::transcript::{self, View};
use super::wrap::{self, width as text_width};
use crate::glyphs;

/// The input box grows to this many lines, then scrolls.
const INPUT_LINES: usize = 8;
/// The command menu shows this many commands at most.
const MENU_LINES: usize = 8;

/// The agent list shows this many rows at most: the main agent, five
/// workers and the count of the others.
const LIST_LINES: usize = 7;

/// Draws the whole screen.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let theme = app.theme.clone();
    frame.buffer_mut().set_style(area, theme.base());
    if area.height < 6 || area.width < 16 {
        return;
    }

    // From the bottom: the agent list (while there are workers), the
    // input box between two rules, the status line.
    let rows = app.list_rows(LIST_LINES.min(area.height as usize / 4));
    let list_rows = rows.len() as u16;
    let text_width = (area.width as usize).saturating_sub(4).max(4);
    app.input_width = text_width;
    let mut input = app.editor.lines(text_width, theme.text(), theme.key());
    let (mut cursor_row, cursor_column) = app.editor.position(text_width);
    let most = INPUT_LINES.min(area.height as usize / 4).max(1);
    if input.len() > most {
        // Scrolled to keep the cursor in sight.
        let first = cursor_row.saturating_sub(most - 1).min(input.len() - most);
        input = input.into_iter().skip(first).take(most).collect();
        cursor_row -= first;
    }
    let input_height = input.len().max(1) as u16;
    let list = Rect::new(area.x, area.bottom() - list_rows, area.width, list_rows);
    let input_area = Rect::new(
        area.x,
        list.y - input_height - 2,
        area.width,
        input_height + 2,
    );
    draw_input(frame, app, input_area, &input, (cursor_row, cursor_column));
    if list_rows > 0 {
        draw_list(frame, app, list, rows);
    } else {
        app.list_drawn = Default::default();
    }
    let status_y = input_area.y - 1;
    draw_status(frame, app, Rect::new(area.x, status_y, area.width, 1));

    // What docks above the status line for a while.
    let above = Rect::new(area.x, area.y, area.width, status_y - area.y);
    let previous = app.docked.take();
    let docked = if !app.menu_items().is_empty() {
        draw_menu(frame, app, above)
    } else if app.question_shown().is_some() {
        draw_question(frame, app, above, previous)
    } else {
        0
    };

    let transcript = Rect::new(above.x, above.y, above.width, above.height - docked);
    let filter = app.filter();
    app.transcript
        .draw(frame.buffer_mut(), transcript, &filter, &theme);
    overlay::draw(frame, app, transcript, area);
    app.after_draw();
}

/// The input box: the text between two rules the width of the screen (no
/// box: the screen's edges are its sides).
fn draw_input(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    lines: &[Vec<Span<'static>>],
    cursor: (usize, usize),
) {
    let theme = &app.theme;
    let rule_style = if theme.is_terminal() {
        theme.dim()
    } else {
        Style::new().fg(theme.border)
    };
    let rule = Line::from(Span::styled(
        glyphs::render("─").repeat(area.width as usize),
        rule_style,
    ));
    frame
        .buffer_mut()
        .set_line(area.x, area.y, &rule, area.width);
    frame
        .buffer_mut()
        .set_line(area.x, area.bottom() - 1, &rule, area.width);
    let rect = Rect::new(area.x, area.y + 1, area.width, area.height - 2);
    let height = rect.height;
    let prompt = glyphs::render("› ").into_owned();
    let answering = app.activity.question.is_some();
    let prompt_style = if app.list_focus.is_some() {
        theme.dim()
    } else if answering {
        theme.accent().add_modifier(Modifier::BOLD)
    } else {
        theme.accent()
    };
    frame.buffer_mut().set_line(
        rect.x + 1,
        rect.y,
        &Line::from(Span::styled(prompt, prompt_style)),
        2,
    );
    let x = rect.x + 3;
    let width = rect.width.saturating_sub(4);
    if app.editor.is_empty() {
        let placeholder = if answering {
            "Answer the agent's question".to_string()
        } else if let Some(id) = &app.watching {
            format!("Message {id}")
        } else if app.activity.working {
            "Add to the task".to_string()
        } else {
            "Message".to_string()
        };
        let used = text_width(&placeholder);
        let mut spans = vec![Span::styled(placeholder, theme.dim())];
        // How to find the rest, while there is room: on an empty box only.
        let hint = "  / for commands";
        if !app.activity.working && used + text_width(hint) < width as usize {
            spans.push(Span::styled(hint.to_string(), theme.dim()));
        }
        frame
            .buffer_mut()
            .set_line(x, rect.y, &Line::from(spans), width);
    } else {
        for (row, spans) in lines.iter().enumerate() {
            frame
                .buffer_mut()
                .set_line(x, rect.y + row as u16, &Line::from(spans.clone()), width);
        }
        // A command's argument: what completes it, or what it takes.
        if let Some(ghost) = app.ghost() {
            let column = (cursor.1 as u16).min(width);
            frame.buffer_mut().set_line(
                x + column,
                rect.y + (cursor.0 as u16).min(height - 1),
                &Line::from(Span::styled(ghost, theme.dim())),
                width - column,
            );
        }
    }
    if app.overlay.is_none() && app.list_focus.is_none() {
        let column = (cursor.1 as u16).min(width.saturating_sub(1));
        frame.set_cursor_position(Position {
            x: x + column,
            y: rect.y + (cursor.0 as u16).min(height - 1),
        });
    }
}

/// The agent list's state column, as wide as its widest word (`finished`).
const STATE_WIDTH: usize = 8;
/// Its time column, as wide as `9h 59m`.
const TIME_WIDTH: usize = 6;
/// The figures of its tool call column, up to 999.
const CALLS_WIDTH: usize = 3;

/// A worker's time in the agent list, such as `45s`, `12m` or `1h 05m`. The
/// minutes have two digits beside the hours, so they line up with those of
/// the rows that show minutes only.
fn list_time(ms: i64) -> String {
    let seconds = (ms / 1000).max(0);
    if seconds < 3600 {
        return transcript::duration(ms);
    }
    format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
}

/// The agent list, below the input: `● main` and a row for each worker
/// running or waiting and each that ended in the last five minutes, the one
/// shown marked, up to five workers, with a count of the workers above them
/// and one of those below when there are. ↓ from the box moves into it,
/// Enter shows a worker's own transcript, and the wheel scrolls it.
fn draw_list(frame: &mut Frame, app: &mut App, area: Rect, rows: Vec<ListRow>) {
    let count = app.list().len();
    let focus = app.list_focus.map(|row| row.min(count - 1));
    app.list_drawn = (area, rows.clone());
    let theme = &app.theme;
    let list = app.list();
    let now = chrono::Utc::now().timestamp_millis();
    let id_width = list
        .iter()
        .map(|row| row.map_or(4, |w| w.id.len()))
        .max()
        .unwrap_or(4);
    for (line, row) in rows.iter().enumerate() {
        let y = area.y + line as u16;
        let counted = match *row {
            ListRow::Agent(_) => None,
            ListRow::Above(count) => Some(format!("{count} above")),
            ListRow::More(count) => Some(format!("{count} more")),
            ListRow::Both(above, below) => Some(format!("{above} above, {below} more")),
        };
        if let Some(counted) = counted {
            let text = format!("   {} {counted}", glyphs::render("…"));
            frame.buffer_mut().set_line(
                area.x,
                y,
                &Line::from(Span::styled(text, theme.dim())),
                area.width,
            );
            continue;
        }
        let ListRow::Agent(index) = *row else {
            continue;
        };
        let row = list[index];
        let shown = row.map(|w| w.id.as_str()) == app.watching.as_deref();
        // A worker that ended has a filled bullet in its state's colour (the
        // theme's success colour once it finished). The one shown has its
        // name in bold.
        let ended = row.filter(|w| !w.state.is_active());
        let bullet = if shown || ended.is_some() {
            glyphs::render("●").into_owned()
        } else {
            glyphs::render("○").into_owned()
        };
        let bullet_style = match ended {
            Some(worker) => transcript::state(worker, theme).1,
            None if shown => theme.accent(),
            None => theme.dim(),
        };
        let name_style = if shown {
            theme.text().add_modifier(Modifier::BOLD)
        } else {
            theme.text()
        };
        let mut left = vec![
            Span::raw(" "),
            Span::styled(format!("{bullet} "), bullet_style),
        ];
        let mut right: Vec<Span<'static>> = Vec::new();
        match row {
            None => left.push(Span::styled(format!("{:<id_width$}", "main"), name_style)),
            Some(worker) => {
                left.push(Span::styled(
                    format!("{:<id_width$}", worker.id),
                    name_style,
                ));
                let doing = if worker.state == crate::remote::WorkerState::Running {
                    worker.last.clone().unwrap_or_else(|| worker.title.clone())
                } else {
                    worker.title.clone()
                };
                left.push(Span::styled(format!("  {doing}"), theme.dim()));
                // In columns: the state, the time and the tool calls line
                // up from row to row.
                let (state, style) = transcript::state(worker, theme);
                right.push(Span::styled(format!("{state:<STATE_WIDTH$}"), style));
                let time = transcript::run_time(worker, now).map_or_else(
                    || " ".repeat(TIME_WIDTH + 3),
                    |ms| format!("{} {:>TIME_WIDTH$}", glyphs::render(" ·"), list_time(ms)),
                );
                right.push(Span::styled(time, theme.dim()));
                right.push(Span::styled(
                    format!(
                        "{} {:>CALLS_WIDTH$} tool call{} ",
                        glyphs::render(" ·"),
                        worker.tool_calls,
                        if worker.tool_calls == 1 { " " } else { "s" }
                    ),
                    theme.dim(),
                ));
            }
        }
        let right = Line::from(right);
        let rw = right.width() as u16;
        let row_rect = Rect::new(area.x, y, area.width, 1);
        let room = area.width.saturating_sub(rw + 1) as usize;
        let cut: Vec<Span> = wrap::wrap(&left, room.max(1), 0)
            .into_iter()
            .next()
            .unwrap_or_default();
        let mut left = Line::from(cut);
        let mut right = right;
        if focus == Some(index) {
            frame.buffer_mut().set_style(row_rect, theme.chosen());
            for span in left.spans.iter_mut().chain(right.spans.iter_mut()) {
                span.style = theme.chosen();
            }
        }
        frame.buffer_mut().set_line(area.x, y, &left, area.width);
        if rw < area.width {
            frame
                .buffer_mut()
                .set_line(area.right() - rw, y, &right, rw);
        }
    }
}

/// The status line: is it working (or how long the last turn ran), how
/// close is a handover, what does it cost; what is unusual (outside access, a worker watched, a view other
/// than the full one); and the key that matters now.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let dot = glyphs::render("●").into_owned();
    let mut left: Vec<Span<'static>> = vec![Span::raw(" ")];
    let sep = || Span::styled(glyphs::render(" · ").into_owned(), theme.dim());
    if let Some(worker) = &app.watching {
        left.push(Span::styled(
            format!("viewing {worker}"),
            theme.accent().add_modifier(Modifier::BOLD),
        ));
        left.push(sep());
    }
    if app.activity.question.is_some() {
        left.push(Span::styled(format!("{dot} asks you"), theme.accent()));
    } else if app.activity.working {
        let time = app
            .working_since
            .map(|since| clock(since.elapsed().as_secs()))
            .unwrap_or_default();
        let word = if app.stopping { "stopping" } else { "working" };
        left.push(Span::styled(format!("{dot} {word}"), theme.accent()));
        left.push(Span::styled(format!(" {time}"), theme.dim()));
    } else {
        left.push(Span::styled(
            format!("{} idle", glyphs::render("○")),
            theme.dim(),
        ));
        // In place of a line after each turn in the transcript.
        if let Some(turn) = app.last_turn {
            left.push(sep());
            left.push(Span::styled(
                format!("last turn {}", clock(turn.as_secs())),
                theme.dim(),
            ));
        }
    }
    let reading = &app.reading;
    // The risky setting, only while it is on, first.
    if app.outside() {
        left.push(sep());
        left.push(Span::styled("outside access".to_string(), theme.warning()));
    }
    if let Some(tokens) = reading.context_tokens {
        let handover = app.handover_at();
        let near = handover > 0 && tokens * 10 >= handover * 9;
        left.push(sep());
        let text = if handover > 0 {
            format!("{}k/{}k", tokens / 1000, handover / 1000)
        } else {
            format!("{}k", tokens / 1000)
        };
        left.push(Span::styled(
            text,
            if near { theme.warning() } else { theme.dim() },
        ));
    }
    // On a plan, its usage, not dollars; what was billed, if anything;
    // and what the session would cost at API list prices, where a plan
    // paid for some of it.
    if let Some(plan) = &reading.plan_usage {
        // ("Claude plan 5 h 21%, 7 d 4%": the windows, as the page says
        // them.)
        let windows = plan
            .split_once(" plan ")
            .map_or(plan.as_str(), |(_, windows)| windows);
        left.push(sep());
        left.push(Span::styled(windows.to_string(), theme.dim()));
    }
    if reading.cost >= 0.005 {
        left.push(sep());
        left.push(Span::styled(format!("${:.2}", reading.cost), theme.dim()));
    }
    if let Some(api) = reading.api_price.filter(|api| *api >= 0.005) {
        left.push(sep());
        left.push(Span::styled(
            format!("{} ${api:.2}", glyphs::render("≈")),
            theme.dim(),
        ));
    }
    let running = app
        .transcript
        .workers()
        .iter()
        .filter(|w| w.state.is_active())
        .count();
    if running > 0 {
        left.push(sep());
        left.push(Span::styled(
            format!("{running} worker{}", if running == 1 { "" } else { "s" }),
            theme.dim(),
        ));
    }
    if app.view != View::Full && app.watching.is_none() {
        left.push(sep());
        left.push(Span::styled(app.view.name().to_string(), theme.dim()));
    }

    // The key that matters now, or a word for a moment.
    let right: Vec<Span<'static>> = if let Some(hint) = app.hint() {
        vec![
            Span::styled(hint.to_string(), theme.accent()),
            Span::raw(" "),
        ]
    } else {
        let pairs: &[(&str, &str)] = if app.list_focus.is_some() {
            &[("Enter", "view"), ("Esc", "back to the box")]
        } else if app.watching.is_some() && app.command_running() {
            &[("Ctrl+B", "background"), ("Esc", "back")]
        } else if app.watching.is_some() {
            &[("Esc", "back")]
        } else if app.activity.working && app.activity.question.is_none() && app.command_running() {
            &[("Ctrl+B", "background"), ("Ctrl+C", "stop")]
        } else if app.activity.working && app.activity.question.is_none() {
            &[("Ctrl+C", "stop")]
        } else {
            &[]
        };
        let mut spans = hints(theme, pairs);
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans
    };
    let left = Line::from(left);
    let right = Line::from(right);
    frame
        .buffer_mut()
        .set_line(area.x, area.y, &left, area.width);
    let (lw, rw) = (left.width() as u16, right.width() as u16);
    if lw + rw + 2 <= area.width {
        frame
            .buffer_mut()
            .set_line(area.right() - rw, area.y, &right, rw);
    }
}

/// `seconds` as a clock: `14s`, `2m14s`, `1h02m`.
fn clock(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// The agent's question, docked above the status line: the input box is
/// the answer box while it is open. Returns the rows it takes.
fn draw_question(frame: &mut Frame, app: &mut App, above: Rect, previous: Option<Docked>) -> u16 {
    let theme = app.theme.clone();
    let theme = &theme;
    let Some(question) = app.question_shown() else {
        return 0;
    };
    let id = question.id;
    let inner_width = (above.width as usize).saturating_sub(4).max(4);
    let mut lines = markdown::render(&question.text, theme.text(), theme, inner_width);
    let room = (above.height as usize / 2).saturating_sub(2).max(1);
    if lines.len() > room {
        lines.truncate(room.saturating_sub(1));
        // (Not the question's text: a copy leaves it out.)
        let mut more = wrap::Wrapped::line(vec![Span::styled(
            glyphs::render("… (the whole question is in the transcript)").into_owned(),
            theme.dim(),
        )]);
        more.bare = true;
        lines.push(more);
    }
    let height = (lines.len() as u16 + 2).min(above.height);
    let rect = Rect::new(above.x, above.bottom() - height, above.width, height);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.borders())
        .border_style(theme.accent())
        .style(theme.base())
        .title(Line::from(Span::styled(
            " The agent asks ",
            theme.accent().add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    lines.truncate(inner.height as usize);
    for (row, line) in lines.iter().enumerate() {
        frame.buffer_mut().set_line(
            inner.x + 1,
            inner.y + row as u16,
            &Line::from(line.spans.clone()),
            inner.width.saturating_sub(1),
        );
    }
    // Its text is selected as the transcript's is: what was highlighted
    // stays while the same question shows the same way.
    let area = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(1),
        lines.len() as u16,
    );
    let selection = previous
        .filter(|docked| docked.id == id && docked.area == area && docked.lines == lines)
        .and_then(|docked| docked.selection);
    let docked = Docked {
        id,
        area,
        lines,
        selection,
    };
    docked.draw_selection(frame.buffer_mut(), theme);
    app.docked = Some(docked);
    let keys = Line::from({
        let mut spans = hints(theme, &[("Enter", "answer"), ("Esc", "later")]);
        spans.push(Span::raw(" "));
        spans
    });
    let width = keys.width() as u16;
    if width + 4 < rect.width {
        frame
            .buffer_mut()
            .set_line(rect.right() - width - 2, rect.bottom() - 1, &keys, width);
    }
    height
}

/// The commands matching what is typed, above the status line. Returns the
/// rows it takes.
fn draw_menu(frame: &mut Frame, app: &App, above: Rect) -> u16 {
    let theme = &app.theme;
    let items = app.menu_items();
    let rows = items.len().min(MENU_LINES).min(above.height as usize);
    if rows == 0 {
        return 0;
    }
    let selected = app.menu.selected.min(items.len() - 1);
    let first = selected.saturating_sub(rows - 1);
    let rect = Rect::new(
        above.x,
        above.bottom() - rows as u16,
        above.width,
        rows as u16,
    );
    let surface = if theme.is_terminal() {
        theme.base()
    } else {
        Style::new().bg(theme.raised).fg(theme.text)
    };
    frame.buffer_mut().set_style(rect, surface);
    let name_width = items.iter().map(|c| c.name.len()).max().unwrap_or(8);
    for (row, command) in items.iter().enumerate().skip(first).take(rows) {
        let y = rect.y + (row - first) as u16;
        let chosen = row == selected;
        let mut spans = vec![
            Span::raw("   "),
            Span::styled(
                format!("{:<name_width$}", command.name),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::raw(command.about.to_string()),
        ];
        // What it takes, on the row chosen.
        if chosen && !command.args.is_empty() {
            spans.push(Span::raw(format!("  {}", command.args)));
        }
        if chosen {
            let line_rect = Rect::new(rect.x, y, rect.width, 1);
            frame.buffer_mut().set_style(line_rect, theme.chosen());
            for span in &mut spans {
                span.style = theme.chosen();
            }
        }
        let line = Line::from(spans);
        let cut: Vec<Span> = wrap::wrap(&line.spans, rect.width as usize, 0)
            .into_iter()
            .next()
            .unwrap_or_default();
        frame
            .buffer_mut()
            .set_line(rect.x, y, &Line::from(cut), rect.width);
    }
    rows as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{Activity, Item, Kind, Question, Update};
    use crate::tui::app::App;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        use crate::tui::app::Info;
        use crate::tui::input::{Editor, History};
        use crate::tui::theme::{Catalog, Depth};
        App::new(
            crate::remote::Snapshot::default(),
            Editor::new(History::load(None)),
            Info {
                workspace: "work".into(),
                handover_at: 250_000,
                ..Info::default()
            },
            Catalog::load(None),
            Depth::Rgb,
        )
    }

    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The API keys menu over an 80-column screen, and the settings' row
    /// that opens it.
    #[test]
    fn the_keys_menu_fits_an_80_column_screen() {
        use crate::tui::overlay::{Overlay, SettingsRow};
        let mut app = app();
        app.key_states = crate::tui::keys_menu::tests::states;
        app.open_keys("", None);
        let rows = screen(&mut app, 80, 24);
        println!("{}", rows.join("\n"));
        assert!(
            rows.iter()
                .any(|row| row.contains("│ ▸ Anthropic      sk-ant-…a1b2    saved by Arko")),
            "{rows:#?}"
        );
        app.open_keys("add voyage", None);
        let rows = screen(&mut app, 80, 24);
        println!("{}", rows.join("\n"));
        assert!(
            rows.iter()
                .any(|row| row.contains("Paste a Voyage API key")),
            "{rows:#?}"
        );
        let row = app
            .settings_rows()
            .iter()
            .position(|row| *row == SettingsRow::Keys)
            .unwrap();
        app.overlay = Some(Overlay::Settings { row, picker: None });
        let rows = screen(&mut app, 80, 30);
        println!("{}", rows.join("\n"));
        assert!(
            rows.iter()
                .any(|row| row.contains("▸ API keys       3 of 5 set ›")),
            "{rows:#?}"
        );
        // A 24-row terminal scrolls the settings to show the row whole.
        let rows = screen(&mut app, 80, 24);
        println!("{}", rows.join("\n"));
        assert!(
            rows.iter()
                .any(|row| row.contains("search indexes. Enter opens them.")),
            "{rows:#?}"
        );
    }

    /// The input box moves a word that crosses its edge whole to the next
    /// line, and the cursor is drawn after it.
    #[test]
    fn the_input_box_wraps_whole_words_with_the_cursor_after_them() {
        let mut app = app();
        for c in "Please explain the Methodology".chars() {
            app.editor.insert(c);
        }
        let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row = |y: u16| {
            (0..30)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert_eq!(row(9), " › Please explain the");
        assert_eq!(row(10), "   Methodology");
        let cursor = terminal.get_cursor_position().unwrap();
        assert_eq!((cursor.x, cursor.y), (14, 10));
    }

    /// A list taller than its area counts what it hides above and below
    /// the workers it shows, as the focus moves and as workers come and go.
    #[test]
    fn the_agent_list_counts_the_workers_above_and_below_it() {
        use crate::remote::WorkerState::{Finished, Running};
        use crate::tui::app::tests::{ended_earlier, worker};
        let mut app = app();
        for n in 1..=6 {
            let summary = worker(&format!("r{n}"), Running);
            app.update(Update::Worker(summary), Activity::default);
        }
        let list = |app: &mut App| {
            let rows = screen(app, 80, 40);
            rows[rows.len() - 7..].to_vec()
        };
        let name = |row: &String| {
            row.split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string()
        };
        // One more worker than fits: the last is below.
        let shown = list(&mut app);
        assert_eq!(shown[6], "   … 1 more", "{shown:#?}");
        assert!(!shown.iter().any(|row| row.contains("above")));
        // At the bottom, it is shown, and the first is above.
        app.list_focus = Some(6);
        let shown = list(&mut app);
        assert_eq!(shown[1], "   … 1 above", "{shown:#?}");
        assert_eq!(
            shown.iter().map(name).collect::<Vec<_>>(),
            ["main", "1", "r2", "r3", "r4", "r5", "r6"]
        );
        assert!(!shown.iter().any(|row| row.contains("more")));
        // A worker leaves the list: the others fit, and nothing is counted.
        app.update(
            Update::Worker(ended_earlier("r1", Finished)),
            Activity::default,
        );
        let rows = screen(&mut app, 80, 40);
        assert!(
            !rows
                .iter()
                .any(|row| row.ends_with(" more") || row.ends_with(" above")),
            "{rows:#?}"
        );
    }

    /// The agent list under the input: the main agent, five of the workers
    /// running and how many more. A worker that ended more than five minutes
    /// ago isn't in it.
    #[test]
    fn the_agent_list_shows_five_workers_running_and_counts_the_rest() {
        use crate::remote::WorkerState::{Finished, Running};
        use crate::tui::app::tests::ended_earlier;
        let mut app = app();
        let states = [
            Running, Finished, Running, Running, Running, Running, Running, Running,
        ];
        for (n, state) in states.into_iter().enumerate() {
            let summary = ended_earlier(&format!("r{}", n + 1), state);
            app.update(Update::Worker(summary), Activity::default);
        }
        let rows = screen(&mut app, 80, 40);
        println!("{}", rows.join("\n"));
        let list = &rows[rows.len() - 7..];
        let names: Vec<&str> = list
            .iter()
            .map(|row| row.split_whitespace().nth(1).unwrap_or_default())
            .collect();
        assert_eq!(
            names,
            ["main", "r1", "r3", "r4", "r5", "r6", "2"],
            "{list:#?}"
        );
        assert_eq!(list[6], "   … 2 more");
        assert!(!rows.iter().any(|row| row.contains("r2")), "{rows:#?}");
    }

    /// A worker that ended stays in the agent list with a filled bullet in
    /// its state's colour, the theme's success colour once it finished. The
    /// one shown keeps its name in bold.
    #[test]
    fn ended_workers_have_a_filled_bullet_in_their_state_s_colour() {
        use crate::remote::WorkerState::{Failed, Finished, Running, Stopped};
        use crate::tui::app::tests::worker;
        let mut app = app();
        for (id, state) in [
            ("r1", Running),
            ("r2", Finished),
            ("r3", Failed),
            ("r4", Stopped),
        ] {
            app.update(Update::Worker(worker(id, state)), Activity::default);
        }
        let theme = app.theme.clone();
        assert_ne!(theme.success().fg, theme.accent().fg);
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        let mut look = |app: &mut App| {
            terminal.draw(|frame| draw(frame, app)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            // Each agent's bullet, its colour, and whether its name is bold.
            (35..40)
                .map(|y| {
                    let name: String = (3..7).map(|x| buffer[(x, y)].symbol()).collect();
                    let bullet = &buffer[(1, y)];
                    let bold = buffer[(3, y)].modifier.contains(Modifier::BOLD);
                    (
                        name.trim().to_string(),
                        bullet.symbol().to_string(),
                        bullet.fg,
                        bold,
                    )
                })
                .collect::<Vec<_>>()
        };
        let row = |name: &str, bullet: &str, style: Style, bold| {
            (
                name.to_string(),
                bullet.to_string(),
                style.fg.unwrap(),
                bold,
            )
        };
        assert_eq!(
            look(&mut app),
            [
                row("main", "●", theme.accent(), true),
                row("r1", "○", theme.dim(), false),
                row("r2", "●", theme.success(), false),
                row("r3", "●", theme.error(), false),
                row("r4", "●", theme.warning(), false),
            ]
        );
        app.watch(Some("r2".into()));
        assert_eq!(
            look(&mut app)[..3],
            [
                row("main", "○", theme.dim(), false),
                row("r1", "○", theme.dim(), false),
                row("r2", "●", theme.success(), true),
            ]
        );
    }

    /// The agent list's state, time and tool calls are columns that line
    /// up, for times up to 9h 59m and up to 999 tool calls.
    #[test]
    fn the_agent_list_s_figures_line_up() {
        use crate::remote::WorkerState::{Queued, Running};
        use crate::tui::app::tests::worker;
        let mut app = app();
        let workers = [
            (Running, 720_000, 18),
            (Running, 3_900_000, 104),
            (Running, 45_000, 1),
            (Queued, 0, 0),
            (Running, 35_940_000, 999),
        ];
        for (n, (state, ran_ms, tool_calls)) in workers.into_iter().enumerate() {
            let mut summary = worker(&format!("r{}", n + 1), state);
            summary.ran_ms = ran_ms;
            summary.tool_calls = tool_calls;
            app.update(Update::Worker(summary), Activity::default);
        }
        let rows = screen(&mut app, 80, 40);
        let list = &rows[rows.len() - 5..];
        // The figures from the state's column on, which is as far left of
        // the tool calls in every row.
        let column = |row: &String| {
            let calls = row[..row.find(" tool call").unwrap()].chars().count();
            row.chars().skip(calls - 23).collect::<String>()
        };
        assert_eq!(
            list.iter().map(column).collect::<Vec<_>>(),
            [
                "running  ·    12m ·  18 tool calls".to_string(),
                "running  · 1h 05m · 104 tool calls".to_string(),
                "running  ·    45s ·   1 tool call".to_string(),
                format!("queued{}·   0 tool calls", " ".repeat(12)),
                "running  · 9h 59m · 999 tool calls".to_string(),
            ],
            "{list:#?}"
        );
        let calls: Vec<usize> = list
            .iter()
            .map(|row| row[..row.find(" tool call").unwrap()].chars().count())
            .collect();
        assert!(calls.iter().all(|at| *at == calls[0]), "{list:#?}");
    }

    /// The quit popup over an 80-column screen: the background jobs, each
    /// cut to fit with how long it has run, the workers that stop too, and
    /// the choices. The settings' row that turns it off.
    #[test]
    fn the_quit_popup_lists_the_jobs_and_the_workers() {
        use crate::tui::app::tests::job;
        use crate::tui::overlay::{Overlay, SettingsRow};
        use std::time::Duration;
        let mut app = app();
        let long = format!("cargo build --release {}", "x".repeat(80));
        app.reading.workers_running = vec!["r1".into()];
        app.ask_quit(vec![
            job("sleep 300", Duration::from_secs(150)),
            job(&format!("{long}\necho done"), Duration::from_secs(5)),
        ]);
        let rows = screen(&mut app, 80, 24);
        println!("{}", rows.join("\n"));
        let row = |text: &str| {
            rows.iter()
                .find(|row| row.contains(text))
                .unwrap_or_else(|| panic!("no {text:?} in {rows:#?}"))
        };
        row("Quit Arko");
        row("2 background jobs are running. They stop when Arko");
        assert!(row("sleep 300").ends_with("2m │"), "{rows:#?}");
        let cut = row("cargo build --release xx");
        assert!(cut.ends_with("5s │") && !cut.contains(&long), "{rows:#?}");
        row("1 worker is running too. It stops as well, and resuming");
        row("Quit now?");
        assert!(row(" Yes ").contains("[ ] Don't ask again"), "{rows:#?}");
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let rows = screen(&mut app, 80, 24);
        assert!(
            rows.iter().any(|row| row.contains("[x] Don't ask again")),
            "{rows:#?}"
        );

        let row = app
            .settings_rows()
            .iter()
            .position(|row| *row == SettingsRow::ConfirmQuit)
            .unwrap();
        app.overlay = Some(Overlay::Settings { row, picker: None });
        let rows = screen(&mut app, 80, 30);
        println!("{}", rows.join("\n"));
        assert!(
            rows.iter()
                .any(|row| row.contains("▸ Ask first      on   off")),
            "{rows:#?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("With background jobs running, quitting asks first")),
            "{rows:#?}"
        );
    }

    #[test]
    fn the_empty_screen_has_the_status_line_and_a_message_box() {
        let mut app = app();
        let rows = screen(&mut app, 60, 8);
        assert_eq!(rows[4], " ○ idle");
        assert_eq!(rows[5], "─".repeat(60));
        assert_eq!(rows[6], " › Message  / for commands");
        assert_eq!(rows[7], "─".repeat(60));
    }

    /// In a worker's own view, the box says where a message goes.
    #[test]
    fn a_workers_view_says_whom_the_box_messages() {
        let mut app = app();
        app.watch(Some("r1".into()));
        assert_eq!(screen(&mut app, 60, 8)[6], " › Message r1  / for commands");
    }

    #[test]
    fn the_status_line_says_what_runs_and_what_it_costs() {
        let mut app = app();
        app.set_activity(Activity {
            working: true,
            since: None,
            question: None,
        });
        app.reading.context_tokens = Some(142_000);
        app.reading.cost = 3.2;
        app.view = View::Quiet;
        let rows = screen(&mut app, 80, 6);
        assert!(
            rows[2].starts_with(" ● working 0s · 142k/250k · $3.20 · quiet"),
            "{}",
            rows[2]
        );
        assert!(rows[2].ends_with("Ctrl+C  stop"), "{}", rows[2]);
        assert_eq!(rows[4], " › Add to the task");
        // Once the turn ends, the line says how long it ran.
        app.set_activity(Activity {
            working: false,
            since: None,
            question: None,
        });
        let rows = screen(&mut app, 80, 6);
        assert!(
            rows[2].starts_with(" ○ idle · last turn 0s · 142k/250k · $3.20 · quiet"),
            "{}",
            rows[2]
        );
    }

    /// While the agent shown runs a shell command, the status line offers
    /// Ctrl+B, the main agent's or a watched worker's.
    #[test]
    fn the_status_line_offers_ctrl_b_while_a_command_runs() {
        let mut app = app();
        app.set_activity(Activity {
            working: true,
            since: None,
            question: None,
        });
        app.reading.commands = vec!["main".into()];
        let rows = screen(&mut app, 80, 6);
        assert!(
            rows[2].ends_with("Ctrl+B  background   Ctrl+C  stop"),
            "{}",
            rows[2]
        );
        app.watching = Some("r1".into());
        let rows = screen(&mut app, 80, 6);
        assert!(rows[2].ends_with("Esc  back"), "{}", rows[2]);
        assert!(!rows[2].contains("Ctrl+B"), "{}", rows[2]);
        app.reading.commands = vec!["r1".into()];
        let rows = screen(&mut app, 80, 6);
        assert!(
            rows[2].ends_with("Ctrl+B  background   Esc  back"),
            "{}",
            rows[2]
        );
    }

    #[test]
    fn the_question_docks_above_the_status_line() {
        let mut app = app();
        app.set_activity(Activity {
            working: true,
            since: None,
            question: Some(Question {
                id: 1,
                text: "Tabs or spaces?".into(),
            }),
        });
        let rows = screen(&mut app, 50, 10);
        assert!(rows[3].contains("The agent asks"), "{rows:?}");
        assert!(rows[4].contains("Tabs or spaces?"));
        assert!(rows[5].contains("Enter  answer"));
        assert_eq!(rows[6], " ● asks you");
        assert_eq!(rows[8], " › Answer the agent's question");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let rows = screen(&mut app, 50, 10);
        assert!(!rows.iter().any(|row| row.contains("Tabs or spaces?")));
        assert_eq!(rows[6], " ● asks you");
    }

    /// With the symbols of the classic Windows console's fonts (`basic`) and
    /// with plain ASCII (`ascii`), the whole screen is drawn in characters
    /// those fonts have: the banner, every kind of item, the separators
    /// between their parts, the status line, the agent list, the docked
    /// question, a worker's own view and the overlays.
    #[test]
    fn every_symbol_on_the_screen_has_its_stand_in() {
        use crate::glyphs::Symbols;
        use crate::glyphs::tests::{choose_symbols, wgl4};
        use crate::remote::{CallStatus, Delivery, Level, Source, WorkerState};
        use crate::tui::app::tests::{job, worker};
        use crate::tui::overlay::Overlay;
        use std::time::Duration;
        for symbols in [Symbols::Basic, Symbols::Ascii] {
            let _chosen = choose_symbols(symbols);
            let mut app = app();
            app.key_states = crate::tui::keys_menu::tests::states;
            app.transcript.banner = Some(transcript::Banner {
                model: "gpt-6-luna (xhigh effort)".into(),
                workers: "parallel workers".into(),
                workspace: "/home/someone/project".into(),
                session: "20261021-101010-abcd".into(),
                project: Some("AGENTS.md".into()),
                tools: vec!["1 skill".into(), "2 MCP servers".into()],
                model_note: None,
                notes: vec![("Outside access is on.".into(), Some(Level::Warn))],
            });
            let mut items = Vec::new();
            let mut add = |item: Item| items.push(item);
            let mut user = Item::new(Kind::User, "Fix the parser");
            user.delivery = Some(Delivery::Turn);
            user.source = Some(Source::Web);
            add(user);
            add(Item::new(Kind::Thinking, "**Plan**\nRead it first."));
            add(Item::new(Kind::Text, "Reading the parser."));
            let mut call = Item::new(Kind::Call, "$ cargo test");
            call.status = Some(CallStatus::Ok);
            call.outcome = Some("12 passed".into());
            add(call);
            let mut result = Item::new(Kind::Result, "exit code 0").detail("running 12 tests");
            result.call = Some(4);
            add(result);
            let mut failed = Item::new(Kind::Call, "read a.rs");
            failed.status = Some(CallStatus::Failed);
            failed.outcome = Some("not found".into());
            add(failed);
            let mut running = Item::new(Kind::Call, "$ sleep 30");
            running.status = Some(CallStatus::Running);
            add(running);
            add(Item::new(Kind::Notice, "retrying in 2 s").level(Level::Warn));
            add(Item::new(Kind::Notice, "handed over").level(Level::Info));
            add(Item::new(Kind::Output, "make: done"));
            add(Item::new(Kind::Message, "Halfway there."));
            let mut line = Item::new(Kind::WorkerLine, "Test it");
            line.worker = Some("r1".into());
            add(line);
            let mut call = Item::new(Kind::Call, "cargo test").agent("r1");
            call.status = Some(CallStatus::Ok);
            add(call);
            add(Item::new(Kind::Thinking, "Which test?").agent("r1"));
            add(Item::new(Kind::Result, "done").agent("r1"));
            let mut conversation = Item::new(Kind::Conversation, "Conversation 2");
            conversation.detail = Some("plan 5 h 21%".into());
            add(conversation);
            add(Item::new(Kind::Question, "Tabs or spaces?"));
            add(Item::new(Kind::Reply, "Done."));
            let mut note = Item::new(Kind::User, "Keep the old name");
            note.delivery = Some(Delivery::Note);
            note.source = Some(Source::Web);
            note.pending = true;
            add(note);
            let now = chrono::Utc::now().timestamp_millis();
            for (id, mut item) in (1..).zip(items) {
                item.id = id;
                item.at = now;
                item.turn = Some(1);
                app.update(Update::Item(item), Activity::default);
            }
            let mut summary = worker("r1", WorkerState::Running);
            summary.last = Some("cargo test".into());
            app.update(Update::Worker(summary), Activity::default);
            app.reading.context_tokens = Some(142_000);
            app.reading.cost = 3.2;
            app.set_activity(Activity {
                working: true,
                since: None,
                question: Some(crate::remote::Question {
                    id: 1,
                    text: "Tabs or spaces for the new files?".into(),
                }),
            });

            let mut wrong = Vec::new();
            let mut look = |app: &mut App, state: &str| {
                for row in screen(app, 100, 48) {
                    let shown = |c: char| match symbols {
                        Symbols::Ascii => c.is_ascii(),
                        _ => wgl4(c),
                    };
                    if !row.chars().all(shown) {
                        wrong.push(format!("{symbols:?}, {state}: {row}"));
                    }
                }
            };
            for view in View::ALL {
                app.view = view;
                look(&mut app, view.name());
            }
            app.view = View::Full;
            app.watch(Some("r1".into()));
            look(&mut app, "the worker's view");
            app.watch(None);
            app.overlay = Some(Overlay::Help { scroll: 0 });
            look(&mut app, "help");
            app.overlay = Some(Overlay::Settings {
                row: 0,
                picker: None,
            });
            look(&mut app, "settings");
            app.overlay = None;
            app.open_keys("", None);
            look(&mut app, "keys");
            app.overlay = None;
            app.ask_quit(vec![job("sleep 300", Duration::from_secs(150))]);
            look(&mut app, "quit");
            app.overlay = None;
            app.model_needs = |reachable, name| {
                (!name.starts_with("cc:"))
                    .then(|| crate::tui::model_picker::needs(reachable, name))
                    .flatten()
            };
            app.set_reading(crate::remote::Reading {
                models: crate::remote::Models {
                    main: "gpt-6-luna".into(),
                    main_effort: "high".into(),
                    workers: "gpt-6-luna".into(),
                    workers_effort: "high".into(),
                    workers_follow: true,
                    reachable: vec!["openai".into()],
                    has_workers: true,
                    ..Default::default()
                },
                ..Default::default()
            });
            app.open_model(crate::model_command::Target::Main);
            look(&mut app, "the model picker");
            app.overlay = None;
            app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
            look(&mut app, "the command menu");
            assert!(wrong.is_empty(), "{wrong:#?}");
        }
    }

    /// "Working…" shows while the agent works and waits on its model, not
    /// while it asks the user, and the screen is drawn again for its frames
    /// only while it shows.
    #[test]
    fn working_shows_while_the_agent_waits_on_its_model() {
        let mut app = app();
        let working = |rows: &[String]| rows.iter().any(|row| row.ends_with(" Working…"));
        assert!(!working(&screen(&mut app, 50, 10)));
        assert!(!app.transcript.drawn.moving);
        let mut activity = Activity {
            working: true,
            since: Some(0),
            question: None,
        };
        app.set_activity(activity.clone());
        let rows = screen(&mut app, 50, 10);
        assert!(working(&rows), "{rows:?}");
        assert!(app.transcript.drawn.moving);
        activity.question = Some(Question {
            id: 1,
            text: "Tabs or spaces?".into(),
        });
        app.set_activity(activity);
        let rows = screen(&mut app, 50, 10);
        assert!(!working(&rows), "{rows:?}");
        assert!(!app.transcript.drawn.moving);
    }

    #[test]
    fn the_menu_lists_commands_above_the_input() {
        let mut app = app();
        for c in "/wa".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let rows = screen(&mut app, 100, 8);
        assert!(
            rows[3].starts_with("   /watch  follow one worker"),
            "{rows:?}"
        );
        assert!(rows[3].ends_with("WORKER|main"), "{rows:?}");
        assert_eq!(rows[6], " › /wa");
        // An argument's completion follows the cursor.
        for c in "tch m".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let rows = screen(&mut app, 100, 8);
        assert_eq!(rows[6], " › /watch main");
    }

    fn mouse(app: &mut App, kind: crossterm::event::MouseEventKind, x: u16, y: u16) {
        app.mouse(crossterm::event::MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
    }

    /// Letting go of a drag copies what it highlighted, and says how much;
    /// a click copies nothing; a double click copies a word.
    #[test]
    fn a_drag_copies_and_a_click_does_not() {
        use crate::tui::app::Effect;
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut app = app();
        let mut item = Item::new(Kind::Reply, "Hello there, reader.");
        item.id = 1;
        app.update(Update::Item(item), Activity::default);
        screen(&mut app, 40, 8);
        let left = MouseButton::Left;
        mouse(&mut app, MouseEventKind::Down(left), 3, 0);
        mouse(&mut app, MouseEventKind::Up(left), 3, 0);
        assert!(app.take_effects().is_empty());
        mouse(&mut app, MouseEventKind::Down(left), 8, 0);
        mouse(&mut app, MouseEventKind::Drag(left), 15, 0);
        screen(&mut app, 40, 8);
        mouse(&mut app, MouseEventKind::Drag(left), 21, 0);
        mouse(&mut app, MouseEventKind::Up(left), 21, 0);
        let copy = |text: &str, what: &str| Effect::Copy {
            text: text.into(),
            what: what.into(),
        };
        assert_eq!(
            app.take_effects(),
            [copy("there, reader.", "14 characters")]
        );
        // The highlight stays until a key is pressed.
        assert!(app.transcript.selection.is_some());
        app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(app.transcript.selection.is_none());
        // (Frames are drawn between the clicks, as in the terminal.)
        for kind in [
            MouseEventKind::Down(left),
            MouseEventKind::Up(left),
            MouseEventKind::Down(left),
            MouseEventKind::Up(left),
        ] {
            mouse(&mut app, kind, 9, 0);
            screen(&mut app, 40, 8);
        }
        assert_eq!(app.take_effects(), [copy("there,", "6 characters")]);
        // A triple click (on another cell, so the count starts over): the
        // second click copies the word, the third the line.
        for _ in 0..3 {
            mouse(&mut app, MouseEventKind::Down(left), 10, 0);
            screen(&mut app, 40, 8);
            mouse(&mut app, MouseEventKind::Up(left), 10, 0);
        }
        assert_eq!(
            app.take_effects(),
            [
                copy("there,", "6 characters"),
                copy("Hello there, reader.", "20 characters")
            ]
        );
    }

    /// The question docked above the status line is text to select, as the
    /// transcript's is: a drag highlights it and copies it when the button
    /// is let go, a double click copies a word, and a triple click the
    /// line, joined again where the box wrapped it. A key clears the
    /// highlight.
    #[test]
    fn the_docked_questions_text_selects_and_copies() {
        use crate::tui::app::Effect;
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut app = app();
        let text = "Tabs or spaces for the **new** files? The old ones use tabs, so I'd keep them.";
        app.set_activity(Activity {
            working: true,
            since: None,
            question: Some(Question {
                id: 1,
                text: text.into(),
            }),
        });
        let rows = screen(&mut app, 50, 20);
        let y = rows.iter().position(|row| row.contains("Tabs or")).unwrap();
        let x = rows[y].chars().position(|c| c == 'T').unwrap() as u16;
        let y = y as u16;
        let left = MouseButton::Left;
        let copy = |text: &str, what: &str| Effect::Copy {
            text: text.into(),
            what: what.into(),
        };
        mouse(&mut app, MouseEventKind::Down(left), x, y);
        mouse(&mut app, MouseEventKind::Drag(left), x + 6, y);
        screen(&mut app, 50, 20);
        mouse(&mut app, MouseEventKind::Drag(left), x + 13, y);
        mouse(&mut app, MouseEventKind::Up(left), x + 13, y);
        assert_eq!(
            app.take_effects(),
            [copy("Tabs or spaces", "14 characters")]
        );
        // It is highlighted, from its first cell to its last.
        let mut terminal = Terminal::new(TestBackend::new(50, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let lit = |dx: u16| buffer[(x + dx, y)].bg == app.theme.selection;
        assert!(lit(0) && lit(13) && !lit(14));
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.docked.as_ref().unwrap().selection, None);
        app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        // A double click: a word.
        for kind in [
            MouseEventKind::Down(left),
            MouseEventKind::Up(left),
            MouseEventKind::Down(left),
            MouseEventKind::Up(left),
        ] {
            mouse(&mut app, kind, x + 9, y);
            screen(&mut app, 50, 20);
        }
        assert_eq!(app.take_effects(), [copy("spaces", "6 characters")]);
        // A triple click (on another cell, so the count starts over): the
        // second click copies the word, the third the line, whole.
        for _ in 0..3 {
            mouse(&mut app, MouseEventKind::Down(left), x + 1, y);
            screen(&mut app, 50, 20);
            mouse(&mut app, MouseEventKind::Up(left), x + 1, y);
        }
        let whole = text.replace("**", "");
        let length = format!("{} characters", whole.chars().count());
        assert_eq!(
            app.take_effects(),
            [copy("Tabs", "4 characters"), copy(&whole, &length)]
        );
        // A click elsewhere clears it.
        mouse(&mut app, MouseEventKind::Down(left), 5, 0);
        mouse(&mut app, MouseEventKind::Up(left), 5, 0);
        assert_eq!(app.docked.as_ref().unwrap().selection, None);
        assert!(app.take_effects().is_empty());
    }

    /// A click on a command's "more lines" opens its whole output, and a
    /// click on the line that ends it closes it: the line clicked stays
    /// under the mouse. A drag, a double click and a triple click select
    /// text as anywhere else, and leave the block as it was.
    #[test]
    fn a_click_opens_and_closes_a_commands_output() {
        use crate::remote::CallStatus;
        use crate::tui::app::Effect;
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut app = app();
        let mut call = Item::new(Kind::Call, "$ seq 10");
        call.status = Some(CallStatus::Ok);
        // (As the feed makes a command's block.)
        let output: Vec<String> = (1..=10).map(|n| format!("line {n}")).collect();
        let mut block = Item::new(Kind::Result, "exit code 0, 0.1 s")
            .detail(format!("{}\n… 6 more lines", output[..4].join("\n")));
        block.whole = Some(output.join("\n").into());
        block.call = Some(2);
        let after = Item::new(Kind::Output, "a\nb\nc\nd\ne\nf");
        for (id, mut item) in [Item::new(Kind::Reply, "Intro."), call, block, after]
            .into_iter()
            .enumerate()
        {
            item.id = id as u64 + 1;
            app.update(Update::Item(item), Activity::default);
        }
        // Eight rows of transcript, scrolled up to the block's lines.
        screen(&mut app, 40, 12);
        app.transcript.scroll(-4);
        let rows = screen(&mut app, 40, 12);
        assert_eq!(rows[0], "  │ line 1");
        assert_eq!(rows[4], "  │ … 6 more lines");
        let left = MouseButton::Left;
        let click = |app: &mut App, x: u16| {
            mouse(app, MouseEventKind::Down(left), x, 4);
            mouse(app, MouseEventKind::Up(left), x, 4);
            screen(app, 40, 12)
        };
        let rows = click(&mut app, 12);
        assert_eq!(
            rows[..6],
            [
                "  │ line 7",
                "  │ line 8",
                "  │ line 9",
                "  │ line 10",
                "  │ ↑ show the first lines only",
                "",
            ]
        );
        // (Elsewhere on the line: the same cell again would be a double
        // click.)
        let rows = click(&mut app, 14);
        assert_eq!(rows[0], "  │ line 1");
        assert_eq!(rows[4], "  │ … 6 more lines");
        assert!(app.take_effects().is_empty());
        let copy = |text: &str, what: &str| Effect::Copy {
            text: text.into(),
            what: what.into(),
        };
        // A drag over it copies.
        mouse(&mut app, MouseEventKind::Down(left), 4, 4);
        mouse(&mut app, MouseEventKind::Drag(left), 7, 4);
        mouse(&mut app, MouseEventKind::Up(left), 7, 4);
        assert_eq!(screen(&mut app, 40, 12)[4], "  │ … 6 more lines");
        assert_eq!(app.take_effects(), [copy("… 6", "3 characters")]);
        // A double click copies a word, then a triple click (on another
        // cell) the line. (Frames are drawn between the clicks.)
        click(&mut app, 9);
        let rows = click(&mut app, 9);
        assert_eq!(rows[4], "  │ … 6 more lines");
        for _ in 0..3 {
            click(&mut app, 10);
        }
        assert_eq!(screen(&mut app, 40, 12)[4], "  │ … 6 more lines");
        assert_eq!(
            app.take_effects(),
            [
                copy("more", "4 characters"),
                copy("more", "4 characters"),
                copy("… 6 more lines", "14 characters"),
            ]
        );
    }

    #[test]
    fn the_theme_picker_opens_with_the_flash_notice() {
        let mut app = app();
        for code in [KeyCode::Down, KeyCode::Enter] {
            if code == KeyCode::Down {
                app.editor.set("/settings");
                app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            }
            app.key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        let rows = screen(&mut app, 100, 30);
        let all = rows.join("\n");
        assert!(all.contains("Theme previews"), "{all}");
        assert!(all.contains("flash"), "{all}");
        assert!(all.contains(" Preview themes    No previews "), "{all}");
        assert!(all.contains("Enter  go on"), "{all}");
        // Answered, it is gone and the list shows.
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let all = screen(&mut app, 100, 30).join("\n");
        assert!(!all.contains("Theme previews"), "{all}");
        assert!(all.contains("Enter  keep"), "{all}");
    }

    #[test]
    fn the_transcript_fills_the_rest() {
        let mut app = app();
        let mut item = Item::new(Kind::Reply, "Hello **there**.");
        item.id = 1;
        app.update(Update::Item(item), Activity::default);
        let rows = screen(&mut app, 40, 7);
        assert_eq!(rows[0], "  Hello there.");
    }

    /// The picker's two steps, drawn: a box with the whose-model title,
    /// the models in aligned columns with the one now marked, the ways that
    /// can't be used with what they need, the efforts with Back, and the
    /// keys as keycaps.
    #[test]
    fn the_model_picker_draws_both_steps() {
        let mut app = app();
        app.model_needs = |reachable, name| {
            if name.starts_with("cc:") {
                Some("Claude Code signed in".into())
            } else {
                crate::tui::model_picker::needs(reachable, name)
            }
        };
        app.set_reading(crate::remote::Reading {
            models: crate::remote::Models {
                main: "gpt-6-luna".into(),
                main_effort: "high".into(),
                workers: "gpt-6-luna".into(),
                workers_effort: "high".into(),
                workers_follow: true,
                reachable: vec!["openai".into()],
                openai_auth: Some("OpenAI API key".into()),
                has_workers: true,
                main_fast: false,
                ..Default::default()
            },
            ..Default::default()
        });
        app.open_model(crate::model_command::Target::Main);
        let rows = screen(&mut app, 100, 30).join("\n");
        assert!(rows.contains("┌ Main agent's model "), "{rows}");
        assert!(rows.contains("OpenAI, with your API key"), "{rows}");
        assert!(
            rows.contains("▸ gpt-6-luna   luna   now, high effort"),
            "{rows}"
        );
        assert!(rows.contains("  gpt-6.1-sol  sol"), "{rows}");
        assert!(
            rows.contains("Google Gemini, billed per token  needs GEMINI_API_KEY"),
            "{rows}"
        );
        assert!(rows.contains("│ Another model  "), "{rows}");
        assert!(
            rows.contains("│ Type a model ID, an alias, cc:<model> or openrouter:<slug>…"),
            "{rows}"
        );
        assert!(rows.contains(" Enter  next   Esc  close"), "{rows}");
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let rows = screen(&mut app, 100, 30).join("\n");
        assert!(
            rows.contains("┌ Main agent's effort on gpt-6-luna "),
            "{rows}"
        );
        assert!(rows.contains("▸ high    thinks more  now"), "{rows}");
        assert!(rows.contains("  max     thinks most, slowest"), "{rows}");
        assert!(rows.contains("← Back to the models"), "{rows}");
        assert!(rows.contains(" Enter  apply   Esc  back"), "{rows}");
    }

    /// `/resume` opens the picker of the conversations over an 80-column
    /// screen, on the one used last before the current one. Enter sends
    /// the session `/resume` with its number. While a turn runs, the
    /// session answers instead.
    #[test]
    fn the_resume_picker_fits_an_80_column_screen() {
        use crate::remote::ConversationEntry;
        use crate::tui::app::Effect;
        use crate::ui::Input;
        let mut app = app();
        use crate::tui::resume_picker::tests::noon;
        let ago = |minutes: i64| Some(noon().to_utc() - chrono::Duration::minutes(minutes));
        app.reading.conversations = vec![
            ConversationEntry {
                number: 3,
                title: "Make the login page remember the user between visits, with a cookie that lasts a month".into(),
                last_used: ago(0),
                current: true,
            },
            ConversationEntry {
                number: 2,
                title: "Add a README with a usage section".into(),
                last_used: ago(125),
                current: false,
            },
            ConversationEntry {
                number: 1,
                title: "Fix the build on Windows".into(),
                last_used: ago(190),
                current: false,
            },
        ];
        let type_line = |app: &mut App, text: &str| {
            for c in text.chars() {
                app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        };
        type_line(&mut app, "/resume");
        if let Some(crate::tui::overlay::Overlay::Resume(picker)) = &mut app.overlay {
            picker.now = noon();
        }
        let rows = screen(&mut app, 80, 24);
        println!("{}", rows.join("\n"));
        let text = rows.join("\n");
        assert!(text.contains("┌ Conversations "), "{text}");
        assert!(
            text.contains(
                "│   Make the login page remember the user between…  just now         current │"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "│ ▸ Add a README with a usage section               2 hours ago              │"
            ),
            "{text}"
        );
        assert!(text.contains(" Enter  switch   Esc  close"), "{text}");
        assert!(app.take_effects().is_empty());
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.overlay.is_none());
        assert_eq!(
            app.take_effects(),
            [Effect::Input(Input::Line("/resume 2".into()))]
        );
        app.set_activity(Activity {
            working: true,
            since: None,
            question: None,
        });
        type_line(&mut app, "/resume");
        assert!(app.overlay.is_none());
        assert_eq!(
            app.take_effects(),
            [Effect::Input(Input::Line("/resume".into()))]
        );
    }
}

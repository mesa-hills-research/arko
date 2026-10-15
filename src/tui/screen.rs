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

use super::app::App;
use super::markdown;
use super::overlay::{self, hints};
use super::transcript::{self, View};
use super::wrap::{self, width as text_width};
use crate::commands;
use crate::glyphs;

/// The input box grows to this many lines, then scrolls.
const INPUT_LINES: usize = 8;
/// The command menu shows this many commands at most.
const MENU_LINES: usize = 8;

/// The agent list shows this many rows at most, scrolling within.
const LIST_LINES: usize = 6;

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
    let list_rows = app
        .list()
        .len()
        .min(LIST_LINES)
        .min(area.height as usize / 4) as u16;
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
        draw_list(frame, app, list);
    } else {
        app.list_drawn = Default::default();
    }
    let status_y = input_area.y - 1;
    draw_status(frame, app, Rect::new(area.x, status_y, area.width, 1));

    // What docks above the status line for a while.
    let above = Rect::new(area.x, area.y, area.width, status_y - area.y);
    let docked = if !app.menu_items().is_empty() {
        draw_menu(frame, app, above)
    } else if app.question_shown().is_some() {
        draw_question(frame, app, above)
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
            "Answer the agent's question"
        } else if app.watching.is_some() {
            "Message the main agent"
        } else if app.activity.working {
            "Add to the task"
        } else {
            "Message"
        };
        let mut spans = vec![Span::styled(placeholder.to_string(), theme.dim())];
        // How to find the rest, while there is room: on an empty box only.
        let hint = "  / for commands";
        if !app.activity.working && text_width(placeholder) + text_width(hint) < width as usize {
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
        // What a command takes, after its name.
        let typed = app.editor.shown();
        if let Some(name) = typed.strip_suffix(' ')
            && let Some(command) = commands::find(name)
            && !command.args.is_empty()
            && !name.contains(' ')
        {
            let column = (text_width_of(lines.last()) as u16).min(width);
            frame.buffer_mut().set_line(
                x + column,
                rect.bottom() - 1,
                &Line::from(Span::styled(command.args.to_string(), theme.dim())),
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

/// The agent list, below the input: `● main` and a row for each worker,
/// the one shown marked; ↓ from the box moves into it, Enter shows a
/// worker's own transcript.
fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let rows = area.height as usize;
    let count = app.list().len();
    let focus = app.list_focus.map(|row| row.min(count - 1));
    let first = focus.map_or(0, |row| row.saturating_sub(rows - 1));
    app.list_drawn = (area, first);
    let theme = &app.theme;
    let list = app.list();
    let id_width = list
        .iter()
        .map(|row| row.map_or(4, |w| w.id.len()))
        .max()
        .unwrap_or(4);
    for (index, row) in list.iter().enumerate().skip(first).take(rows) {
        let y = area.y + (index - first) as u16;
        let shown = row.map(|w| w.id.as_str()) == app.watching.as_deref();
        let bullet = if shown {
            glyphs::render("●").into_owned()
        } else {
            glyphs::render("○").into_owned()
        };
        let name_style = if shown {
            theme.text().add_modifier(Modifier::BOLD)
        } else {
            theme.text()
        };
        let mut left = vec![
            Span::raw(" "),
            Span::styled(
                format!("{bullet} "),
                if shown { theme.accent() } else { theme.dim() },
            ),
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
                let (state, style) = transcript::state(worker, theme);
                right.push(Span::styled(state.to_string(), style));
                if let Some(time) = transcript::worker_time(worker) {
                    right.push(Span::styled(format!(" · {time}"), theme.dim()));
                }
                right.push(Span::styled(
                    format!(
                        " · {} tool call{} ",
                        worker.tool_calls,
                        if worker.tool_calls == 1 { "" } else { "s" }
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

fn text_width_of(spans: Option<&Vec<Span>>) -> usize {
    spans.map_or(0, |spans| {
        spans.iter().map(|span| text_width(&span.content)).sum()
    })
}

/// The status line: is it working (or how long the last turn ran), how
/// close is a handover, what does it cost; what is unusual (outside access, a worker watched, a view other
/// than the full one); and the key that matters now.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let dot = glyphs::render("●").into_owned();
    let mut left: Vec<Span<'static>> = vec![Span::raw(" ")];
    let sep = || Span::styled(" · ".to_string(), theme.dim());
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
        } else if app.watching.is_some() {
            &[("Esc", "back")]
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
fn draw_question(frame: &mut Frame, app: &App, above: Rect) -> u16 {
    let theme = &app.theme;
    let Some(question) = app.question_shown() else {
        return 0;
    };
    let inner_width = (above.width as usize).saturating_sub(4).max(4);
    let mut lines: Vec<Vec<Span<'static>>> =
        markdown::render(&question.text, theme.text(), theme, inner_width)
            .into_iter()
            .map(|line| line.spans)
            .collect();
    let room = (above.height as usize / 2).saturating_sub(2).max(1);
    if lines.len() > room {
        lines.truncate(room.saturating_sub(1));
        lines.push(vec![Span::styled(
            glyphs::render("… (the whole question is in the transcript)").into_owned(),
            theme.dim(),
        )]);
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
    for (row, spans) in lines.into_iter().enumerate().take(inner.height as usize) {
        frame.buffer_mut().set_line(
            inner.x + 1,
            inner.y + row as u16,
            &Line::from(spans),
            inner.width.saturating_sub(1),
        );
    }
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
                .any(|row| row.contains("│ ▸ Anthropic      sk-ant-…a1b2    saved by fin")),
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

    #[test]
    fn the_empty_screen_has_the_status_line_and_a_message_box() {
        let mut app = app();
        let rows = screen(&mut app, 60, 8);
        assert_eq!(rows[4], " ○ idle");
        assert_eq!(rows[5], "─".repeat(60));
        assert_eq!(rows[6], " › Message  / for commands");
        assert_eq!(rows[7], "─".repeat(60));
    }

    #[test]
    fn the_status_line_says_what_runs_and_what_it_costs() {
        let mut app = app();
        app.set_activity(Activity {
            working: true,
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
            question: None,
        });
        let rows = screen(&mut app, 80, 6);
        assert!(
            rows[2].starts_with(" ○ idle · last turn 0s · 142k/250k · $3.20 · quiet"),
            "{}",
            rows[2]
        );
    }

    #[test]
    fn the_question_docks_above_the_status_line() {
        let mut app = app();
        app.set_activity(Activity {
            working: true,
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

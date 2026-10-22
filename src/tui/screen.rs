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
use super::cache;
use super::markdown;
use super::overlay::{self, hints};
use super::select::Docked;
use super::theme::Theme;
use super::transcript::{self, View};
use super::wrap::{self, width as text_width};
use crate::glyphs;
use crate::remote::{AgentStatus, WorkerState, WorkerSummary};

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
    // input box between two rules, the status line, and a rule above it.
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
    let rule_y = status_y - 1;
    frame
        .buffer_mut()
        .set_line(area.x, rule_y, &rule(&theme, area.width), area.width);

    // What docks above the status line's rule for a while.
    let above = Rect::new(area.x, area.y, area.width, rule_y - area.y);
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

/// A rule `width` columns wide, as the input box and the status line have.
fn rule(theme: &Theme, width: u16) -> Line<'static> {
    let style = if theme.is_terminal() {
        theme.dim()
    } else {
        Style::new().fg(theme.border)
    };
    Line::from(Span::styled(
        glyphs::render("─").repeat(width as usize),
        style,
    ))
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
    let rule = rule(theme, area.width);
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

/// How far a worker is indented in the agent list: two columns for each
/// level it is below the main agent's workers.
fn indent(worker: &WorkerSummary) -> usize {
    2 * worker.depth.saturating_sub(1) as usize
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
    // The ID column holds the indent of the workers a worker started.
    let id_width = list
        .iter()
        .map(|row| row.map_or(4, |w| indent(w) + w.id.len()))
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
                    format!(
                        "{:<id_width$}",
                        format!("{}{}", " ".repeat(indent(worker)), worker.id)
                    ),
                    name_style,
                ));
                let doing = if worker.state == crate::remote::WorkerState::Running {
                    worker.last.clone().unwrap_or_else(|| worker.title.clone())
                } else {
                    worker.title.clone()
                };
                // The model, when it isn't the one workers run unless told
                // otherwise.
                let other = worker
                    .model
                    .as_deref()
                    .filter(|model| *model != app.reading.models.workers);
                let tag = match (other, &worker.effort) {
                    (Some(model), Some(effort)) => Some(format!(
                        "{} {effort}",
                        crate::providers::short_model_name(model)
                    )),
                    (Some(model), None) => Some(crate::providers::short_model_name(model)),
                    _ => None,
                };
                left.push(Span::styled(
                    match tag {
                        Some(tag) => format!("  {tag}  {doing}"),
                        None => format!("  {doing}"),
                    },
                    theme.dim(),
                ));
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

/// The worker whose own transcript is shown, with what the session knows of
/// it, which the status line shows in place of the main agent's figures.
struct Watched<'a> {
    /// What the feed keeps of it (its state, run time and cost), once its
    /// summary has come.
    worker: Option<&'a WorkerSummary>,
    /// Its context and the size it hands over at, once a request reported
    /// them.
    context: Option<&'a AgentStatus>,
}

impl<'a> Watched<'a> {
    fn of(app: &'a App) -> Option<Self> {
        let id = app.watching.as_deref()?;
        Some(Self {
            worker: app.transcript.worker(id),
            context: app.reading.agents.iter().find(|agent| agent.id == id),
        })
    }

    /// Its context in tokens and the size it hands over at (0 where not
    /// known).
    fn context(&self) -> (Option<usize>, usize) {
        (
            self.context.and_then(|agent| agent.context_tokens),
            self.context
                .and_then(|agent| agent.handover_at)
                .unwrap_or(0),
        )
    }

    /// What its model calls billed, and what they would cost at API list
    /// prices where a plan covered some.
    fn cost(&self) -> (f64, Option<f64>) {
        self.worker.map_or((0.0, None), |worker| {
            let covered = worker.cost_list > worker.cost_billed + 1e-9;
            (worker.cost_billed, covered.then_some(worker.cost_list))
        })
    }

    /// Whether it works, or how it ended, with how long it has run (in all
    /// its runs, as the agent list says) at `now` in Unix milliseconds. A
    /// worker that isn't in the list yet has nothing to say.
    fn state(&self, theme: &Theme, dot: &str, now: i64) -> Vec<Span<'static>> {
        let Some(worker) = self.worker else {
            return Vec::new();
        };
        let time = transcript::run_time(worker, now).map(|ms| clock(ms.max(0) as u64 / 1000));
        let (word, style) = transcript::state(worker, theme);
        let mut spans = if worker.state == WorkerState::Running {
            vec![Span::styled(format!("{dot} working"), theme.accent())]
        } else {
            vec![Span::styled(word.to_string(), style)]
        };
        spans.extend(time.map(|time| Span::styled(format!(" {time}"), theme.dim())));
        spans
    }
}

/// The status line: is it working (or how long the last turn ran), how
/// close is a handover, what does it cost; what is unusual (outside access, a worker watched, a view other
/// than the full one); and the key that matters now. While a worker is
/// watched, the figures are the worker's own.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let dot = glyphs::render("●").into_owned();
    let mut left: Vec<Span<'static>> = vec![Span::raw(" ")];
    let sep = || Span::styled(glyphs::render(" · ").into_owned(), theme.dim());
    let watched = Watched::of(app);
    if let Some(worker) = &app.watching {
        // A worker's worker says whose it is.
        let under = watched
            .as_ref()
            .and_then(|watched| watched.worker)
            .and_then(|worker| worker.parent.as_deref())
            .map(|parent| format!(" (under {parent})"))
            .unwrap_or_default();
        let bold = theme.accent().add_modifier(Modifier::BOLD);
        left.push(Span::styled(format!("viewing {worker}"), bold));
        if !under.is_empty() {
            left.push(Span::styled(under, bold));
        }
        left.push(sep());
    }
    let worker_state = watched
        .as_ref()
        .map(|watched| watched.state(theme, &dot, chrono::Utc::now().timestamp_millis()));
    if app.activity.question.is_some() {
        // (A worker's view leaves the main agent's question open, and the
        // box answers it.)
        let asks = if watched.is_some() {
            "main asks you"
        } else {
            "asks you"
        };
        left.push(Span::styled(format!("{dot} {asks}"), theme.accent()));
        if let Some(state) = worker_state.filter(|state| !state.is_empty()) {
            left.push(sep());
            left.extend(state);
        }
    } else if let Some(state) = worker_state {
        left.extend(state);
    } else if app.activity.working {
        let time = app
            .working_since
            .map(|since| clock(since.elapsed().as_secs()))
            .unwrap_or_default();
        let word = if app.stopping { "stopping" } else { "working" };
        left.push(Span::styled(format!("{dot} {word}"), theme.accent()));
        left.push(Span::styled(format!(" {time}"), theme.dim()));
    } else if let Some(turn) = app.last_turn {
        // Waiting for the user is the usual state, so it isn't named. The
        // last turn's time stands in place of a line after each turn in the
        // transcript.
        left.push(Span::styled(
            format!("last turn {}", clock(turn.as_secs())),
            theme.dim(),
        ));
    }
    let reading = &app.reading;
    // The risky setting, only while it is on, first.
    if app.outside() {
        left.push(sep());
        left.push(Span::styled("outside access".to_string(), theme.warning()));
    }
    let (tokens, handover) = match &watched {
        Some(watched) => watched.context(),
        None => (reading.context_tokens, app.handover_at()),
    };
    if let Some(tokens) = tokens {
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
    // How long the prompt cache has left while the agent waits, in the
    // warning colour for its last minute.
    if let Some(timer) = app.cache_timer(cache::now_ms()) {
        left.push(sep());
        left.push(Span::styled(
            timer.label(),
            if timer.warns() {
                theme.warning()
            } else {
                theme.dim()
            },
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
    let (cost, api_price) = match &watched {
        Some(watched) => watched.cost(),
        None => (reading.cost, reading.api_price),
    };
    if cost >= 0.005 {
        left.push(sep());
        left.push(Span::styled(format!("${cost:.2}"), theme.dim()));
    }
    if let Some(api) = api_price.filter(|api| *api >= 0.005) {
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
    // The worker's starter goes first when the row is too wide, before the
    // figures at its end are cut.
    if left.iter().map(|span| span.width()).sum::<usize>() > area.width as usize
        && let Some(at) = left
            .iter()
            .position(|span| span.content.starts_with(" (under "))
    {
        left.remove(at);
    }
    // Nothing before the first item: no separator before it either.
    if left
        .get(1)
        .is_some_and(|span| span.content == sep().content)
    {
        left.remove(1);
    }
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
    use crate::remote::{Activity, AgentStatus, Item, Kind, Question, Update, WorkerState};
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
                .any(|row| row.contains("▸ API keys           3 of 5 set ›")),
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

    /// A worker's workers follow it in the agent list, indented two columns
    /// for each level, with their IDs lined up in one column. The family is
    /// listed together whatever order the workers started in.
    #[test]
    fn the_agent_list_shows_a_workers_workers_under_it() {
        use crate::remote::WorkerState::Running;
        use crate::tui::app::tests::worker;
        let mut app = app();
        for (id, parent, depth) in [
            ("r1", None, 1),
            ("r2", None, 1),
            ("r3", Some("r1"), 2),
            ("r4", Some("r3"), 3),
            ("r5", Some("r2"), 2),
        ] {
            let mut summary = worker(id, Running);
            summary.parent = parent.map(str::to_string);
            summary.depth = depth;
            app.update(Update::Worker(summary), Activity::default);
        }
        for width in [100, 80] {
            let rows = screen(&mut app, width, 40);
            let list = &rows[rows.len() - 6..];
            let ids: Vec<&str> = list
                .iter()
                .map(|row| row.split_whitespace().nth(1).unwrap_or_default())
                .collect();
            assert_eq!(ids, ["main", "r1", "r3", "r4", "r2", "r5"], "{list:#?}");
            // The IDs start at the main agent's column plus the indent.
            let at = |row: &String| row.chars().position(|c| c == 'm' || c == 'r').unwrap();
            let columns: Vec<usize> = list.iter().map(at).collect();
            assert_eq!(columns, [3, 3, 5, 7, 3, 5], "{width}: {list:#?}");
        }
        // Tab goes through the family in the list's order.
        let order: Vec<String> = app
            .list()
            .into_iter()
            .map(|row| row.map_or("main".to_string(), |worker| worker.id.clone()))
            .collect();
        assert_eq!(order, ["main", "r1", "r3", "r4", "r2", "r5"]);
        // The counts above and below count rows of the family order.
        let mut crowded = self::app();
        for (id, parent, depth) in [
            ("r1", None, 1),
            ("r2", None, 1),
            ("r3", Some("r1"), 2),
            ("r4", Some("r3"), 3),
            ("r5", Some("r2"), 2),
            ("r6", Some("r4"), 4),
            ("r7", Some("r4"), 4),
            ("r8", Some("r2"), 2),
        ] {
            let mut summary = worker(id, Running);
            summary.parent = parent.map(str::to_string);
            summary.depth = depth;
            crowded.update(Update::Worker(summary), Activity::default);
        }
        let rows = screen(&mut crowded, 80, 40);
        let list = &rows[rows.len() - 7..];
        let ids: Vec<&str> = list
            .iter()
            .map(|row| row.split_whitespace().nth(1).unwrap_or_default())
            .collect();
        assert_eq!(
            ids,
            ["main", "r1", "r3", "r4", "r6", "r7", "3"],
            "{list:#?}"
        );
        assert_eq!(list[6], "   … 3 more");
    }

    /// A worker on another model than the session's workers say so on its
    /// row, with its effort, and one on that model says nothing.
    #[test]
    fn the_agent_list_names_a_workers_model_when_it_is_not_the_default() {
        use crate::remote::WorkerState::Running;
        use crate::tui::app::tests::worker;
        let mut app = app();
        app.reading.models.workers = "gpt-6-luna".into();
        let mut default = worker("r1", Running);
        default.model = Some("gpt-6-luna".into());
        default.effort = Some("high".into());
        let mut other = worker("r2", Running);
        other.model = Some("gpt-6.1-sol".into());
        other.effort = Some("xhigh".into());
        let mut plan = worker("r3", Running);
        plan.model = Some("cc:claude-haiku-5-5".into());
        let unknown = worker("r4", Running);
        for summary in [default, other, plan, unknown] {
            app.update(Update::Worker(summary), Activity::default);
        }
        let rows = screen(&mut app, 100, 30);
        let list = &rows[rows.len() - 4..];
        assert!(list[0].contains("r1    Task of r1"), "{list:#?}");
        assert!(list[1].contains("r2    sol xhigh  Task of r2"), "{list:#?}");
        assert!(list[2].contains("r3    cc:haiku  Task of r3"), "{list:#?}");
        assert!(list[3].contains("r4    Task of r4"), "{list:#?}");
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
                .any(|row| row.contains("▸ Ask first          on   off")),
            "{rows:#?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("With background jobs running, quitting asks")),
            "{rows:#?}"
        );
    }

    #[test]
    fn the_empty_screen_has_the_status_line_and_a_message_box() {
        let mut app = app();
        let rows = screen(&mut app, 60, 8);
        assert_eq!(rows[3], "─".repeat(60));
        assert_eq!(rows[4], "", "nothing to say yet");
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
            rows[2].starts_with(" last turn 0s · 142k/250k · $3.20 · quiet"),
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

    /// The status line row of `rows`, which is two rows above the box's
    /// first line.
    fn status_row(rows: &[String]) -> &str {
        let at = rows
            .iter()
            .position(|row| row.starts_with(" › ") || row.starts_with(" > "))
            .expect("the box");
        &rows[at - 2]
    }

    /// A worker's context, as the status reports it.
    fn context_of(id: &str, tokens: usize, handover: usize) -> AgentStatus {
        AgentStatus {
            id: id.into(),
            context_tokens: Some(tokens),
            context_window: Some(1_000_000),
            handover_at: Some(handover),
            compacts: false,
        }
    }

    /// While a worker is watched, the status line shows the worker's run
    /// time, its context against the size it hands over at, and its cost,
    /// under its name. The main agent's figures are back when the view is.
    #[test]
    fn the_status_line_shows_the_figures_of_the_worker_watched() {
        use crate::tui::app::tests::worker;
        let mut app = app();
        app.set_activity(Activity {
            working: true,
            since: None,
            question: None,
        });
        app.reading.context_tokens = Some(142_000);
        app.reading.cost = 3.2;
        let mut r1 = worker("r1", WorkerState::Running);
        r1.running_since = Some(chrono::Utc::now().timestamp_millis() - 134_100);
        r1.cost_billed = 0.12;
        r1.cost_list = 0.12;
        app.update(Update::Worker(r1), Activity::default);
        app.reading.agents = vec![context_of("r1", 88_000, 190_000)];

        let main = " ● working 0s · 142k/250k · $3.20 · 1 worker";
        let rows = screen(&mut app, 100, 12);
        assert!(status_row(&rows).starts_with(main), "{rows:#?}");
        app.watch(Some("r1".into()));
        let rows = screen(&mut app, 100, 12);
        let status = status_row(&rows);
        assert!(
            status.starts_with(" viewing r1 · ● working 2m14s · 88k/190k · $0.12 · 1 worker"),
            "{status}"
        );
        assert!(status.ends_with("Esc  back"), "{status}");
        assert!(
            !status.contains("142k") && !status.contains("$3.20"),
            "{status}"
        );
        app.watch(None);
        let rows = screen(&mut app, 100, 12);
        assert!(status_row(&rows).starts_with(main), "{rows:#?}");
    }

    /// While a worker that a worker started is watched, the status line
    /// says whose it is.
    #[test]
    fn the_status_line_names_the_parent_of_a_watched_nested_worker() {
        use crate::tui::app::tests::worker;
        let mut app = app();
        app.update(
            Update::Worker(worker("r1", WorkerState::Running)),
            Activity::default,
        );
        let mut nested = worker("r2", WorkerState::Running);
        nested.parent = Some("r1".into());
        nested.depth = 2;
        app.update(Update::Worker(nested), Activity::default);
        app.watch(Some("r2".into()));
        let rows = screen(&mut app, 100, 12);
        assert!(
            status_row(&rows).starts_with(" viewing r2 (under r1) · "),
            "{rows:#?}"
        );
        let rows = screen(&mut app, 80, 12);
        assert!(
            status_row(&rows).starts_with(" viewing r2 (under r1) · "),
            "{rows:#?}"
        );
        app.watch(Some("r1".into()));
        let rows = screen(&mut app, 100, 12);
        assert!(status_row(&rows).starts_with(" viewing r1 · "), "{rows:#?}");
    }

    /// A watched worker's starter is named in the status line while the row
    /// has room, and goes before the count of workers and the plan's usage are
    /// cut.
    #[test]
    fn the_status_line_drops_the_starter_before_it_cuts_a_figure() {
        use crate::tui::app::tests::worker;
        let mut app = app();
        app.reading.plan_usage = Some("Claude plan 5 h 54%, 7 d 58%".into());
        app.update(
            Update::Worker(worker("r2", WorkerState::Running)),
            Activity::default,
        );
        let mut nested = worker("r4", WorkerState::Running);
        nested.parent = Some("r2".into());
        nested.depth = 2;
        nested.running_since = Some(chrono::Utc::now().timestamp_millis() - 43_000);
        app.update(Update::Worker(nested), Activity::default);
        app.reading.agents = vec![context_of("r4", 12_000, 950_000)];
        app.watch(Some("r4".into()));
        let status = |app: &mut App, width: u16| status_row(&screen(app, width, 12)).to_string();
        let wide = status(&mut app, 80);
        assert!(
            wide.starts_with(" viewing r4 (under r2) · ● working ")
                && wide.contains("5 h 54%, 7 d 58%")
                && wide.contains("2 workers"),
            "{wide}"
        );
        // At 72 columns the usage and the count would be cut: the starter
        // goes instead.
        let narrow = status(&mut app, 72);
        assert!(
            narrow.starts_with(" viewing r4 · ● working ")
                && narrow.contains("5 h 54%, 7 d 58%")
                && narrow.contains("2 workers"),
            "{narrow}"
        );
    }

    /// A worker that ended shows its final figures, with its state and how
    /// long it ran, also while the main agent is idle or its turn ended
    /// long ago.
    #[test]
    fn the_status_line_shows_the_final_figures_of_a_worker_that_ended() {
        use crate::tui::app::tests::worker;
        let mut app = app();
        app.last_turn = Some(std::time::Duration::from_secs(5));
        app.reading.context_tokens = Some(142_000);
        app.reading.cost = 3.2;
        for (id, state, word) in [
            ("r1", WorkerState::Finished, "finished"),
            ("r2", WorkerState::Failed, "failed"),
            ("r3", WorkerState::Stopped, "stopped"),
        ] {
            let mut summary = worker(id, state);
            summary.ran_ms = 134_000;
            summary.cost_billed = 0.4;
            summary.cost_list = 0.4;
            app.update(Update::Worker(summary), Activity::default);
            app.reading.agents.push(context_of(id, 181_000, 190_000));
            app.watch(Some(id.into()));
            let rows = screen(&mut app, 100, 14);
            let status = status_row(&rows);
            let line = format!(" viewing {id} · {word} 2m14s · 181k/190k · $0.40");
            assert!(status.starts_with(&line), "{status}");
            assert!(!status.contains("last turn"), "{status}");
        }
        // A worker on a plan: the API price of its calls, besides what it
        // billed.
        let mut plan = worker("r4", WorkerState::Finished);
        plan.ran_ms = 3_725_000;
        plan.cost_list = 0.31;
        app.update(Update::Worker(plan), Activity::default);
        app.watch(Some("r4".into()));
        let rows = screen(&mut app, 100, 14);
        assert!(
            status_row(&rows).starts_with(" viewing r4 · finished 1h02m · ≈ $0.31"),
            "{rows:#?}"
        );
    }

    /// A worker that has no figures yet says only what it does, and the
    /// main agent's don't stand in for them.
    #[test]
    fn the_status_line_of_a_worker_without_figures_shows_none_of_the_main_agents() {
        use crate::tui::app::tests::worker;
        let mut app = app();
        app.set_activity(Activity {
            working: true,
            since: None,
            question: None,
        });
        app.reading.context_tokens = Some(142_000);
        app.reading.cost = 3.2;
        let mut queued = worker("r1", WorkerState::Queued);
        queued.started_at = None;
        app.update(Update::Worker(queued), Activity::default);
        let mut started = worker("r2", WorkerState::Running);
        started.started_at = None;
        app.update(Update::Worker(started), Activity::default);
        app.watch(Some("r1".into()));
        let rows = screen(&mut app, 100, 14);
        assert_eq!(
            status_row(&rows).trim_end().split("   ").next().unwrap(),
            " viewing r1 · queued · 2 workers",
            "{rows:#?}"
        );
        app.watch(Some("r2".into()));
        let rows = screen(&mut app, 100, 14);
        assert_eq!(
            status_row(&rows).trim_end().split("   ").next().unwrap(),
            " viewing r2 · ● working · 2 workers",
            "{rows:#?}"
        );
        // A context without a handover size shows alone.
        app.reading.agents = vec![AgentStatus {
            handover_at: None,
            ..context_of("r2", 12_000, 0)
        }];
        let rows = screen(&mut app, 100, 14);
        assert!(
            status_row(&rows).starts_with(" viewing r2 · ● working · 12k · 2 workers"),
            "{rows:#?}"
        );
    }

    /// The main agent's question stays in sight in a worker's view, named,
    /// and a context near the handover size is a warning there as it is
    /// for the main agent.
    #[test]
    fn a_worker_s_view_keeps_the_main_agents_question_and_warns_near_its_handover() {
        use crate::tui::app::tests::worker;
        let mut app = app();
        let mut r1 = worker("r1", WorkerState::Running);
        r1.running_since = Some(chrono::Utc::now().timestamp_millis() - 65_100);
        app.update(Update::Worker(r1), Activity::default);
        app.reading.agents = vec![context_of("r1", 171_000, 190_000)];
        app.watch(Some("r1".into()));
        app.set_activity(Activity {
            working: true,
            since: None,
            question: Some(Question {
                id: 1,
                text: "Tabs or spaces?".into(),
            }),
        });
        let rows = screen(&mut app, 100, 14);
        assert!(
            status_row(&rows)
                .starts_with(" viewing r1 · ● main asks you · ● working 1m05s · 171k/190k"),
            "{rows:#?}"
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row = (0..14u16)
            .find(|y| {
                (0..100u16)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("viewing r1")
            })
            .unwrap();
        let at = (0..100u16)
            .find(|x| buffer[(*x, row)].symbol() == "1" && buffer[(*x + 1, row)].symbol() == "7")
            .unwrap();
        assert_eq!(buffer[(at, row)].fg, app.theme.warning().fg.unwrap());
        // Below nine tenths it isn't one.
        app.reading.agents = vec![context_of("r1", 170_000, 190_000)];
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        assert_ne!(buffer[(at, row)].fg, app.theme.warning().fg.unwrap());
    }

    /// A session on the mock model whose main agent started a worker that
    /// finished. The screen watching the worker shows the context its latest
    /// request reported against the handover size of its model, and the
    /// main agent's own figures once the view goes back.
    #[tokio::test]
    async fn the_screen_of_a_session_shows_the_context_of_a_worker_that_ran() {
        use crate::tui::app::Info;
        use crate::tui::input::{Editor, History};
        use crate::tui::theme::{Catalog, Depth};
        let (reading, snapshot) =
            crate::harness::tests::session_with_a_worker_that_ran(7_777).await;
        let mut app = App::new(
            snapshot,
            Editor::new(History::load(None)),
            Info {
                workspace: "work".into(),
                ..Info::default()
            },
            Catalog::load(None),
            Depth::Rgb,
        );
        app.set_reading(reading);
        let main_tokens = app
            .reading
            .context_tokens
            .expect("the main agent's context");
        assert!(main_tokens < 7_000, "{main_tokens}");

        app.watch(Some("r1".into()));
        let rows = screen(&mut app, 100, 20);
        let status = status_row(&rows);
        assert!(status.starts_with(" viewing r1 · finished"), "{status}");
        assert!(status.contains(" · 7k/200k"), "{status}");

        app.watch(None);
        let rows = screen(&mut app, 100, 20);
        let status = status_row(&rows);
        assert!(!status.contains("viewing"), "{status}");
        assert!(
            status.contains(&format!(" {}k/200k", main_tokens / 1000)),
            "{status}"
        );
    }

    /// At 80 columns the watched worker's line fits with its hint, and in
    /// plain ASCII it has nothing else.
    #[test]
    fn the_status_line_of_a_watched_worker_fits_80_columns_in_ascii() {
        use crate::glyphs::Symbols;
        use crate::glyphs::tests::choose_symbols;
        use crate::tui::app::tests::worker;
        let _chosen = choose_symbols(Symbols::Ascii);
        let mut app = app();
        let mut r1 = worker("r1", WorkerState::Running);
        r1.running_since = Some(chrono::Utc::now().timestamp_millis() - 134_100);
        r1.cost_billed = 0.12;
        r1.cost_list = 0.31;
        app.update(Update::Worker(r1), Activity::default);
        app.reading.agents = vec![context_of("r1", 88_000, 190_000)];
        app.reading.commands = vec!["r1".into()];
        app.watch(Some("r1".into()));
        let rows = screen(&mut app, 80, 14);
        let status = status_row(&rows);
        assert!(
            status.starts_with(
                " viewing r1 | * working 2m14s | 88k/190k | $0.12 | ~ $0.31 | 1 worker"
            ),
            "{status}"
        );
        assert!(status.is_ascii(), "{status}");
        assert!(status.chars().count() <= 80, "{status}");
        // Without the cost, the hints have room beside the figures.
        app.update(
            Update::Worker(crate::remote::WorkerSummary::new("r1")),
            Activity::default,
        );
        let rows = screen(&mut app, 80, 14);
        let status = status_row(&rows);
        assert!(
            status.ends_with("Ctrl+B  background   Esc  back") && status.is_ascii(),
            "{status}"
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
        assert!(rows[2].contains("The agent asks"), "{rows:?}");
        assert!(rows[3].contains("Tabs or spaces?"));
        assert!(rows[4].contains("Enter  answer"));
        assert_eq!(rows[5], "─".repeat(50));
        assert_eq!(rows[6], " ● asks you");
        assert_eq!(rows[8], " › Answer the agent's question");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let rows = screen(&mut app, 50, 10);
        assert!(!rows.iter().any(|row| row.contains("Tabs or spaces?")));
        assert_eq!(rows[6], " ● asks you");
    }

    /// The settings rows for the workers carry the library's names and
    /// sentences, as the web page and an app's settings page do.
    #[test]
    fn the_worker_settings_rows_read_as_the_library_words_them() {
        use crate::tui::overlay::{Overlay, SettingsRow};
        use crate::workers;
        let mut app = app();
        app.reading.models.has_workers = true;
        app.reading.settings.message_copies = true;
        for (row, name, about) in [
            (
                SettingsRow::WorkerModels,
                workers::MODELS_NAME,
                format!(
                    "{} {} {}",
                    workers::MODELS_ABOUT,
                    app.worker_models().about(),
                    workers::MODELS_APPLIES
                ),
            ),
            (
                SettingsRow::WorkerDepth,
                workers::DEPTH_NAME,
                workers::depth_about().to_string(),
            ),
            (
                SettingsRow::MessageCopies,
                workers::COPIES_NAME,
                workers::copies_about(true).to_string(),
            ),
        ] {
            let at = app
                .settings_rows()
                .iter()
                .position(|other| *other == row)
                .unwrap();
            app.overlay = Some(Overlay::Settings {
                row: at,
                picker: None,
            });
            let rows = screen(&mut app, 80, 40);
            assert!(
                rows.iter().any(|line| line.contains(&format!("▸ {name}"))),
                "{name}\n{}",
                rows.join("\n")
            );
            // The description wraps in the box, so compare it by its words.
            let text = rows
                .iter()
                .map(|line| line.trim_matches(|c: char| c == '│' || c.is_whitespace()))
                .collect::<Vec<_>>()
                .join(" ");
            let first_words = about
                .split_whitespace()
                .take(8)
                .collect::<Vec<_>>()
                .join(" ");
            assert!(text.contains(&first_words), "{first_words}\n{text}");
        }
    }

    /// The question to save a profile with long instructions is cut to fit
    /// the dock, and what is left still says how much is cut.
    #[test]
    fn a_cut_approval_question_still_says_how_long_the_instructions_are() {
        use crate::profiles::{Place, Profile};
        let mut profile = Profile::new("judge", "Judges renders against a reference image");
        profile.model = Some("sol".into());
        profile.instructions = (1..=30)
            .map(|line| format!("Check point {line} of the render."))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = app();
        app.set_activity(Activity {
            working: true,
            since: None,
            question: Some(Question {
                id: 1,
                text: crate::profiles::approval_text(&profile, Place::User),
            }),
        });
        for (width, height) in [(80, 24), (100, 24), (120, 30)] {
            let rows = screen(&mut app, width, height).join("\n");
            assert!(
                rows.contains("whole question is in the transcript"),
                "{rows}"
            );
            assert!(
                rows.contains("Its instructions are 890 characters (30 lines)"),
                "{width}x{height}\n{rows}"
            );
        }
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
            summary.running_since = Some(now - 134_100);
            summary.cost_billed = 0.12;
            summary.cost_list = 0.31;
            app.update(Update::Worker(summary), Activity::default);
            app.reading.agents = vec![context_of("r1", 171_000, 190_000)];
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

    /// Not a test: how long a frame of a long transcript takes while
    /// Working… or the thinking that streams moves on it, and how much of a
    /// core the frames take at ten a second. Run it with
    /// `cargo test --lib animation_cost -- --ignored --nocapture`, and
    /// with `--release` for a release build's numbers.
    #[test]
    #[ignore]
    #[cfg(unix)]
    fn animation_cost() {
        use crate::remote::{CallStatus, Delivery, WorkerState};
        use crate::tui::app::tests::worker;
        use std::time::{Duration, Instant};
        // The CPU time this thread has used.
        let cpu = || {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `time` is a valid timespec for the call to fill.
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
            Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
        };
        let long = |n: usize, line: &str| vec![line; n].join("\n");
        let mut items = Vec::new();
        for turn in 0..300 {
            let mut user = Item::new(Kind::User, format!("Turn {turn}: fix the parser's tests"));
            user.delivery = Some(Delivery::Turn);
            items.push(user);
            items.push(Item::new(
                Kind::Thinking,
                "**Planning**\nRead the tests first.\nThen the parser.",
            ));
            items.push(Item::new(
                Kind::Text,
                "Reading the **parser** first, then `cargo test`:\n\n```rust\nfn main() {}\n```",
            ));
            let mut call = Item::new(Kind::Call, "$ cargo test --lib parser");
            call.status = Some(CallStatus::Ok);
            call.outcome = Some("12 passed".into());
            items.push(call);
            let mut result = Item::new(Kind::Result, "exit code 0, 1.2 s")
                .detail(long(5, "test parser::tests::reads ... ok"));
            result.whole = Some(long(40, "test parser::tests::reads ... ok").into());
            items.push(result);
            let mut read = Item::new(Kind::Call, "read src/parser.rs");
            read.status = Some(CallStatus::Ok);
            items.push(read);
            items
                .push(Item::new(Kind::Notice, "retrying in 2 s").level(crate::remote::Level::Warn));
            items.push(Item::new(
                Kind::Text,
                "The parser skipped blank lines at the end, so the last test failed.",
            ));
            items.push(Item::new(
                Kind::Reply,
                "Fixed.\n\n- The parser keeps blank lines.\n- All **12** tests pass.",
            ));
            items.push(Item::new(Kind::Turn, "3 s · $0.010"));
        }
        for n in 0..1000 {
            let mut call = Item::new(Kind::Call, format!("$ cargo build -p crate{n}")).agent("r1");
            call.status = Some(CallStatus::Ok);
            items.push(call);
        }
        let mut user = Item::new(Kind::User, "And the docs?");
        user.delivery = Some(Delivery::Turn);
        items.push(user);
        let mut call = Item::new(Kind::Call, "read README.md");
        call.status = Some(CallStatus::Ok);
        items.push(call);
        let now = chrono::Utc::now().timestamp_millis();
        let mut app = app();
        for (id, mut item) in (1..).zip(items) {
            item.id = id;
            item.at = now;
            item.turn = Some(id / 10);
            app.update(Update::Item(item), Activity::default);
        }
        app.update(
            Update::Worker(worker("r1", WorkerState::Running)),
            Activity::default,
        );
        app.set_activity(Activity {
            working: true,
            since: Some(now),
            question: None,
        });
        // A terminal of 120 by 40 that the frames' escape codes go to, as
        // they would to the screen: what changed since the last frame.
        struct Count(std::rc::Rc<std::cell::Cell<usize>>);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.set(self.0.get() + bytes.len());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = std::rc::Rc::new(std::cell::Cell::new(0));
        let backend = ratatui::backend::CrosstermBackend::new(Count(bytes.clone()));
        let viewport = ratatui::Viewport::Fixed(Rect::new(0, 0, 120, 40));
        let mut terminal =
            Terminal::with_options(backend, ratatui::TerminalOptions { viewport }).unwrap();
        // Draws a frame, and returns how many bytes it wrote.
        let mut frame = |app: &mut App| {
            terminal.draw(|frame| draw(frame, app)).unwrap();
            bytes.replace(0)
        };
        let mut measure = |app: &mut App, what: &str| {
            // The first frame fills the lines' caches.
            frame(app);
            assert!(app.transcript.drawn.moving, "{what} moves");
            let mut times: Vec<Duration> = (0..300)
                .map(|_| {
                    let start = Instant::now();
                    frame(app);
                    start.elapsed()
                })
                .collect();
            times.sort();
            let mean = times.iter().sum::<Duration>() / times.len() as u32;
            // Ten frames a second for three seconds, as the screen draws
            // them: the share of a core they take.
            let (wall, used) = (Instant::now(), cpu());
            let mut written = 0;
            for tick in 1..=30 {
                written += frame(app);
                std::thread::sleep(
                    (wall + Duration::from_millis(100 * tick))
                        .saturating_duration_since(Instant::now()),
                );
            }
            let share = (cpu() - used).as_secs_f64() / wall.elapsed().as_secs_f64();
            // Of a frame, the transcript's part.
            let area = Rect::new(0, 0, 120, 30);
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            let (filter, theme) = (app.filter(), app.theme.clone());
            let start = Instant::now();
            for _ in 0..300 {
                app.transcript.draw(&mut buffer, area, &filter, &theme);
            }
            let transcript = start.elapsed() / 300;
            println!(
                "{what}: a frame {mean:.2?} on average ({transcript:.2?} of it the transcript), {:.2?} at the median, {:.2?} at most, {} bytes to the terminal; {:.1}% of a core at 10 frames a second",
                times[times.len() / 2],
                times[times.len() - 1],
                written / 30,
                share * 100.0,
            );
        };
        measure(&mut app, "Working… on 4000 items");
        let mut thinking = Item::new(Kind::Thinking, "**Checking the docs**\nThe README first.");
        thinking.id = 10_000;
        thinking.at = now;
        app.update(Update::Item(thinking), Activity::default);
        measure(&mut app, "thinking that streams, on 4000 items");
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
            rows[2].starts_with("   /watch  follow one worker"),
            "{rows:?}"
        );
        assert!(rows[2].ends_with("WORKER|main"), "{rows:?}");
        assert_eq!(rows[6], " › /wa");
        // An argument's completion follows the cursor.
        for c in "tch m".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let rows = screen(&mut app, 100, 8);
        assert_eq!(rows[6], " › /watch main");
    }

    /// The menu line of `/worker-depth` says in its first 62 columns what the
    /// number is and what 1 means, so an 80-column screen, which cuts the
    /// rest, still says it.
    #[test]
    fn the_worker_depth_menu_line_says_what_it_sets_at_80_columns() {
        let mut app = app();
        for c in "/worker-d".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let rows = screen(&mut app, 80, 8);
        let line = rows
            .iter()
            .find(|row| row.contains("/worker-depth"))
            .unwrap_or_else(|| panic!("{rows:?}"));
        assert!(
            line.starts_with(
                "   /worker-depth  how deep workers go, from 1 to 4. 1 means only the main agent"
            ),
            "{line}"
        );
    }

    /// The help lists what each command takes after what it does, as the menu
    /// does for the row chosen, so a description needn't give an example.
    #[test]
    fn the_help_lists_what_a_command_takes() {
        use crate::tui::overlay::Overlay;
        let mut app = app();
        app.overlay = Some(Overlay::Help { scroll: 0 });
        // The box wraps the lines, so read the words in order.
        let text = screen(&mut app, 140, 120)
            .iter()
            .map(|row| row.trim_matches(|c: char| c == '│' || c.is_whitespace()))
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for (command, takes) in [("/worker-depth", " 1-4 "), ("/message-copies", " on|off ")] {
            let at = text.find(command).unwrap_or_else(|| panic!("{text}"));
            let next = text[at + command.len()..]
                .find(" /")
                .map_or(text.len(), |end| at + command.len() + end);
            assert!(
                format!("{} ", &text[at..next]).contains(takes),
                "{command}\n{text}"
            );
        }
    }

    /// The menu line of `/message-copies` says in its first columns what it
    /// sets, which an 80-column screen cuts the rest of.
    #[test]
    fn the_message_copies_menu_line_says_what_it_sets_at_80_columns() {
        let mut app = app();
        for c in "/message-c".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let rows = screen(&mut app, 80, 8);
        let line = rows
            .iter()
            .find(|row| row.contains("/message-copies"))
            .unwrap_or_else(|| panic!("{rows:?}"));
        assert!(
            line.starts_with(
                "   /message-copies  copies of your messages to a worker for the agent"
            ),
            "{line}"
        );
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
        screen(&mut app, 40, 13);
        app.transcript.scroll(-4);
        let rows = screen(&mut app, 40, 13);
        assert_eq!(rows[0], "  │ line 1");
        assert_eq!(rows[4], "  │ … 6 more lines");
        let left = MouseButton::Left;
        let click = |app: &mut App, x: u16| {
            mouse(app, MouseEventKind::Down(left), x, 4);
            mouse(app, MouseEventKind::Up(left), x, 4);
            screen(app, 40, 13)
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
        assert_eq!(screen(&mut app, 40, 13)[4], "  │ … 6 more lines");
        assert_eq!(app.take_effects(), [copy("… 6", "3 characters")]);
        // A double click copies a word, then a triple click (on another
        // cell) the line. (Frames are drawn between the clicks.)
        click(&mut app, 9);
        let rows = click(&mut app, 9);
        assert_eq!(rows[4], "  │ … 6 more lines");
        for _ in 0..3 {
            click(&mut app, 10);
        }
        assert_eq!(screen(&mut app, 40, 13)[4], "  │ … 6 more lines");
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

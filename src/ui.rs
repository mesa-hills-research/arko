//! The terminal: a prompt that stays at the bottom while the agents' work
//! prints above it, and how their events are shown.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use arko_agents::builtins::{
    APPLY_DIFF_TOOL_NAME, ASK_USER_TOOL_NAME, LIST_FILES_TOOL_NAME, READ_FILE_TOOL_NAME,
    WRITE_FILE_TOOL_NAME,
};
use arko_agents::handover::{HANDOVER_REPORT_PARAM_NAME, HANDOVER_REPORT_TOOL_NAME};
use arko_agents::terminal::TerminalTool;
use arko_agents::{AgentEvent, LOAD_SKILL_TOOL_NAME, TaskState};
use arko_workflows::WorkflowEvent;
use arko_workflows::services::RUN_WORKFLOW_AGENT_TOOL_NAME;
use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{ExternalPrinter, Helper};
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;

use crate::agents::MAIN;
use crate::crew;
use crate::glyphs;
use crate::index;
use crate::mcp;
use crate::message;
use crate::remote;
use crate::shell::SHELL_TOOL_NAME;

pub const PROMPT: &str = "› ";

/// What the user did at the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Line(String),
    /// Ctrl-C.
    Interrupt,
    /// Ctrl-D, or the end of standard input.
    Eof,
}

/// Where everything is printed: above the prompt when there is one, else
/// to standard output (and, in one-shot mode, progress to standard error).
/// While the full-screen view owns the terminal, lines go to the feed it
/// shows instead, and echoes (what the feed has already) nowhere.
pub struct Output {
    printer: Mutex<Option<Box<dyn ExternalPrinter + Send>>>,
    /// The full-screen view's feed, while it shows the session.
    feed: Mutex<Option<remote::Feed>>,
    color: bool,
    /// Progress goes to standard error; only answers go to standard output.
    progress_to_stderr: bool,
}

impl Output {
    pub fn new(printer: Option<Box<dyn ExternalPrinter + Send>>, progress_to_stderr: bool) -> Self {
        let terminal = if progress_to_stderr {
            std::io::stderr().is_terminal()
        } else {
            std::io::stdout().is_terminal()
        };
        let color = terminal && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
        Self {
            printer: Mutex::new(printer),
            feed: Mutex::new(None),
            color,
            progress_to_stderr,
        }
    }

    /// Prints straight to the terminal from now on, not above the prompt
    /// (nor to the full-screen view's feed).
    pub fn detach(&self) {
        self.printer.lock().unwrap().take();
        self.feed.lock().unwrap().take();
    }

    /// Prints above the line prompt `printer` from now on.
    pub fn attach(&self, printer: Box<dyn ExternalPrinter + Send>) {
        *self.printer.lock().unwrap() = Some(printer);
    }

    /// Sends lines to `feed` from now on, which the full-screen view shows;
    /// echoes are left out.
    pub fn to_feed(&self, feed: remote::Feed) {
        *self.feed.lock().unwrap() = Some(feed);
    }

    /// Clears the line the cursor is on (the prompt's).
    pub fn clear_line(&self) {
        if self.color {
            let mut stdout = std::io::stdout().lock();
            let _ = write!(stdout, "\r\x1b[2K");
            let _ = stdout.flush();
        }
    }

    /// Prints `text` (one or more lines) as progress, with its symbols as
    /// the terminal shows them.
    pub fn line(&self, text: &str) {
        if let Some(feed) = self.feed.lock().unwrap().as_ref() {
            feed.output(&remote::plain(text));
            return;
        }
        let text = glyphs::render(text);
        let mut printer = self.printer.lock().unwrap();
        if let Some(printer) = printer.as_mut() {
            if printer.print(format!("{text}\n")).is_ok() {
                return;
            }
        }
        if self.progress_to_stderr {
            eprintln!("{text}");
        } else {
            println!("{text}");
        }
    }

    /// Prints a line the feed has already (the user's text, a notice, the
    /// agent's question or reply, the end of a turn), or one only the line
    /// prompt needs: as [`line`](Self::line) does, but not while the
    /// full-screen view shows the feed.
    pub fn echo(&self, text: &str) {
        if self.feed.lock().unwrap().is_none() {
            self.line(text);
        }
    }

    /// Prints the agent's reply: to standard output in one-shot mode. (The
    /// full-screen view has it from the feed.)
    pub fn answer(&self, text: &str) {
        if self.feed.lock().unwrap().is_some() {
            return;
        }
        let text = glyphs::render(text);
        if self.progress_to_stderr {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{text}");
            let _ = stdout.flush();
        } else {
            self.line(&text);
        }
    }

    /// The same output, but always in colour: a command's lines as the
    /// feed takes them, which keep their warnings' level that way.
    pub fn colored(mut self) -> Self {
        self.color = true;
        self
    }

    /// Whether lines are printed in colour.
    pub fn in_color(&self) -> bool {
        self.color
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn accent(&self, text: &str) -> String {
        self.paint("36", text)
    }

    /// The level of a line [`warn`](Self::warn) or [`error`](Self::error)
    /// coloured (in colour), else `None`.
    pub fn level_of(text: &str) -> Option<remote::Level> {
        if text.starts_with("\x1b[33m") {
            Some(remote::Level::Warn)
        } else if text.starts_with("\x1b[31m") {
            Some(remote::Level::Error)
        } else {
            None
        }
    }

    pub fn warn(&self, text: &str) -> String {
        self.paint("33", text)
    }

    pub fn error(&self, text: &str) -> String {
        self.paint("31", text)
    }
}

/// Lines ending with `\` continue on the next line.
struct InputHelper;

impl Completer for InputHelper {
    type Candidate = String;
}

impl Hinter for InputHelper {
    type Hint = String;
}

impl Highlighter for InputHelper {}

impl Validator for InputHelper {
    fn validate(&self, context: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        Ok(if context.input().ends_with('\\') {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Valid(None)
        })
    }
}

impl Helper for InputHelper {}

/// A line as typed: continuation backslashes removed.
fn joined(line: &str) -> String {
    line.replace("\\\r\n", "\n").replace("\\\n", "\n")
}

/// Set when fin is closing: the line editor doesn't read again, so it
/// doesn't put the terminal back in raw mode after it was restored.
static CLOSING: AtomicBool = AtomicBool::new(false);

/// Stops the line editor once the line it is reading (if any) ends.
pub fn stop_reading() {
    CLOSING.store(true, Ordering::SeqCst);
}

/// Starts reading the user's input on its own thread. With a terminal, the
/// prompt is a line editor with history, and the returned printer prints
/// above it; without one, lines are read from standard input as they come.
/// `/keys add PROVIDER` is read here, with the key's input hidden, and
/// added to `keys`: neither reaches the session as a line.
pub fn spawn_input(
    history: PathBuf,
    sender: mpsc::UnboundedSender<Input>,
    keys: Option<Arc<crate::keys::Keys>>,
) -> Option<Box<dyn ExternalPrinter + Send>> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let (printer_sender, printer_receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let config = rustyline::Config::builder()
                .auto_add_history(true)
                .max_history_size(1_000)
                .and_then(|config| config.history_ignore_dups(true))
                .map(|config| config.build())
                .unwrap_or_default();
            let mut editor = match rustyline::Editor::<
                InputHelper,
                rustyline::history::DefaultHistory,
            >::with_config(config)
            {
                Ok(editor) => editor,
                Err(_) => {
                    let _ = printer_sender.send(None);
                    return read_lines(sender);
                }
            };
            editor.set_helper(Some(InputHelper));
            let _ = editor.load_history(&history);
            let printer = editor
                .create_external_printer()
                .ok()
                .map(|printer| Box::new(printer) as Box<dyn ExternalPrinter + Send>);
            let _ = printer_sender.send(printer);
            let prompt = glyphs::render(PROMPT).into_owned();
            while !CLOSING.load(Ordering::SeqCst) {
                let input = match editor.readline(prompt.as_str()) {
                    Ok(line) => {
                        if let Some(keys) = &keys
                            && let Some(key) = crate::keys::added_at_prompt(&line)
                        {
                            crate::keys::add_at_prompt(keys, key);
                            continue;
                        }
                        Input::Line(joined(&line))
                    }
                    Err(ReadlineError::Interrupted) => Input::Interrupt,
                    Err(_) => Input::Eof,
                };
                let eof = matches!(input, Input::Eof);
                if sender.send(input).is_err() || eof {
                    break;
                }
                if let Some(parent) = history.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = editor.save_history(&history);
            }
        });
        printer_receiver.recv().ok().flatten()
    } else {
        std::thread::spawn(move || read_lines(sender));
        None
    }
}

fn read_lines(sender: mpsc::UnboundedSender<Input>) {
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if sender.send(Input::Line(line)).is_err() {
            return;
        }
    }
    let _ = sender.send(Input::Eof);
}

/// What the footer after each turn shows, gathered from the events.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// The input of the main agent's latest model request, in tokens.
    pub context_tokens: Option<usize>,
    /// The subscription plans' usage windows, as last reported: one line
    /// for every plan the session's agents use (ChatGPT, Claude).
    pub plan_usage: Option<String>,
    /// Each plan's part of `plan_usage`, by plan (`providers::plan_name`).
    pub plans: std::collections::BTreeMap<String, String>,
}

/// A request to the event printer.
pub enum Control {
    /// Print what is buffered, then answer.
    Flush(oneshot::Sender<()>),
}

/// Prints a workflow's events until its channel closes. Native worker runs
/// use their r1, r2, ... handles in the terminal and remote feed. With
/// `text_replies` (the main agent ends its turns with a reply in plain
/// text), its text is held until it proves to be progress or its reply.
#[allow(clippy::too_many_arguments)]
pub async fn print_events(
    mut events: broadcast::Receiver<WorkflowEvent>,
    mut control: mpsc::UnboundedReceiver<Control>,
    output: Arc<Output>,
    stats: Arc<Mutex<Stats>>,
    show_thinking: bool,
    text_replies: bool,
    workflow: Option<std::sync::Weak<arko_workflows::Workflow>>,
    feed: Option<remote::Feed>,
) {
    let mut printer = EventPrinter::new(output, stats, show_thinking, None);
    printer.hold_text = text_replies;
    loop {
        tokio::select! {
            biased;
            event = events.recv() => match event {
                Ok(event) => {
                    let label = if event.agent_name == MAIN {
                        None
                    } else {
                        printer.workers.get(&event.agent_run_id).cloned().or_else(||
                            workflow.as_ref().and_then(std::sync::Weak::upgrade)
                                .and_then(|workflow| workflow.subagents().into_iter()
                                    .find(|run| run.run_id == event.agent_run_id)
                                    .map(|run| run.handle)))
                    };
                    if let Some(label) = &label {
                        printer.workers.insert(event.agent_run_id, label.clone());
                    }
                    if let Some(feed) = &feed {
                        feed.event(&event, label.as_deref());
                    }
                    printer.event(event);
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    printer.output.echo(&printer.output.dim(&format!("  ({missed} events not shown)")));
                    if let Some(feed) = &feed {
                        feed.missed(missed);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            request = control.recv() => match request {
                Some(Control::Flush(done)) => {
                    printer.flush();
                    let _ = done.send(());
                }
                None => break,
            },
        }
    }
    printer.flush();
}

struct EventPrinter {
    output: Arc<Output>,
    stats: Arc<Mutex<Stats>>,
    show_thinking: bool,
    /// The main agent's text since its last complete line, or with
    /// `hold_text` since its last call.
    text: String,
    /// The main agent ends its turns with a reply in plain text, which the
    /// harness prints as the reply: its text is held, and printed as
    /// progress once a call follows it (or the turn ends otherwise), or
    /// dropped when the turn ends with it as the reply.
    hold_text: bool,
    thinking: String,
    /// Labels of the workers' runs, by run ID.
    workers: HashMap<Uuid, String>,
    /// The label of every run that isn't the main agent's.
    label: Option<String>,
    /// Shell commands the main agent is running, by call ID.
    commands: HashMap<Uuid, String>,
}

impl EventPrinter {
    fn new(
        output: Arc<Output>,
        stats: Arc<Mutex<Stats>>,
        show_thinking: bool,
        label: Option<String>,
    ) -> Self {
        Self {
            output,
            stats,
            show_thinking,
            text: String::new(),
            hold_text: false,
            thinking: String::new(),
            workers: HashMap::new(),
            label,
            commands: HashMap::new(),
        }
    }

    fn worker(&mut self, run: Uuid) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }
        let count = self.workers.len() + 1;
        self.workers
            .entry(run)
            .or_insert_with(|| format!("worker {count}"))
            .clone()
    }

    /// Prints the complete lines of the buffered text (the main agent's
    /// text only when it isn't held).
    fn print_lines(&mut self) {
        while let Some(end) = self.text.find('\n').filter(|_| !self.hold_text) {
            let line: String = self.text.drain(..=end).collect();
            self.output.echo(line.trim_end_matches(['\n', '\r']));
        }
        while let Some(end) = self.thinking.find('\n') {
            let line: String = self.thinking.drain(..=end).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            self.output.echo(&self.output.dim(&format!("  {line}")));
        }
    }

    fn flush(&mut self) {
        self.print_lines();
        if !self.thinking.trim().is_empty() {
            let rest = std::mem::take(&mut self.thinking);
            self.output
                .echo(&self.output.dim(&format!("  {}", rest.trim_end())));
        }
        self.thinking.clear();
        if !self.text.trim().is_empty() {
            let rest = std::mem::take(&mut self.text);
            self.output.echo(rest.trim_end());
        }
        self.text.clear();
    }

    fn event(&mut self, event: WorkflowEvent) {
        let main = event.agent_name == MAIN;
        match event.event {
            AgentEvent::AssistantResponseChunk(text) if main => {
                self.flush_thinking();
                self.text.push_str(&text);
                self.print_lines();
            }
            AgentEvent::ReasoningChunk(text) if main && self.show_thinking => {
                self.thinking.push_str(&text);
                self.print_lines();
            }
            AgentEvent::ToolCallFormatted {
                tool_call_id,
                tool_name,
                parameters,
            } => {
                if main {
                    self.flush();
                    if let Some(line) = self.call_line(&tool_name, &parameters) {
                        self.output.echo(&line);
                    }
                    if tool_name == SHELL_TOOL_NAME {
                        self.commands.insert(tool_call_id, tool_name);
                    }
                } else if tool_name != "complete_task" {
                    let worker = self.worker(event.agent_run_id);
                    let call = brief(&tool_name, &parameters);
                    self.output
                        .echo(&self.output.dim(&format!("  ↳ {worker}: {call}")));
                }
            }
            AgentEvent::ToolResult {
                tool_call_id,
                tool_name,
                output,
            } if main => {
                if self.commands.remove(&tool_call_id).is_some() {
                    self.shell_result(&output);
                } else if crew::LEAD_TOOL_NAMES.contains(&tool_name.as_str())
                    && tool_name != crew::START_AGENT
                {
                    // (A started worker is announced by the crew.)
                    let first = output.lines().next().unwrap_or_default();
                    self.output
                        .echo(&self.output.dim(&format!("  ⎿ {}", cut(first, 160))));
                } else if tool_name == RUN_WORKFLOW_AGENT_TOOL_NAME {
                    let lines = output.lines().count();
                    self.output.echo(&self.output.dim(&format!(
                        "  ⎿ the worker reported ({lines} line{})",
                        if lines == 1 { "" } else { "s" }
                    )));
                }
            }
            AgentEvent::ErrorEncountered {
                error_message,
                tool_call_id,
                ..
            } => {
                if main {
                    self.flush();
                }
                self.commands.retain(|id, _| Some(*id) != tool_call_id);
                let who = if main {
                    String::new()
                } else {
                    format!("{}: ", self.worker(event.agent_run_id))
                };
                let text = cut(&error_message, 400);
                let line = if tool_call_id.is_some() {
                    format!("  ✗ {who}{text}")
                } else {
                    format!("  ! {who}{text}")
                };
                self.output.echo(&self.output.error(&line));
            }
            AgentEvent::RetryScheduled {
                attempt,
                delay_ms,
                rate_limit,
                ..
            } => {
                let why = if rate_limit { "rate limited, " } else { "" };
                self.output.echo(&self.output.warn(&format!(
                    "  … {why}retrying in {:.0} s (attempt {attempt})",
                    delay_ms as f64 / 1000.0
                )));
            }
            AgentEvent::UsageLimitReached { error_message, .. } => {
                self.output.echo(&self.output.warn(&format!(
                    "  … usage limit reached: {}",
                    cut(&error_message, 300)
                )));
            }
            AgentEvent::OutputTokenLimitReached { output_tokens, .. } if main => {
                // Held text may be the reply, which the turn's end shows.
                if self.hold_text {
                    self.flush_thinking();
                } else {
                    self.flush();
                }
                self.output.echo(&self.output.warn(&format!(
                    "  ! the reply reached the model's output limit ({output_tokens} tokens)"
                )));
            }
            AgentEvent::HandoverRequested { input_tokens } if main => {
                self.flush();
                self.output.echo(&self.output.dim(&format!(
                    "  ⟳ {}",
                    crate::handover::requested_text(event.agent_run_id, input_tokens)
                )));
            }
            AgentEvent::ConversationHandedOver { .. } if main => {
                // The report's last line has no newline after it.
                self.flush();
                self.output.echo(&self.output.dim(
                    "  ⟳ handed over: the agent continues in a fresh conversation from its report",
                ));
            }
            AgentEvent::ConversationCompacted {
                input_tokens,
                compacted_tokens,
            } if main => {
                self.flush();
                self.output.echo(&self.output.dim(&format!(
                    "  ⟳ {}",
                    crate::handover::compacted_text(input_tokens, compacted_tokens)
                )));
            }
            AgentEvent::RefusalReceived(refusal) => {
                self.output.echo(
                    &self
                        .output
                        .warn(&format!("  ! the model refused ({:?})", refusal.category)),
                );
            }
            // The reply, held: the harness prints it as the reply.
            AgentEvent::TaskStateChanged(TaskState::Completed { summary })
                if main && self.hold_text && self.text.trim() == summary.trim() =>
            {
                self.text.clear();
            }
            AgentEvent::ModelCallUsageReported(usage) if main => {
                self.stats.lock().unwrap().context_tokens = Some(usage.input_tokens);
            }
            AgentEvent::PlanUsageReported(usage) => {
                if let Some(text) = crate::providers::plan_usage_text(&usage) {
                    let mut stats = self.stats.lock().unwrap();
                    let plan = crate::providers::plan_name(usage.plan);
                    stats.plans.insert(plan.to_string(), text);
                    stats.plan_usage = crate::providers::plan_usage_line(&stats.plans);
                }
            }
            _ => {}
        }
    }

    fn flush_thinking(&mut self) {
        if !self.thinking.trim().is_empty() {
            let rest = std::mem::take(&mut self.thinking);
            for line in rest.trim_end().lines() {
                self.output.echo(&self.output.dim(&format!("  {line}")));
            }
        }
        self.thinking.clear();
    }

    /// How a call of the main agent is shown; `None` for calls not shown.
    fn call_line(
        &self,
        tool: &str,
        parameters: &HashMap<String, serde_json::Value>,
    ) -> Option<String> {
        let text = |name: &str| {
            parameters
                .get(name)
                .and_then(|value| value.as_str())
                .unwrap_or_default()
        };
        let bullet = self.output.accent("●");
        let worker = || self.output.bold(text("to").trim());
        Some(match tool {
            // The question is shown as it is asked.
            "complete_task" | ASK_USER_TOOL_NAME => return None,
            // The report is shown whole: the agent continues from it alone.
            HANDOVER_REPORT_TOOL_NAME => format!(
                "{} {}\n  {}",
                self.output.accent("⟳"),
                self.output.bold("handover report"),
                text(HANDOVER_REPORT_PARAM_NAME)
                    .trim()
                    .replace('\n', "\n  ")
            ),
            message::MESSAGE_USER => {
                let message = text(message::MESSAGE_PARAM).trim();
                format!(
                    "{} {}",
                    self.output.accent("✉"),
                    message.replace('\n', "\n  ")
                )
            }
            crew::START_AGENT => {
                let mut notes = Vec::new();
                if flag(parameters, "isolated") {
                    notes.push("worktree".to_string());
                }
                let cache = text(crew::CACHE_PARAM).trim();
                if !cache.is_empty() {
                    notes.push(format!("{cache} cache"));
                }
                format!(
                    "{bullet} {}{}: {}",
                    self.output.bold("start worker"),
                    if notes.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", notes.join(", "))
                    },
                    cut(text("task"), 160)
                )
            }
            crew::MESSAGE_AGENT => format!(
                "{bullet} {} {}: {}",
                self.output.bold("message"),
                worker(),
                cut(text("message"), 160)
            ),
            crew::WAIT_FOR_AGENTS => {
                let which = text("runs").trim();
                format!(
                    "{bullet} {}",
                    self.output.bold(&if which.is_empty() {
                        "waiting for the workers".to_string()
                    } else {
                        format!("waiting for {which}")
                    })
                )
            }
            crew::LIST_AGENTS => format!("{bullet} {}", self.output.bold("list workers")),
            crew::STOP_AGENT => format!(
                "{bullet} {} {}{}",
                self.output.bold("stop"),
                self.output.bold(text("run")),
                if flag(parameters, "remove_workspace") {
                    " and remove its worktree"
                } else {
                    ""
                }
            ),
            READ_FILE_TOOL_NAME | WRITE_FILE_TOOL_NAME | APPLY_DIFF_TOOL_NAME
            | LIST_FILES_TOOL_NAME => {
                let (verb, detail) = file_call(tool, parameters);
                format!("{bullet} {} {detail}", self.output.bold(verb))
            }
            SHELL_TOOL_NAME => {
                let cwd = text("cwd");
                let cwd = cwd.trim();
                let place = if cwd.is_empty() || cwd == "." {
                    String::new()
                } else {
                    self.output.dim(&format!("(in {cwd}) "))
                };
                format!(
                    "{bullet} {place}{}",
                    self.output.bold(&command_line(text("command")))
                )
            }
            RUN_WORKFLOW_AGENT_TOOL_NAME => {
                let resumed = if text("resume_run").is_empty() {
                    ""
                } else {
                    " (resumed)"
                };
                format!(
                    "{bullet} {}{resumed}: {}",
                    self.output.bold("worker"),
                    cut(text("prompt"), 160)
                )
            }
            "terminal_start" => format!(
                "{bullet} {} {}",
                self.output.bold("background:"),
                command_line(text("command"))
            ),
            "terminal_control" => format!("{bullet} terminal {}", text("action")),
            "terminal_input" => format!("{bullet} terminal input"),
            "terminal_status" => format!("{bullet} terminal status"),
            LOAD_SKILL_TOOL_NAME => {
                format!("{bullet} {} {}", self.output.bold("skill"), text("name"))
            }
            tool if index::TOOL_NAMES.contains(&tool) || mcp::split_tool_name(tool).is_some() => {
                let (name, detail) = labelled(tool, parameters);
                format!("{bullet} {}{}", self.output.bold(&name), detail)
            }
            other => format!("{bullet} {other}"),
        })
    }

    /// A shell command's result: its status and the first lines of output.
    fn shell_result(&self, output: &str) {
        const SHOWN: usize = 4;
        let mut lines = output.lines();
        let status = lines.next().unwrap_or_default();
        let rest: Vec<&str> = lines.collect();
        let failed = !status.starts_with("exit code 0,");
        let mut text = Vec::new();
        if !(rest.len() == 1 && rest[0] == "(no output)") {
            for line in rest.iter().take(SHOWN) {
                text.push(format!("  │ {}", cut(line, 160)));
            }
            if rest.len() > SHOWN {
                text.push(format!("  │ … {} more lines", rest.len() - SHOWN));
            }
        }
        for line in text {
            self.output.echo(&self.output.dim(&line));
        }
        let status = format!("  ⎿ {status}");
        self.output.echo(&if failed {
            self.output.warn(&status)
        } else {
            self.output.dim(&status)
        });
    }
}

/// A command's first line, with a count of the others.
fn command_line(command: &str) -> String {
    let command = command.trim();
    let mut lines = command.lines();
    let first = cut(lines.next().unwrap_or_default(), 200);
    match lines.count() {
        0 => format!("$ {first}"),
        more => format!("$ {first}  (+{more} lines)"),
    }
}

/// A worker's call, briefly.
pub fn brief(tool: &str, parameters: &HashMap<String, serde_json::Value>) -> String {
    let text = |name: &str| {
        parameters
            .get(name)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
    };
    match tool {
        SHELL_TOOL_NAME => command_line(text("command")),
        READ_FILE_TOOL_NAME | WRITE_FILE_TOOL_NAME | APPLY_DIFF_TOOL_NAME
        | LIST_FILES_TOOL_NAME => {
            let (verb, detail) = file_call(tool, parameters);
            format!("{verb} {detail}")
        }
        crew::MESSAGE_AGENT => format!("message to {}", text("to")),
        HANDOVER_REPORT_TOOL_NAME => "handover report".to_string(),
        crew::WAIT_FOR_AGENTS => "waiting for messages".to_string(),
        "terminal_start" => format!("background {}", command_line(text("command"))),
        tool if TerminalTool::NAMES.contains(&tool) => tool.replace('_', " "),
        LOAD_SKILL_TOOL_NAME => format!("skill {}", text("name")),
        tool if index::TOOL_NAMES.contains(&tool) || mcp::split_tool_name(tool).is_some() => {
            let (name, detail) = labelled(tool, parameters);
            format!("{name}{detail}")
        }
        other => other.to_string(),
    }
}

/// A boolean parameter: `true`, or the text `true`.
fn flag(parameters: &HashMap<String, serde_json::Value>, name: &str) -> bool {
    match parameters.get(name) {
        Some(serde_json::Value::Bool(value)) => *value,
        Some(serde_json::Value::String(text)) => text.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// A file tool's call: its verb (`read`, `write`, `edit`, `list`) and what
/// it reads or changes (`src/main.rs:10-40`, `src/new.rs (12 lines)`).
fn file_call(
    tool: &str,
    parameters: &HashMap<String, serde_json::Value>,
) -> (&'static str, String) {
    let text = |name: &str| match parameters.get(name) {
        Some(serde_json::Value::String(text)) => text.trim().to_string(),
        Some(serde_json::Value::Number(number)) => number.to_string(),
        _ => String::new(),
    };
    let path = cut(&text("path"), 160);
    match tool {
        READ_FILE_TOOL_NAME => {
            let range = match (text("start_line").as_str(), text("end_line").as_str()) {
                ("", "") => String::new(),
                (start, "") => format!(":{start}-"),
                ("", end) => format!(":1-{end}"),
                (start, end) => format!(":{start}-{end}"),
            };
            ("read", format!("{path}{range}"))
        }
        WRITE_FILE_TOOL_NAME => {
            let lines = parameters
                .get("content")
                .and_then(|value| value.as_str())
                .map_or(0, |content| content.lines().count());
            (
                "write",
                format!("{path} ({lines} line{})", if lines == 1 { "" } else { "s" }),
            )
        }
        APPLY_DIFF_TOOL_NAME => {
            let blocks = parameters
                .get("diff")
                .and_then(|value| value.as_str())
                .map_or(0, |diff| diff.matches("<<<<<<< SEARCH").count());
            (
                "edit",
                if blocks > 1 {
                    format!("{path} ({blocks} changes)")
                } else {
                    path
                },
            )
        }
        _ => {
            let directory = cut(&text("directory"), 160);
            (
                "list",
                if directory.is_empty() {
                    ".".into()
                } else {
                    directory
                },
            )
        }
    }
}

/// A search index or MCP tool's call: its name as shown (`index search:`,
/// `github: create_issue`), and what it is called with, briefly.
fn labelled(tool: &str, parameters: &HashMap<String, serde_json::Value>) -> (String, String) {
    let text = |name: &str| {
        parameters
            .get(name)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let with = |detail: String| {
        if detail.is_empty() {
            String::new()
        } else {
            format!(" {detail}")
        }
    };
    match tool {
        "index_search" => ("index search:".into(), with(cut(&text("query"), 160))),
        "index_status" => ("index status".into(), with(text("index"))),
        "index_create" => (
            "index create:".into(),
            with(match text("path").as_str() {
                "" => ".".into(),
                path => cut(path, 160),
            }),
        ),
        "index_manage" => (
            format!("index {}", text("action")),
            with(cut(&text("index"), 160)),
        ),
        _ => {
            let (server, name) = mcp::split_tool_name(tool).unwrap_or(("mcp", tool));
            let arguments = if parameters.is_empty() {
                String::new()
            } else {
                // In key order, so a call always shows the same way.
                let sorted: std::collections::BTreeMap<_, _> = parameters.iter().collect();
                cut(&serde_json::to_string(&sorted).unwrap_or_default(), 120)
            };
            (format!("{server}: {name}"), with(arguments))
        }
    }
}

/// `text` on one line, at most `limit` characters.
pub fn cut(text: &str, limit: usize) -> String {
    let single = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() <= limit {
        single
    } else {
        format!("{}…", single.chars().take(limit).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continued_lines_are_joined_with_their_line_breaks() {
        assert_eq!(joined("first\\\nsecond"), "first\nsecond");
        assert_eq!(joined("one line"), "one line");
    }

    #[test]
    fn index_and_mcp_calls_are_shown_briefly() {
        let params = |pairs: &[(&str, serde_json::Value)]| -> HashMap<String, serde_json::Value> {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect()
        };
        assert_eq!(
            brief(
                "index_search",
                &params(&[("query", "where are retries handled?".into())])
            ),
            "index search: where are retries handled?"
        );
        assert_eq!(brief("index_create", &params(&[])), "index create: .");
        assert_eq!(
            brief(
                "index_manage",
                &params(&[("action", "rebuild".into()), ("index", "finn".into())])
            ),
            "index rebuild finn"
        );
        assert_eq!(
            brief("load_skill", &params(&[("name", "review".into())])),
            "skill review"
        );
        assert_eq!(
            brief(
                "mcp__github__create_issue",
                &params(&[
                    ("title", "Bug".into()),
                    ("labels", serde_json::json!(["a"]))
                ])
            ),
            r#"github: create_issue {"labels":["a"],"title":"Bug"}"#
        );
        assert_eq!(brief("mcp__db__ping", &params(&[])), "db: ping");
    }

    #[test]
    fn file_and_worker_calls_are_shown_briefly() {
        let params = |pairs: &[(&str, serde_json::Value)]| -> HashMap<String, serde_json::Value> {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect()
        };
        assert_eq!(
            brief(
                READ_FILE_TOOL_NAME,
                &params(&[
                    ("path", "src/main.rs".into()),
                    ("start_line", 10.into()),
                    ("end_line", "40".into())
                ])
            ),
            "read src/main.rs:10-40"
        );
        assert_eq!(
            brief(READ_FILE_TOOL_NAME, &params(&[("path", "a.rs".into())])),
            "read a.rs"
        );
        assert_eq!(
            brief(
                WRITE_FILE_TOOL_NAME,
                &params(&[("path", "a.rs".into()), ("content", "a\nb\n".into())])
            ),
            "write a.rs (2 lines)"
        );
        let block = "<<<<<<< SEARCH\n:start_line:1\n-------\na\n=======\nb\n>>>>>>> REPLACE\n";
        assert_eq!(
            brief(
                APPLY_DIFF_TOOL_NAME,
                &params(&[("path", "a.rs".into()), ("diff", block.into())])
            ),
            "edit a.rs"
        );
        assert_eq!(
            brief(
                APPLY_DIFF_TOOL_NAME,
                &params(&[("path", "a.rs".into()), ("diff", block.repeat(2).into())])
            ),
            "edit a.rs (2 changes)"
        );
        assert_eq!(brief(LIST_FILES_TOOL_NAME, &params(&[])), "list .");
        assert_eq!(
            brief(
                crew::MESSAGE_AGENT,
                &params(&[("to", "lead".into()), ("message", "hi".into())])
            ),
            "message to lead"
        );

        let printer = EventPrinter::new(
            Arc::new(Output::new(None, true)),
            Arc::default(),
            false,
            None,
        );
        let line = |tool: &str, pairs: &[(&str, serde_json::Value)]| {
            printer
                .call_line(tool, &params(pairs))
                .map(|line| strip_styles(&line))
        };
        assert_eq!(
            line(
                crew::START_AGENT,
                &[
                    ("task", "Fix the parser.".into()),
                    ("isolated", true.into())
                ]
            )
            .unwrap(),
            "● start worker (worktree): Fix the parser."
        );
        assert_eq!(
            line(
                crew::MESSAGE_AGENT,
                &[("to", "w2".into()), ("message", "Use plan B.".into())]
            )
            .unwrap(),
            "● message w2: Use plan B."
        );
        assert_eq!(
            line(crew::WAIT_FOR_AGENTS, &[]).unwrap(),
            "● waiting for the workers"
        );
        assert_eq!(
            line(
                crew::STOP_AGENT,
                &[("run", "r1".into()), ("remove_workspace", "true".into())]
            )
            .unwrap(),
            "● stop r1 and remove its worktree"
        );
        assert_eq!(
            line(
                WRITE_FILE_TOOL_NAME,
                &[("path", "b.rs".into()), ("content", "x".into())]
            )
            .unwrap(),
            "● write b.rs (1 line)"
        );
        // The question is shown as it is asked.
        assert_eq!(
            line(ASK_USER_TOOL_NAME, &[("questions", "Which?".into())]),
            None
        );
        assert_eq!(
            line(
                crew::START_AGENT,
                &[("task", "Look it up.".into()), ("cache", "5m".into())]
            )
            .unwrap(),
            "● start worker (5m cache): Look it up."
        );
        // The agent's message to the user is shown whole.
        assert_eq!(
            line(
                message::MESSAGE_USER,
                &[("message", "Halfway there.\nTests next.".into())]
            )
            .unwrap(),
            "✉ Halfway there.\n  Tests next."
        );
        // So is a handover report, which a worker's line only names.
        assert_eq!(
            line(
                HANDOVER_REPORT_TOOL_NAME,
                &[("report", "## Done\nThe parser.\n".into())]
            )
            .unwrap(),
            "⟳ handover report\n  ## Done\n  The parser."
        );
        assert_eq!(
            brief(
                HANDOVER_REPORT_TOOL_NAME,
                &params(&[("report", "x".into())])
            ),
            "handover report"
        );
    }

    /// `text` without its terminal styles.
    fn strip_styles(text: &str) -> String {
        let mut plain = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                plain.push(c);
            }
        }
        plain
    }

    #[test]
    fn commands_show_their_first_line() {
        assert_eq!(command_line("cargo test"), "$ cargo test");
        assert_eq!(
            command_line("cat > a <<'EOF'\nx\nEOF"),
            "$ cat > a <<'EOF'  (+2 lines)"
        );
    }

    #[tokio::test]
    async fn text_is_printed_by_whole_lines_and_flushed_at_the_end() {
        let output = Arc::new(Output::new(None, false));
        let stats = Arc::new(Mutex::new(Stats::default()));
        let mut printer = EventPrinter::new(output, stats.clone(), false, None);
        let event = |agent: &str, event: AgentEvent| WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: agent.into(),
            agent_run_id: Uuid::nil(),
            event,
        };
        printer.event(event(
            MAIN,
            AgentEvent::AssistantResponseChunk("Hel".into()),
        ));
        assert_eq!(printer.text, "Hel");
        printer.event(event(
            MAIN,
            AgentEvent::AssistantResponseChunk("lo\nWor".into()),
        ));
        assert_eq!(printer.text, "Wor");
        // A worker's text isn't shown.
        printer.event(event(
            "worker",
            AgentEvent::AssistantResponseChunk("x\n".into()),
        ));
        assert_eq!(printer.text, "Wor");
        printer.flush();
        assert!(printer.text.is_empty());
    }

    /// A handover report's last line, which has no newline after it, is
    /// printed before the handover is announced.
    #[tokio::test]
    async fn the_handover_report_is_printed_to_its_last_line() {
        let output = Arc::new(Output::new(None, true));
        let mut printer = EventPrinter::new(output, Arc::default(), false, None);
        let event = |event: AgentEvent| WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: MAIN.into(),
            agent_run_id: Uuid::nil(),
            event,
        };
        printer.event(event(AgentEvent::AssistantResponseChunk(
            "## Waiting on\nNothing from the user.".into(),
        )));
        assert_eq!(printer.text, "Nothing from the user.");
        printer.event(event(AgentEvent::ConversationHandedOver {
            report: "## Waiting on\nNothing from the user.".into(),
            input_tokens: 500_000,
            handovers: 1,
        }));
        assert!(printer.text.is_empty(), "{:?}", printer.text);
    }

    /// The lines an output printed.
    #[derive(Clone, Default)]
    struct Printed(Arc<Mutex<Vec<String>>>);

    impl ExternalPrinter for Printed {
        fn print(&mut self, text: String) -> rustyline::Result<()> {
            self.0
                .lock()
                .unwrap()
                .extend(text.lines().map(str::to_string));
            Ok(())
        }
    }

    /// With text replies, the main agent's text is held: printed as
    /// progress once a call follows it, and left to the harness, which
    /// prints the reply, when the turn ends with it.
    #[tokio::test]
    async fn text_replies_hold_the_text_until_a_call_or_the_reply() {
        let printed = Printed::default();
        let output = Arc::new(Output::new(Some(Box::new(printed.clone())), false));
        let mut printer = EventPrinter::new(output, Arc::default(), false, None);
        printer.hold_text = true;
        let event = |event: AgentEvent| WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: MAIN.into(),
            agent_run_id: Uuid::nil(),
            event,
        };
        printer.event(event(AgentEvent::AssistantResponseChunk(
            "Looking at\nthe files.".into(),
        )));
        assert!(printed.0.lock().unwrap().is_empty(), "held");
        printer.event(event(AgentEvent::ToolCallFormatted {
            tool_call_id: Uuid::new_v4(),
            tool_name: "list_files".into(),
            parameters: [("path".to_string(), serde_json::json!("."))].into(),
        }));
        let lines = printed.0.lock().unwrap().clone();
        assert_eq!(&lines[..2], ["Looking at", "the files."]);

        printer.event(event(AgentEvent::AssistantResponseChunk(
            "There are\n3 files.\n".into(),
        )));
        printer.event(event(AgentEvent::OutputTokenLimitReached {
            model: "m".into(),
            output_tokens: 10,
        }));
        printer.event(event(AgentEvent::TaskStateChanged(TaskState::Completed {
            summary: "There are\n3 files.".into(),
        })));
        printer.flush();
        let lines = printed.0.lock().unwrap().clone();
        assert!(
            !lines.iter().any(|line| line.contains("3 files.")),
            "the reply is the harness's to print: {lines:?}"
        );
    }
}

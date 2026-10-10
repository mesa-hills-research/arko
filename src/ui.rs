//! The terminal: a prompt that stays at the bottom while the agents' work
//! prints above it, and how their events are shown.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use arko_agents::AgentEvent;
use arko_agents::terminal::TerminalTool;
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
use crate::shell::SHELL_TOOL_NAME;

pub const PROMPT: &str = "› ";

/// What the user did at the prompt.
#[derive(Debug)]
pub enum Input {
    Line(String),
    /// Ctrl-C.
    Interrupt,
    /// Ctrl-D, or the end of standard input.
    Eof,
}

/// Where everything is printed: above the prompt when there is one, else
/// to standard output (and, in one-shot mode, progress to standard error).
pub struct Output {
    printer: Mutex<Option<Box<dyn ExternalPrinter + Send>>>,
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
            color,
            progress_to_stderr,
        }
    }

    /// Prints straight to the terminal from now on, not above the prompt.
    pub fn detach(&self) {
        self.printer.lock().unwrap().take();
    }

    /// Clears the line the cursor is on (the prompt's).
    pub fn clear_line(&self) {
        if self.color {
            let mut stdout = std::io::stdout().lock();
            let _ = write!(stdout, "\r\x1b[2K");
            let _ = stdout.flush();
        }
    }

    /// Prints `text` (one or more lines) as progress.
    pub fn line(&self, text: &str) {
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

    /// Prints the agent's reply: to standard output in one-shot mode.
    pub fn answer(&self, text: &str) {
        if self.progress_to_stderr {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{text}");
            let _ = stdout.flush();
        } else {
            self.line(text);
        }
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
pub fn spawn_input(
    history: PathBuf,
    sender: mpsc::UnboundedSender<Input>,
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
            while !CLOSING.load(Ordering::SeqCst) {
                let input = match editor.readline(PROMPT) {
                    Ok(line) => Input::Line(joined(&line)),
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
    /// The ChatGPT plan's usage windows, as last reported.
    pub plan_usage: Option<String>,
}

/// A request to the event printer.
pub enum Control {
    /// Print what is buffered, then answer.
    Flush(oneshot::Sender<()>),
}

/// Prints a workflow's events until its channel closes.
pub async fn print_events(
    mut events: broadcast::Receiver<WorkflowEvent>,
    mut control: mpsc::UnboundedReceiver<Control>,
    output: Arc<Output>,
    stats: Arc<Mutex<Stats>>,
    show_thinking: bool,
) {
    let mut printer = EventPrinter::new(output, stats, show_thinking);
    loop {
        tokio::select! {
            biased;
            event = events.recv() => match event {
                Ok(event) => printer.event(event),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    printer.output.line(&printer.output.dim(&format!("  ({missed} events not shown)")));
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
    /// The main agent's text since its last complete line.
    text: String,
    thinking: String,
    /// Labels of the workers' runs, by run ID.
    workers: HashMap<Uuid, String>,
    /// Shell commands the main agent is running, by call ID.
    commands: HashMap<Uuid, String>,
}

impl EventPrinter {
    fn new(output: Arc<Output>, stats: Arc<Mutex<Stats>>, show_thinking: bool) -> Self {
        Self {
            output,
            stats,
            show_thinking,
            text: String::new(),
            thinking: String::new(),
            workers: HashMap::new(),
            commands: HashMap::new(),
        }
    }

    fn worker(&mut self, run: Uuid) -> String {
        let count = self.workers.len() + 1;
        self.workers
            .entry(run)
            .or_insert_with(|| format!("worker {count}"))
            .clone()
    }

    /// Prints the complete lines of the buffered text.
    fn print_lines(&mut self) {
        while let Some(end) = self.text.find('\n') {
            let line: String = self.text.drain(..=end).collect();
            self.output.line(line.trim_end_matches(['\n', '\r']));
        }
        while let Some(end) = self.thinking.find('\n') {
            let line: String = self.thinking.drain(..=end).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            self.output.line(&self.output.dim(&format!("  {line}")));
        }
    }

    fn flush(&mut self) {
        self.print_lines();
        if !self.thinking.trim().is_empty() {
            let rest = std::mem::take(&mut self.thinking);
            self.output
                .line(&self.output.dim(&format!("  {}", rest.trim_end())));
        }
        self.thinking.clear();
        if !self.text.trim().is_empty() {
            let rest = std::mem::take(&mut self.text);
            self.output.line(rest.trim_end());
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
                        self.output.line(&line);
                    }
                    if tool_name == SHELL_TOOL_NAME {
                        self.commands.insert(tool_call_id, tool_name);
                    }
                } else if tool_name != "complete_task" {
                    let worker = self.worker(event.agent_run_id);
                    let call = brief(&tool_name, &parameters);
                    self.output
                        .line(&self.output.dim(&format!("  ↳ {worker}: {call}")));
                }
            }
            AgentEvent::ToolResult {
                tool_call_id,
                tool_name,
                output,
            } if main => {
                if self.commands.remove(&tool_call_id).is_some() {
                    self.shell_result(&output);
                } else if tool_name == RUN_WORKFLOW_AGENT_TOOL_NAME {
                    let lines = output.lines().count();
                    self.output.line(&self.output.dim(&format!(
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
                self.output.line(&self.output.error(&line));
            }
            AgentEvent::RetryScheduled {
                attempt,
                delay_ms,
                rate_limit,
                ..
            } => {
                let why = if rate_limit { "rate limited; " } else { "" };
                self.output.line(&self.output.warn(&format!(
                    "  … {why}retrying in {:.0} s (attempt {attempt})",
                    delay_ms as f64 / 1000.0
                )));
            }
            AgentEvent::UsageLimitReached { error_message, .. } => {
                self.output.line(&self.output.warn(&format!(
                    "  … usage limit reached: {}",
                    cut(&error_message, 300)
                )));
            }
            AgentEvent::OutputTokenLimitReached { output_tokens, .. } if main => {
                self.flush();
                self.output.line(&self.output.warn(&format!(
                    "  ! the reply reached the model's output limit ({output_tokens} tokens)"
                )));
            }
            AgentEvent::HandoverRequested { input_tokens } if main => {
                self.flush();
                self.output.line(&self.output.dim(&format!(
                    "  ⟳ the conversation reached {}k tokens; the agent is writing a handover report",
                    input_tokens / 1000
                )));
            }
            AgentEvent::ConversationHandedOver { .. } if main => {
                self.output.line(&self.output.dim(
                    "  ⟳ handed over: the agent continues in a fresh conversation from its report",
                ));
            }
            AgentEvent::RefusalReceived(refusal) => {
                self.output.line(
                    &self
                        .output
                        .warn(&format!("  ! the model refused ({:?})", refusal.category)),
                );
            }
            AgentEvent::ModelCallUsageReported(usage) if main => {
                self.stats.lock().unwrap().context_tokens = Some(usage.input_tokens);
            }
            AgentEvent::PlanUsageReported(usage) => {
                let windows = [(&usage.primary, "5 h"), (&usage.secondary, "7 d")]
                    .into_iter()
                    .filter_map(|(window, name)| {
                        let window = window.as_ref()?;
                        let name = match window.window_minutes {
                            Some(300) => "5 h".to_string(),
                            Some(10_080) => "7 d".to_string(),
                            Some(minutes) => format!("{minutes} min"),
                            None => name.to_string(),
                        };
                        Some(format!("{name} {:.0}%", window.used_percent))
                    })
                    .collect::<Vec<_>>();
                if !windows.is_empty() {
                    self.stats.lock().unwrap().plan_usage =
                        Some(format!("ChatGPT plan {}", windows.join(", ")));
                }
            }
            _ => {}
        }
    }

    fn flush_thinking(&mut self) {
        if !self.thinking.trim().is_empty() {
            let rest = std::mem::take(&mut self.thinking);
            for line in rest.trim_end().lines() {
                self.output.line(&self.output.dim(&format!("  {line}")));
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
        Some(match tool {
            "complete_task" => return None,
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
            self.output.line(&self.output.dim(&line));
        }
        let status = format!("  ⎿ {status}");
        self.output.line(&if failed {
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
fn brief(tool: &str, parameters: &HashMap<String, serde_json::Value>) -> String {
    let text = |name: &str| {
        parameters
            .get(name)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
    };
    match tool {
        SHELL_TOOL_NAME => command_line(text("command")),
        "terminal_start" => format!("background {}", command_line(text("command"))),
        tool if TerminalTool::NAMES.contains(&tool) => tool.replace('_', " "),
        other => other.to_string(),
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
        let mut printer = EventPrinter::new(output, stats.clone(), false);
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
}

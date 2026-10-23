//! The terminal: a prompt that stays at the bottom while the agents' work
//! prints above it, and how their events are shown.

use std::collections::HashMap;
#[cfg(feature = "tui")]
use std::io::BufRead;
use std::io::{IsTerminal, Write};
#[cfg(feature = "tui")]
use std::path::PathBuf;
#[cfg(feature = "tui")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use arko_agents::builtins::{
    APPLY_DIFF_TOOL_NAME, ASK_USER_TOOL_NAME, EDIT_FILE_TOOL_NAME, LIST_FILES_TOOL_NAME,
    READ_FILE_TOOL_NAME, WRITE_FILE_TOOL_NAME,
};
use arko_agents::handover::{HANDOVER_REPORT_PARAM_NAME, HANDOVER_REPORT_TOOL_NAME};
use arko_agents::terminal::TerminalTool;
use arko_agents::{AgentEvent, LOAD_SKILL_TOOL_NAME, TaskState};
use arko_workflows::WorkflowEvent;
use arko_workflows::services::RUN_WORKFLOW_AGENT_TOOL_NAME;
#[cfg(feature = "tui")]
use rustyline::completion::Completer;
#[cfg(feature = "tui")]
use rustyline::error::ReadlineError;
#[cfg(feature = "tui")]
use rustyline::highlight::Highlighter;
#[cfg(feature = "tui")]
use rustyline::hint::Hinter;
#[cfg(feature = "tui")]
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
#[cfg(feature = "tui")]
use rustyline::{ExternalPrinter, Helper};
use tokio::sync::{mpsc, oneshot};
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
    /// The end of standard input, or of the terminal, and Arko quits.
    Eof,
    /// The full-screen view quits (Ctrl+D, Ctrl+C twice, /exit). `Some`
    /// lists the background jobs the user agreed to stop in the quit popup,
    /// and `None` means the user turned asking off.
    Quit(Option<Vec<uuid::Uuid>>),
    /// Ctrl+B in the full-screen view. The calls this agent runs that can
    /// move (the main agent's, or a worker's by its ID) move to the
    /// background.
    Background(String),
    /// A message from an app. It starts a turn when the agent is idle, is a
    /// note while it works, and runs as a command when it is one. Unlike a
    /// line, it is whole and isn't joined with what comes right after it.
    Message(Message),
    /// The answer to the agent's question `question` (the ID the feed gave
    /// it).
    Answer {
        question: u64,
        text: String,
    },
}

/// A message from an app, with what is attached to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub text: String,
    /// Images to send with it. The message isn't sent when the main agent's
    /// model takes no images or not their type
    /// ([`image_refusal`](crate::models::image_refusal)).
    pub images: Vec<Attachment>,
}

/// A file attached to a message, with its bytes, its type and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub bytes: std::sync::Arc<[u8]>,
    /// Its MIME type, such as `image/png`.
    pub mime: String,
    pub name: Option<String>,
}

/// What quitting would stop (the background jobs and the workers running),
/// for the front end to ask about first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuitAsk {
    pub jobs: Vec<arko_agents::terminal::BackgroundJob>,
    /// The workers running, by their IDs.
    pub workers: Vec<String>,
}

/// Prints a front end's lines where the front end shows them: the line
/// prompt's printer, above the line being edited.
pub trait Printer: Send {
    /// Prints `text` (whole lines, each ending in a newline). `Err` when it
    /// can't: the text goes to the terminal instead.
    fn print(&mut self, text: String) -> Result<(), ()>;
}

/// The line editor's printer.
#[cfg(feature = "tui")]
struct LinePrinter(Box<dyn ExternalPrinter + Send>);

#[cfg(feature = "tui")]
impl Printer for LinePrinter {
    fn print(&mut self, text: String) -> Result<(), ()> {
        self.0.print(text).map_err(|_| ())
    }
}

/// Where everything is printed: above the prompt when there is one, else
/// to standard output (and, in one-shot mode, progress to standard error).
/// While the full-screen view owns the terminal, lines go to the feed it
/// shows instead, and echoes (what the feed has already) nowhere.
pub struct Output {
    printer: Mutex<Option<Box<dyn Printer>>>,
    /// The full-screen view's feed, while it shows the session.
    feed: Mutex<Option<remote::Feed>>,
    color: bool,
    /// Progress goes to standard error; only answers go to standard output.
    progress_to_stderr: bool,
}

impl Output {
    pub fn new(printer: Option<Box<dyn Printer>>, progress_to_stderr: bool) -> Self {
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
    #[cfg(feature = "tui")]
    pub fn detach(&self) {
        self.printer.lock().unwrap().take();
        self.feed.lock().unwrap().take();
    }

    /// Prints above the line prompt `printer` from now on.
    #[cfg(feature = "tui")]
    pub fn attach(&self, printer: Box<dyn Printer>) {
        *self.printer.lock().unwrap() = Some(printer);
    }

    /// Sends lines to `feed` from now on, which the full-screen view shows;
    /// echoes are left out.
    pub fn to_feed(&self, feed: remote::Feed) {
        *self.feed.lock().unwrap() = Some(feed);
    }

    /// Clears the line the cursor is on (the prompt's).
    #[cfg(feature = "tui")]
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
#[cfg(feature = "tui")]
struct InputHelper;

#[cfg(feature = "tui")]
impl Completer for InputHelper {
    type Candidate = String;
}

#[cfg(feature = "tui")]
impl Hinter for InputHelper {
    type Hint = String;
}

#[cfg(feature = "tui")]
impl Highlighter for InputHelper {}

#[cfg(feature = "tui")]
impl Validator for InputHelper {
    fn validate(&self, context: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        Ok(if context.input().ends_with('\\') {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Valid(None)
        })
    }
}

#[cfg(feature = "tui")]
impl Helper for InputHelper {}

/// A line as typed: continuation backslashes removed.
#[cfg(feature = "tui")]
fn joined(line: &str) -> String {
    line.replace("\\\r\n", "\n").replace("\\\n", "\n")
}

/// Set when Arko is closing: the line editor doesn't read again, so it
/// doesn't put the terminal back in raw mode after it was restored.
#[cfg(feature = "tui")]
static CLOSING: AtomicBool = AtomicBool::new(false);

/// Stops the line editor once the line it is reading (if any) ends.
#[cfg(feature = "tui")]
pub fn stop_reading() {
    CLOSING.store(true, Ordering::SeqCst);
}

/// Starts reading the user's input on its own thread. With a terminal, the
/// prompt is a line editor with history, and the returned printer prints
/// above it; without one, lines are read from standard input as they come.
/// `/keys add PROVIDER` is read here, with the key's input hidden, and
/// added to `keys`: neither reaches the session as a line.
#[cfg(feature = "tui")]
pub fn spawn_input(
    history: PathBuf,
    sender: mpsc::UnboundedSender<Input>,
    keys: Option<Arc<crate::keys::Keys>>,
) -> Option<Box<dyn Printer>> {
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
            // Saved as every front end saves it: with the others' entries,
            // whole.
            let mut shared = crate::history::History::load(Some(history.clone()));
            let printer = editor
                .create_external_printer()
                .ok()
                .map(|printer| Box::new(LinePrinter(Box::new(printer))) as Box<dyn Printer>);
            let _ = printer_sender.send(printer);
            let prompt = glyphs::render(PROMPT).into_owned();
            while !CLOSING.load(Ordering::SeqCst) {
                let mut entry = None;
                let input = match editor.readline(prompt.as_str()) {
                    Ok(line) => {
                        if let Some(keys) = &keys
                            && let Some(key) = crate::keys::added_at_prompt(&line)
                        {
                            crate::keys::add_at_prompt(keys, key);
                            continue;
                        }
                        let input = Input::Line(joined(&line));
                        entry = Some(line);
                        input
                    }
                    Err(ReadlineError::Interrupted) => Input::Interrupt,
                    // Ctrl+D is /exit, which warns of what quitting stops.
                    Err(ReadlineError::Eof) => Input::Line("/exit".into()),
                    Err(_) => Input::Eof,
                };
                let eof = matches!(input, Input::Eof);
                if sender.send(input).is_err() || eof {
                    break;
                }
                if let Some(entry) = entry {
                    shared.add(&entry);
                }
            }
        });
        printer_receiver.recv().ok().flatten()
    } else {
        std::thread::spawn(move || read_lines(sender));
        None
    }
}

#[cfg(feature = "tui")]
fn read_lines(sender: mpsc::UnboundedSender<Input>) {
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if sender.send(Input::Line(line)).is_err() {
            return;
        }
    }
    let _ = sender.send(Input::Eof);
}

/// The session's top-level facts, as a banner shows them (the box that
/// opens the full-screen view's transcript).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Banner {
    pub model: String,
    pub workers: String,
    pub workspace: String,
    pub session: String,
    /// The project's instructions file, when one was loaded.
    pub project: Option<String>,
    /// MCP servers, skills, indexes.
    pub tools: Vec<String>,
    /// Why the session runs its model, when Arko chose it (the last
    /// session's). It goes once `/model` picks another.
    pub model_note: Option<String>,
    /// Short notes, a warning's with its level.
    pub notes: Vec<(String, Option<remote::Level>)>,
}

/// What the footer after each turn shows, gathered from the events.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// The input of the main agent's latest model request, in tokens.
    pub context_tokens: Option<usize>,
    /// What the main agent's model accepts, in tokens: as its latest request
    /// said, else as the session started.
    pub context_window: Option<usize>,
    /// Each plan's usage windows, by plan, as last reported.
    pub plan_windows: std::collections::BTreeMap<String, Vec<remote::PlanWindow>>,
    /// Each worker's context, by its handle: the input of its latest
    /// request and what its model accepts, in tokens.
    pub workers: std::collections::BTreeMap<String, (usize, Option<usize>)>,
    /// The subscription plans' usage windows, as last reported: one line
    /// for every plan the session's agents use (ChatGPT, Claude).
    pub plan_usage: Option<String>,
    /// Each plan's part of `plan_usage`, by plan (`providers::plan_name`).
    pub plans: std::collections::BTreeMap<String, String>,
    /// The session's ChatGPT logins, whose number set aside the ChatGPT
    /// plan's usage says.
    pub(crate) chatgpt: Option<arko_ai::ChatGptAccounts>,
    /// The main agent's latest model request.
    pub last_request: Option<remote::LastRequest>,
    /// How long the main agent's prompt cache lasts: as its provider says,
    /// or (Claude Code) as its cache writes show.
    pub cache_lifetime: Option<std::time::Duration>,
    /// The cache lifetime is learned from the cache writes (Claude Code).
    pub learns_cache_lifetime: bool,
    /// What the main agent's latest request read from the cache and wrote
    /// to it, in tokens.
    pub cache_use: Option<(usize, usize)>,
    /// When the conversation's prompt cache was last used (Unix
    /// milliseconds): its latest request's start, a refresh that kept it
    /// warm, or what a resumed session's record says of it.
    pub cache_refreshed_at: Option<i64>,
    /// The session's record of its prompt cache, kept as the main agent's
    /// requests end. None where nothing is kept (tests).
    pub(crate) cache_memory: Option<Arc<crate::cache_memory::Memory>>,
    /// The main agent's model, as the session runs it (its id in the
    /// configuration). The record of the cache names it.
    pub(crate) main_model: Option<String>,
}

/// A request to the event printer.
pub enum Control {
    /// Print what is buffered, then answer.
    Flush(oneshot::Sender<()>),
}

/// Prints a workflow's events until the workflow is gone, every one of them
/// however far behind it falls (a burst of streamed text arrives joined in
/// fewer pieces). Native worker runs use their r1, r2, ... handles in the
/// terminal and remote feed. With `text_replies` (the main agent ends its
/// turns with a reply in plain text), its text is held until it proves to
/// be progress or its reply.
#[allow(clippy::too_many_arguments)]
pub async fn print_events(
    mut events: arko_workflows::EventReceiver,
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
                Some(event) => {
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
                        record_worker_context(&printer.stats, label, &event.event);
                    }
                    if let AgentEvent::ToolCallFormatted { tool_name, .. } = &event.event
                        && tool_name == crew::WAIT_FOR_AGENTS
                    {
                        // What a wait without ids waits for: the workers
                        // the agent started.
                        printer.running_runs = sorted_runs(
                            workflow
                                .as_ref()
                                .and_then(std::sync::Weak::upgrade)
                                .map(|workflow| {
                                    crew::waited_runs(
                                        &workflow.active_subagents(),
                                        &workflow.subagents(),
                                        (event.agent_name != MAIN).then_some(event.agent_run_id),
                                    )
                                })
                                .unwrap_or_default(),
                        );
                    }
                    if let Some(feed) = &feed {
                        feed.event(&event, label.as_deref());
                    }
                    printer.event(event);
                }
                None => break,
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

/// Keeps the worker's context for the status: the input of its latest model
/// request and what its model accepts. The requests Claude Code's subagents
/// make aren't the worker's conversation, so they leave it as it was.
fn record_worker_context(stats: &Mutex<Stats>, worker: &str, event: &AgentEvent) {
    if let AgentEvent::ModelCallUsageReported(usage) = event
        && usage.subagent.is_none()
    {
        stats.lock().unwrap().workers.insert(
            worker.to_string(),
            (usage.input_tokens, usage.max_input_tokens),
        );
    }
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
    /// The runs the main agent has going, which its `wait_for_agents` call
    /// waits for when it names none. [`print_events`] sets them as the call
    /// comes.
    running_runs: Vec<String>,
    /// Shell commands the main agent is running, by call ID.
    commands: HashMap<Uuid, String>,
    /// The provider's refusal of reasoning summaries was told.
    thinking_refused: bool,
    /// The lengths of `text` and `thinking` as the main agent's latest
    /// request went out: what its response adds goes if it fails.
    request_start: (usize, usize),
    /// Lines of text, and of thinking, were printed since that request went
    /// out: they can't be taken back.
    printed_since_request: (bool, bool),
    /// The main agent's latest request is one of Claude Code's subagents':
    /// its usage isn't the conversation's.
    subagent_request: bool,
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
            running_runs: Vec::new(),
            commands: HashMap::new(),
            thinking_refused: false,
            request_start: (0, 0),
            printed_since_request: (false, false),
            subagent_request: false,
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
            self.printed_since_request.0 = true;
        }
        while let Some(end) = self.thinking.find('\n') {
            let line: String = self.thinking.drain(..=end).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            self.output.echo(&self.output.dim(&format!("  {line}")));
            self.printed_since_request.1 = true;
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
        let at = event.timestamp.timestamp_millis();
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
                    let call = brief_waiting_for(&tool_name, &parameters, &self.running_runs);
                    self.output
                        .echo(&self.output.dim(&format!("  ↳ {worker} {call}")));
                }
            }
            AgentEvent::ToolResult {
                tool_call_id,
                tool_name,
                output,
            } if main => {
                if self.commands.remove(&tool_call_id).is_some() {
                    self.shell_result(&output);
                } else if output.starts_with(crate::shell::IN_THE_BACKGROUND) {
                    // Another tool's call that moved: its job.
                    let first = output.lines().next().unwrap_or_default();
                    self.output
                        .echo(&self.output.dim(&format!("  ⎿ {}", cut(first, 160))));
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
                let text = if tool_call_id.is_some() {
                    cut(&error_message, 400)
                } else {
                    cut(&crate::providers::explain_error(&error_message), 400)
                };
                let line = if tool_call_id.is_some() {
                    format!("  ✗ {who}{text}")
                } else {
                    format!("  ! {who}{text}")
                };
                self.output.echo(&self.output.error(&line));
            }
            // The response failed after it streamed: what it added goes, and
            // its retry streams it again.
            AgentEvent::StreamedOutputDiscarded { .. } if main => {
                let (text, thinking) = self.request_start;
                let (printed_text, printed_thinking) = self.printed_since_request;
                // What it added that wasn't printed goes, and what came
                // before stays. Once lines of it were printed, what is left
                // is all its own, and the user is told.
                if printed_text {
                    self.text.clear();
                } else {
                    self.text.truncate(text.min(self.text.len()));
                }
                if printed_thinking {
                    self.thinking.clear();
                } else {
                    self.thinking.truncate(thinking.min(self.thinking.len()));
                }
                if printed_text || printed_thinking {
                    self.output.echo(
                        &self
                            .output
                            .dim("  (the response broke off, so what it said is said again)"),
                    );
                }
                self.printed_since_request = (false, false);
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
                    "  … usage limit reached ({})",
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
            AgentEvent::ReasoningSummariesRefused { .. } if !self.thinking_refused => {
                self.thinking_refused = true;
                self.output.echo(
                    &self
                        .output
                        .dim(&format!("  {}", crate::providers::THINKING_REFUSED)),
                );
            }
            AgentEvent::HandoverRequested { input_tokens } if main => {
                self.flush();
                self.output.echo(&self.output.dim(&format!(
                    "  ⟳ {}",
                    crate::handover::requested_text(event.agent_run_id, input_tokens)
                )));
            }
            AgentEvent::ConversationHandedOver { .. } if main => {
                // The conversation is a fresh one: nothing of it is cached.
                self.forget_cache(event.agent_run_id);
                // The report's last line has no newline after it.
                self.flush();
                self.output.echo(&self.output.dim(
                    "  ⟳ handed over, and the agent continues in a fresh conversation from its report",
                ));
            }
            AgentEvent::ConversationCompacted {
                input_tokens,
                compacted_tokens,
            } if main => {
                // Its summary starts the cache over.
                self.forget_cache(event.agent_run_id);
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
            // The conversation's own requests (not Claude Code's subagents').
            AgentEvent::ModelRequestStarted {
                model,
                subagent: None,
                sizes,
                bytes_per_token,
            } if main => {
                self.request_start = (self.text.len(), self.thinking.len());
                self.printed_since_request = (false, false);
                self.subagent_request = false;
                let mut stats = self.stats.lock().unwrap();
                // The cache lasts from the request's start.
                stats.cache_refreshed_at = Some(at);
                stats.last_request = Some(remote::LastRequest {
                    at,
                    model,
                    sizes,
                    bytes_per_token,
                });
            }
            AgentEvent::ModelRequestStarted {
                subagent: Some(_), ..
            } if main => {
                self.subagent_request = true;
            }
            // The conversation's own usage: its size and its cache.
            AgentEvent::UsageReported(usage) if main && !self.subagent_request => {
                let kept = {
                    let mut stats = self.stats.lock().unwrap();
                    let written = usage.cache_write_tokens.unwrap_or(0);
                    stats.cache_use = Some((usage.cached_tokens.unwrap_or(0), written));
                    if stats.learns_cache_lifetime && written > 0 {
                        let hour = usage.cache_write_1h_tokens.unwrap_or(0) > 0;
                        stats.cache_lifetime = Some(if hour {
                            arko_ai::prompt_cache::ANTHROPIC_EXTENDED_PROMPT_CACHE_LIFETIME
                        } else {
                            arko_ai::prompt_cache::ANTHROPIC_PROMPT_CACHE_LIFETIME
                        });
                    }
                    // The request ended: the session keeps when its cache
                    // lapses, for a resume (outside the lock).
                    stats
                        .cache_memory
                        .clone()
                        .zip(stats.cache_entry(event.agent_run_id))
                };
                if let Some((memory, entry)) = kept {
                    memory.put(entry);
                }
            }
            AgentEvent::ModelCallUsageReported(usage) if main && usage.subagent.is_none() => {
                let mut stats = self.stats.lock().unwrap();
                stats.context_tokens = Some(usage.input_tokens);
                if usage.max_input_tokens.is_some() {
                    stats.context_window = usage.max_input_tokens;
                }
            }
            AgentEvent::PlanUsageReported(usage) => {
                if let Some(mut text) = crate::providers::plan_usage_text(&usage) {
                    let mut stats = self.stats.lock().unwrap();
                    let plan = crate::providers::plan_name(usage.plan);
                    if usage.plan != Some(arko_ai::SubscriptionPlan::ClaudeCode) {
                        text.push_str(&crate::logins::set_aside_note(stats.chatgpt.as_ref()));
                    }
                    stats.plans.insert(plan.to_string(), text);
                    stats.plan_usage = crate::providers::plan_usage_line(&stats.plans);
                    stats
                        .plan_windows
                        .insert(plan.to_string(), crate::providers::plan_windows(&usage));
                }
            }
            _ => {}
        }
    }

    /// The main agent's conversation was replaced, so no cache holds it:
    /// the status counts none, and the session keeps none of `run`.
    fn forget_cache(&mut self, run: Uuid) {
        let memory = {
            let mut stats = self.stats.lock().unwrap();
            stats.set_cache(None);
            stats.cache_memory.clone()
        };
        if let Some(memory) = memory {
            memory.forget(run);
        }
    }

    fn flush_thinking(&mut self) {
        if !self.thinking.trim().is_empty() {
            let rest = std::mem::take(&mut self.thinking);
            for line in rest.trim_end().lines() {
                self.output.echo(&self.output.dim(&format!("  {line}")));
            }
            self.printed_since_request.1 = true;
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
                let profile = text(crew::PROFILE_PARAM).trim();
                if !profile.is_empty() {
                    notes.push(format!("profile {profile}"));
                }
                let model = text(crew::MODEL_PARAM).trim();
                if !model.is_empty() {
                    notes.push(model.to_string());
                }
                let effort = text(crew::EFFORT_PARAM).trim();
                if !effort.is_empty() {
                    notes.push(format!("{effort} effort"));
                }
                if flag(parameters, "isolated") {
                    notes.push("worktree".to_string());
                }
                let cache = text(crew::CACHE_PARAM).trim();
                if !cache.is_empty() {
                    notes.push(format!("{cache} cache"));
                }
                format!(
                    "{bullet} {}{} “{}”",
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
                "{bullet} {} {} “{}”",
                self.output.bold("message"),
                worker(),
                cut(text("message"), 160)
            ),
            crew::WAIT_FOR_AGENTS => format!(
                "{bullet} {}",
                self.output
                    .bold(&waiting_for(parameters, &self.running_runs))
            ),
            crew::LIST_AGENTS => format!("{bullet} {}", self.output.bold("list workers")),
            crew::WORKER_OPTIONS => {
                format!("{bullet} {}", self.output.bold("list worker options"))
            }
            crew::SAVE_WORKER_PROFILE => format!(
                "{bullet} {} {}{}",
                self.output.bold("save worker profile"),
                text("name").trim(),
                match text("scope").trim() {
                    "" | "session" => String::new(),
                    scope => match crate::profiles::Place::parse(scope) {
                        Some(place) => format!(" for {}", place.reach()),
                        None => format!(" for the {scope}"),
                    },
                }
            ),
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
            READ_FILE_TOOL_NAME | WRITE_FILE_TOOL_NAME | EDIT_FILE_TOOL_NAME
            | APPLY_DIFF_TOOL_NAME | LIST_FILES_TOOL_NAME => {
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
                    "{bullet} {}{resumed} “{}”",
                    self.output.bold("worker"),
                    cut(text("prompt"), 160)
                )
            }
            "terminal_start" => format!(
                "{bullet} {} {}",
                self.output.bold("background"),
                command_line(text("command"))
            ),
            "terminal_control" => format!("{bullet} terminal {}", text("action")),
            "monitor_start" => format!(
                "{bullet} {} {}",
                self.output.bold("monitor"),
                command_line(text("command"))
            ),
            "monitor_control" => format!("{bullet} monitor {}", text("action")),
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
        let mut rest: Vec<&str> = lines.collect();
        // Moved to the background: what the agent can do next is left out.
        let moved = status.starts_with(crate::shell::IN_THE_BACKGROUND);
        if moved && !rest.is_empty() {
            rest.remove(0);
        }
        let failed = !status.starts_with("exit code 0,") && !moved;
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

/// The runs after which a call's words count the rest.
const RUNS_NAMED: usize = 3;

/// Run ids in order of their numbers (`r2` before `r10`).
pub fn sorted_runs(mut runs: Vec<String>) -> Vec<String> {
    runs.sort_by_key(|run| {
        let number = run.trim_start_matches(|c: char| !c.is_ascii_digit());
        (number.parse::<u64>().unwrap_or(u64::MAX), run.clone())
    });
    runs
}

/// The runs a `wait_for_agents` call names in `runs`, as the tool reads
/// them, a text with commas or spaces between the ids, or a list of them.
fn named_runs(parameters: &HashMap<String, serde_json::Value>) -> Vec<String> {
    let split = |text: &str| -> Vec<String> {
        text.split([',', ' ', '\n'])
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect()
    };
    match parameters.get("runs") {
        Some(serde_json::Value::String(text)) => split(text),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .flat_map(split)
            .collect(),
        _ => Vec::new(),
    }
}

/// What a `wait_for_agents` call waits for, in words. It waits for the
/// runs it names, else for `own`, the runs its agent has going, and with
/// none it waits for a message. A few runs are named ("waiting for r1, r2")
/// and many are counted ("waiting for r1, r2 and 3 more").
pub fn waiting_for(parameters: &HashMap<String, serde_json::Value>, own: &[String]) -> String {
    let named = named_runs(parameters);
    let runs: &[String] = if named.is_empty() { own } else { &named };
    if runs.is_empty() {
        "waiting for a message".to_string()
    } else if runs.len() <= RUNS_NAMED {
        format!("waiting for {}", runs.join(", "))
    } else {
        format!(
            "waiting for {}, {} and {} more",
            runs[0],
            runs[1],
            runs.len() - 2
        )
    }
}

/// A worker's call, briefly.
pub fn brief(tool: &str, parameters: &HashMap<String, serde_json::Value>) -> String {
    brief_waiting_for(tool, parameters, &[])
}

/// A worker's call, briefly, where `own` are the runs the worker has going
/// (what its `wait_for_agents` waits for).
pub fn brief_waiting_for(
    tool: &str,
    parameters: &HashMap<String, serde_json::Value>,
    own: &[String],
) -> String {
    let text = |name: &str| {
        parameters
            .get(name)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
    };
    match tool {
        SHELL_TOOL_NAME => command_line(text("command")),
        READ_FILE_TOOL_NAME | WRITE_FILE_TOOL_NAME | EDIT_FILE_TOOL_NAME | APPLY_DIFF_TOOL_NAME
        | LIST_FILES_TOOL_NAME => {
            let (verb, detail) = file_call(tool, parameters);
            format!("{verb} {detail}")
        }
        crew::MESSAGE_AGENT => format!("message to {}", text("to")),
        // A worker that starts workers makes these calls, which read as the
        // main agent's own do.
        crew::START_AGENT => format!("start worker “{}”", cut(text("task"), 80)),
        crew::STOP_AGENT => format!("stop {}", text("run")),
        crew::LIST_AGENTS => "list workers".to_string(),
        "link_agents" => "link workers".to_string(),
        HANDOVER_REPORT_TOOL_NAME => "handover report".to_string(),
        crew::WAIT_FOR_AGENTS => waiting_for(parameters, own),
        "terminal_start" => format!("background {}", command_line(text("command"))),
        tool if TerminalTool::NAMES.contains(&tool) => tool.replace('_', " "),
        "monitor_start" => format!("monitor {}", command_line(text("command"))),
        "monitor_control" => format!("monitor {}", text("action")),
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
/// it reads or changes (`src/main.rs:10-40`, `src/new.rs (12 lines)`,
/// `src/lib.rs (all)` for an edit of every occurrence).
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
        EDIT_FILE_TOOL_NAME => (
            "edit",
            if flag(parameters, "replace_all") {
                format!("{path} (all)")
            } else {
                path
            },
        ),
        // In sessions saved before edit_file.
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

/// A search index or MCP tool's call: its name as shown (`index search`,
/// `create_issue (github)`), and what it is called with, briefly.
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
        "index_search" => (
            "index search".into(),
            with(format!("“{}”", cut(&text("query"), 160))),
        ),
        "index_status" => ("index status".into(), with(text("index"))),
        "index_create" => (
            "index create".into(),
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
            (format!("{name} ({server})"), with(arguments))
        }
    }
}

/// `text` as a sentence of its own: its first letter in capitals (an
/// error such as "could not read …", shown by itself).
pub fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
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
            "index search “where are retries handled?”"
        );
        assert_eq!(brief("index_create", &params(&[])), "index create .");
        assert_eq!(
            brief(
                "index_manage",
                &params(&[("action", "rebuild".into()), ("index", "arko".into())])
            ),
            "index rebuild arko"
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
            r#"create_issue (github) {"labels":["a"],"title":"Bug"}"#
        );
        assert_eq!(brief("mcp__db__ping", &params(&[])), "ping (db)");
        assert_eq!(
            brief(
                "monitor_start",
                &params(&[("command", "npm run dev".into())])
            ),
            "monitor $ npm run dev"
        );
        assert_eq!(
            brief("monitor_control", &params(&[("action", "rearm".into())])),
            "monitor rearm"
        );
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
        let edit = |all: Option<serde_json::Value>| {
            let mut pairs = vec![
                ("path", "a.rs".into()),
                ("old_string", "a".into()),
                ("new_string", "b".into()),
            ];
            if let Some(all) = all {
                pairs.push(("replace_all", all));
            }
            brief(EDIT_FILE_TOOL_NAME, &params(&pairs))
        };
        assert_eq!(edit(None), "edit a.rs");
        assert_eq!(edit(Some(false.into())), "edit a.rs");
        assert_eq!(edit(Some(true.into())), "edit a.rs (all)");
        assert_eq!(edit(Some("true".into())), "edit a.rs (all)");
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
        // A worker has no runs, so its wait is for a message.
        assert_eq!(
            brief(crew::WAIT_FOR_AGENTS, &params(&[])),
            "waiting for a message"
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
            "● start worker (worktree) “Fix the parser.”"
        );
        assert_eq!(
            line(
                crew::MESSAGE_AGENT,
                &[("to", "w2".into()), ("message", "Use plan B.".into())]
            )
            .unwrap(),
            "● message w2 “Use plan B.”"
        );
        assert_eq!(
            line(
                crew::START_AGENT,
                &[
                    ("task", "Judge it.".into()),
                    ("profile", "3d-judge".into()),
                    ("model", "sol".into()),
                    ("effort", "high".into()),
                ]
            )
            .unwrap(),
            "● start worker (profile 3d-judge, sol, high effort) “Judge it.”"
        );
        assert_eq!(
            line(crew::WORKER_OPTIONS, &[]).unwrap(),
            "● list worker options"
        );
        assert_eq!(
            line(
                crew::SAVE_WORKER_PROFILE,
                &[("name", "judge".into()), ("scope", "project".into())]
            )
            .unwrap(),
            "● save worker profile judge for this project"
        );
        // A wait for no run in particular waits for a message when the
        // agent has no runs, and names the runs it has going.
        assert_eq!(
            line(crew::WAIT_FOR_AGENTS, &[]).unwrap(),
            "● waiting for a message"
        );
        assert_eq!(
            line(
                crew::WAIT_FOR_AGENTS,
                &[("runs", "r2, r3".into()), ("all", true.into())]
            )
            .unwrap(),
            "● waiting for r2, r3"
        );
        let mut running = EventPrinter::new(
            Arc::new(Output::new(None, true)),
            Arc::default(),
            false,
            None,
        );
        running.running_runs = vec!["r1".into(), "r2".into()];
        assert_eq!(
            running
                .call_line(crew::WAIT_FOR_AGENTS, &params(&[]))
                .map(|line| strip_styles(&line))
                .unwrap(),
            "● waiting for r1, r2"
        );
        // A worker's calls to start, stop and list workers read as the main
        // agent's do.
        assert_eq!(
            brief(
                crew::START_AGENT,
                &params(&[
                    ("agent", "worker".into()),
                    ("task", "Count the files.".into())
                ])
            ),
            "start worker “Count the files.”"
        );
        assert_eq!(
            brief(crew::STOP_AGENT, &params(&[("run", "r4".into())])),
            "stop r4"
        );
        assert_eq!(brief(crew::LIST_AGENTS, &params(&[])), "list workers");
        assert_eq!(brief("link_agents", &params(&[])), "link workers");
        // A worker that started workers waits for those, and names them.
        assert_eq!(
            brief_waiting_for(crew::WAIT_FOR_AGENTS, &params(&[]), &["r4".into()]),
            "waiting for r4"
        );
        assert_eq!(
            brief_waiting_for(crew::WAIT_FOR_AGENTS, &params(&[]), &[]),
            "waiting for a message"
        );
        assert_eq!(
            brief(crew::WAIT_FOR_AGENTS, &params(&[("runs", "r2".into())])),
            "waiting for r2"
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
        assert_eq!(
            line(
                EDIT_FILE_TOOL_NAME,
                &[
                    ("path", "b.rs".into()),
                    ("old_string", "x".into()),
                    ("new_string", "y".into()),
                    ("replace_all", true.into())
                ]
            )
            .unwrap(),
            "● edit b.rs (all)"
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
            "● start worker (5m cache) “Look it up.”"
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

    /// A wait with no runs waits for a message. With runs it names them, a
    /// few in full and many as a count. The runs it names win over the ones
    /// its agent has going, and a list of ids reads like a text.
    #[test]
    fn a_wait_says_what_it_waits_for() {
        let words = |pairs: &[(&str, serde_json::Value)], own: &[&str]| {
            let parameters: HashMap<String, serde_json::Value> = pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect();
            let own: Vec<String> = own.iter().map(|run| run.to_string()).collect();
            waiting_for(&parameters, &own)
        };
        // No runs.
        assert_eq!(words(&[], &[]), "waiting for a message");
        assert_eq!(words(&[("runs", "".into())], &[]), "waiting for a message");
        assert_eq!(words(&[("all", true.into())], &[]), "waiting for a message");
        // One run, several, and many.
        assert_eq!(words(&[], &["r1"]), "waiting for r1");
        assert_eq!(words(&[], &["r1", "r2"]), "waiting for r1, r2");
        assert_eq!(words(&[], &["r1", "r2", "r3"]), "waiting for r1, r2, r3");
        assert_eq!(
            words(&[], &["r1", "r2", "r3", "r4"]),
            "waiting for r1, r2 and 2 more"
        );
        assert_eq!(
            words(&[], &["r1", "r2", "r3", "r4", "r5"]),
            "waiting for r1, r2 and 3 more"
        );
        // The runs a call names are the ones it waits for, as the tool reads
        // them, a text with commas or spaces or a list.
        assert_eq!(
            words(&[("runs", "r2".into())], &["r1", "r2"]),
            "waiting for r2"
        );
        assert_eq!(
            words(&[("runs", "r2,r3 r4".into())], &[]),
            "waiting for r2, r3, r4"
        );
        assert_eq!(
            words(
                &[("runs", serde_json::json!(["r2", "r3, r4", "r5", "r6"]))],
                &[]
            ),
            "waiting for r2, r3 and 3 more"
        );
        // A run that already finished is named all the same.
        assert_eq!(words(&[("runs", "r1".into())], &[]), "waiting for r1");
        // The runs go by their numbers.
        assert_eq!(
            sorted_runs(vec!["r10".into(), "r2".into(), "r1".into()]),
            ["r1", "r2", "r10"]
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

    /// A ChatGPT account's credits show beside its plan windows in the
    /// status line and the figures `/cost`, the page and `worker_options`
    /// read, when the backend reports them: what is left, "unlimited
    /// credits", or nothing.
    #[tokio::test]
    async fn credits_show_beside_the_plan_windows() {
        let output = Arc::new(Output::new(None, false));
        let stats = Arc::new(Mutex::new(Stats::default()));
        let mut printer = EventPrinter::new(output, stats.clone(), false, None);
        let mut report = |credits: Option<arko_ai::PlanCredits>| {
            printer.event(WorkflowEvent {
                timestamp: chrono::Utc::now(),
                workflow_run_id: Uuid::nil(),
                agent_name: MAIN.into(),
                agent_run_id: Uuid::nil(),
                event: AgentEvent::PlanUsageReported(arko_ai::PlanUsage {
                    plan: Some(arko_ai::SubscriptionPlan::ChatGpt),
                    primary: Some(arko_ai::PlanUsageWindow {
                        used_percent: 82.0,
                        window_minutes: Some(10_080),
                        ..Default::default()
                    }),
                    credits,
                    ..Default::default()
                }),
            });
            let stats = stats.lock().unwrap();
            (
                stats.plan_usage.clone().unwrap(),
                stats.plans["ChatGPT plan"].clone(),
            )
        };
        let credits = |has_credits, unlimited, balance: Option<&str>| {
            Some(arko_ai::PlanCredits {
                has_credits,
                unlimited,
                balance: balance.map(str::to_string),
            })
        };
        let line = |text: &str| (text.to_string(), text.to_string());
        assert_eq!(report(None), line("ChatGPT plan 7 d 82%"));
        assert_eq!(
            report(credits(true, false, Some("12.50"))),
            line("ChatGPT plan 7 d 82%, credits left 12.50")
        );
        assert_eq!(
            report(credits(true, true, None)),
            line("ChatGPT plan 7 d 82%, unlimited credits")
        );
        // None left: nothing is said.
        assert_eq!(
            report(credits(false, false, Some("0"))),
            line("ChatGPT plan 7 d 82%")
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

    impl Printer for Printed {
        fn print(&mut self, text: String) -> Result<(), ()> {
            self.0
                .lock()
                .unwrap()
                .extend(text.lines().map(str::to_string));
            Ok(())
        }
    }

    /// The requests Claude Code's subagents make aren't the conversation's:
    /// their usage leaves the context's size and the cache as they were.
    #[tokio::test]
    async fn a_subagents_usage_isnt_the_conversations() {
        let output = Arc::new(Output::new(None, true));
        let stats: Arc<Mutex<Stats>> = Arc::default();
        let mut printer = EventPrinter::new(output, stats.clone(), false, None);
        let event = |event: AgentEvent| WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: MAIN.into(),
            agent_run_id: Uuid::nil(),
            event,
        };
        let started = |subagent: Option<&str>| AgentEvent::ModelRequestStarted {
            model: "claude-opus-5-5".into(),
            subagent: subagent.map(str::to_string),
            sizes: None,
            bytes_per_token: 3.0,
        };
        let usage = |input: usize, subagent: Option<&str>| {
            serde_json::from_value::<arko_ai::ModelCallUsage>(serde_json::json!({
                "call_id": "c", "provider": "anthropic", "model": "claude-opus-5-5",
                "input_tokens": input, "output_tokens": 1, "subagent": subagent,
            }))
            .unwrap()
        };
        let cached = |read: usize| arko_ai::UsageInfo {
            input_tokens: read,
            cached_tokens: Some(read),
            ..arko_ai::UsageInfo::default()
        };
        printer.event(event(started(None)));
        printer.event(event(AgentEvent::UsageReported(cached(500))));
        printer.event(event(AgentEvent::ModelCallUsageReported(usage(500, None))));
        printer.event(event(started(Some("call-1"))));
        printer.event(event(AgentEvent::UsageReported(cached(9_000))));
        printer.event(event(AgentEvent::ModelCallUsageReported(usage(
            9_000,
            Some("call-1"),
        ))));
        let stats = stats.lock().unwrap();
        assert_eq!(stats.context_tokens, Some(500));
        assert_eq!(stats.cache_use, Some((500, 0)));
    }

    /// The session keeps when its prompt cache lapses as each request ends,
    /// not as it starts: a request that was running when Arko stopped counts
    /// from the last one that ended. A handover or a compaction starts the
    /// conversation over, so nothing of the cache is kept for it.
    #[tokio::test]
    async fn the_cache_is_kept_as_requests_end_and_dropped_with_the_conversation() {
        let temp = tempfile::tempdir().unwrap();
        let memory = Arc::new(crate::cache_memory::Memory::open(temp.path()));
        let stats: Arc<Mutex<Stats>> = Arc::new(Mutex::new(Stats {
            cache_lifetime: Some(std::time::Duration::from_secs(300)),
            cache_memory: Some(memory.clone()),
            main_model: Some("claude-opus-5-5".into()),
            ..Stats::default()
        }));
        let output = Arc::new(Output::new(None, true));
        let mut printer = EventPrinter::new(output, stats.clone(), false, None);
        let run = Uuid::from_u128(7);
        let at = |millis: i64| chrono::DateTime::from_timestamp_millis(millis).unwrap();
        let event = |millis: i64, event: AgentEvent| WorkflowEvent {
            timestamp: at(millis),
            workflow_run_id: Uuid::nil(),
            agent_name: MAIN.into(),
            agent_run_id: run,
            event,
        };
        let started = || AgentEvent::ModelRequestStarted {
            model: "claude-opus-5-5".into(),
            subagent: None,
            sizes: None,
            bytes_per_token: 3.0,
        };
        let read = |tokens: usize| {
            AgentEvent::UsageReported(arko_ai::UsageInfo {
                input_tokens: tokens,
                cached_tokens: Some(tokens),
                ..arko_ai::UsageInfo::default()
            })
        };
        let kept = || memory.find(&[run]).map(|entry| (entry.at, entry.read));
        assert_eq!(kept(), None);
        // The request is running: the status counts from its start, and the
        // session keeps nothing yet.
        printer.event(event(1_000, started()));
        assert_eq!(stats.lock().unwrap().cache_refreshed_at, Some(1_000));
        assert_eq!(kept(), None);
        printer.event(event(1_500, read(400)));
        assert_eq!(kept(), Some((1_000, 400)));
        // The next one starts, and Arko stops before it ends.
        printer.event(event(60_000, started()));
        assert_eq!(stats.lock().unwrap().cache_refreshed_at, Some(60_000));
        assert_eq!(kept(), Some((1_000, 400)));
        printer.event(event(61_000, read(500)));
        assert_eq!(kept(), Some((60_000, 500)));
        // The kept entry has the model and the lifetime.
        let entry = memory.find(&[run]).unwrap();
        assert_eq!(
            (entry.model.as_str(), entry.lifetime),
            ("claude-opus-5-5", 300)
        );
        // A subagent's request of Claude Code's isn't the conversation's.
        let subagent = AgentEvent::ModelRequestStarted {
            model: "claude-haiku-5".into(),
            subagent: Some("call-1".into()),
            sizes: None,
            bytes_per_token: 3.0,
        };
        printer.event(event(90_000, subagent));
        printer.event(event(91_000, read(9_000)));
        assert_eq!(kept(), Some((60_000, 500)));
        assert_eq!(stats.lock().unwrap().cache_refreshed_at, Some(60_000));
        // A handover and a compaction replace the conversation.
        let handed_over = AgentEvent::ConversationHandedOver {
            report: "Report.".into(),
            input_tokens: 500,
            handovers: 1,
        };
        printer.event(event(120_000, handed_over));
        assert_eq!(kept(), None);
        {
            let stats = stats.lock().unwrap();
            assert_eq!((stats.cache_refreshed_at, stats.cache_use), (None, None));
        }
        printer.event(event(130_000, started()));
        printer.event(event(131_000, read(10)));
        assert_eq!(kept(), Some((130_000, 10)));
        let compacted = AgentEvent::ConversationCompacted {
            input_tokens: 900,
            compacted_tokens: Some(100),
        };
        printer.event(event(200_000, compacted));
        assert_eq!(kept(), None);
        assert_eq!(stats.lock().unwrap().cache_refreshed_at, None);
        // What was kept is gone from the session's folder too.
        assert_eq!(
            crate::cache_memory::Memory::open(temp.path()).find(&[run]),
            None
        );
    }

    /// A worker's context follows its own requests. The requests of the
    /// subagents Claude Code runs for it leave it as it was.
    #[test]
    fn a_workers_context_follows_its_own_requests_not_its_subagents() {
        let stats: Mutex<Stats> = Mutex::default();
        let usage = |input: usize, window: Option<usize>, subagent: Option<&str>| {
            AgentEvent::ModelCallUsageReported(
                serde_json::from_value::<arko_ai::ModelCallUsage>(serde_json::json!({
                    "call_id": "c", "provider": "anthropic", "model": "claude-opus-5-5",
                    "input_tokens": input, "output_tokens": 1, "subagent": subagent,
                    "max_input_tokens": window,
                }))
                .unwrap(),
            )
        };
        record_worker_context(&stats, "r1", &usage(40_000, Some(200_000), None));
        record_worker_context(&stats, "r1", &usage(9_000, Some(200_000), Some("call-1")));
        record_worker_context(&stats, "r2", &usage(5_000, None, None));
        let other = AgentEvent::UsageReported(arko_ai::UsageInfo::default());
        record_worker_context(&stats, "r1", &other);
        let workers = stats.lock().unwrap().workers.clone();
        assert_eq!(workers["r1"], (40_000, Some(200_000)));
        assert_eq!(workers["r2"], (5_000, None));
        record_worker_context(&stats, "r1", &usage(41_000, Some(200_000), None));
        assert_eq!(stats.lock().unwrap().workers["r1"], (41_000, Some(200_000)));
    }

    /// A response that fails after it streamed takes back what it added
    /// and wasn't printed, and keeps the text from before it. Once lines of
    /// it were printed, the user is told it is said again.
    #[tokio::test]
    async fn a_failed_responses_text_is_taken_back_and_earlier_text_kept() {
        let printed = Printed::default();
        let output = Arc::new(Output::new(Some(Box::new(printed.clone())), false));
        let mut printer = EventPrinter::new(output, Arc::default(), false, None);
        let event = |event: AgentEvent| WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: MAIN.into(),
            agent_run_id: Uuid::nil(),
            event,
        };
        let started = || AgentEvent::ModelRequestStarted {
            model: "claude-opus-5-5".into(),
            subagent: None,
            sizes: None,
            bytes_per_token: 3.0,
        };
        let discarded = || AgentEvent::StreamedOutputDiscarded {
            text_bytes: 6,
            reasoning_bytes: 0,
        };
        let chunk = |text: &str| AgentEvent::AssistantResponseChunk(text.into());
        printer.event(event(started()));
        printer.event(event(chunk("Before.")));
        printer.event(event(started()));
        printer.event(event(chunk(" Broken")));
        printer.event(event(discarded()));
        assert_eq!(printer.text, "Before.");
        printer.event(event(started()));
        printer.event(event(chunk(" Fixed.")));
        printer.flush();
        assert_eq!(*printed.0.lock().unwrap(), ["Before. Fixed."]);
        // A line of it printed: what is left of it goes, and it is said.
        printed.0.lock().unwrap().clear();
        printer.event(event(started()));
        printer.event(event(chunk("Half a line,\nand more")));
        printer.event(event(discarded()));
        assert_eq!(printer.text, "");
        let shown = printed.0.lock().unwrap().clone();
        assert_eq!(shown[0], "Half a line,");
        assert!(shown[1].contains("the response broke off"), "{shown:?}");
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

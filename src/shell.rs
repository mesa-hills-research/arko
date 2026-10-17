//! The `shell` tool: runs one command and returns its output once it
//! finishes. It runs commands as held jobs of the terminal tools'
//! [`TerminalManager`], so the command guard, process-group cleanup and
//! logs are the terminal module's. A command that runs too long, or that
//! the user moves (Ctrl+B), goes on in the background as a job the
//! terminal tools see. Interactive programs, and those meant to run on,
//! use the terminal tools from the start.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arko_agents::terminal::{
    BackgroundJob, JobState, StartOptions, TerminalError, TerminalManager, TerminalSnapshot,
};
use arko_agents::{
    TaskContext, Tool, ToolDefinition, ToolError, ToolParameterDefinition, ToolParameterType,
    ToolTypeDetails,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const SHELL_TOOL_NAME: &str = "shell";

/// The time a command may run when the call doesn't say.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 300;
/// The time a command may run when the call doesn't say, while commands
/// move to the background: well past the time they move at.
pub const BACKGROUND_TIMEOUT_SECONDS: u64 = 1_800;
/// How long a command runs before it moves to the background, by default
/// in sessions the user takes part in (in seconds).
pub const DEFAULT_BACKGROUND_AFTER: u64 = 600;
/// How a backgrounded command's result starts, as the transcript reads it.
pub const IN_THE_BACKGROUND: &str = "in the background as job ";
/// The most a call may ask for.
pub const MAX_TIMEOUT_SECONDS: u64 = 3_600;
/// Output longer than this many characters keeps its start and end.
const MAX_OUTPUT_CHARS: usize = 30_000;
const HEAD_CHARS: usize = 10_000;

/// The shells Arko can run commands in on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "tui", derive(clap::ValueEnum))]
#[serde(rename_all = "lowercase")]
pub enum WindowsShell {
    /// cmd.exe.
    Cmd,
    /// PowerShell 7 (`pwsh`) where installed, else Windows PowerShell.
    Powershell,
}

impl std::str::FromStr for WindowsShell {
    type Err = ();

    /// Its name as the flags take it, in any case.
    fn from_str(text: &str) -> Result<Self, ()> {
        match text.to_ascii_lowercase().as_str() {
            "cmd" => Ok(WindowsShell::Cmd),
            "powershell" => Ok(WindowsShell::Powershell),
            _ => Err(()),
        }
    }
}

/// A shell's syntax, for what Arko adds around a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Posix,
    Cmd,
    PowerShell,
}

/// What the agents are told cmd.exe is.
const CMD_NAME: &str = "cmd.exe (a command is one line: join steps with && or &, quote with double quotes; for a longer script, write it to a .cmd or .ps1 file and run that)";

/// The shell commands run in, and how it is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: PathBuf,
    /// The arguments before the command, for one command run to completion.
    pub args: Vec<String>,
    /// The arguments before the command, for a background job (the terminal
    /// tools), which may be interactive.
    pub job_args: Vec<String>,
    /// What the agents are told the shell is.
    pub name: &'static str,
    pub kind: ShellKind,
}

impl Shell {
    /// bash where there is one, else `/bin/sh`; on Windows the one
    /// `windows` names: cmd.exe, or PowerShell 7 (`pwsh`) where installed,
    /// else Windows PowerShell.
    pub fn detect(windows: WindowsShell) -> Self {
        if cfg!(windows) && windows == WindowsShell::Cmd {
            Self::cmd(find_program("cmd.exe").unwrap_or_else(|| "cmd.exe".into()))
        } else if cfg!(windows) {
            let (program, name) = match find_program("pwsh.exe") {
                Some(path) => (path, "PowerShell 7 (pwsh)"),
                None => (
                    find_program("powershell.exe").unwrap_or_else(|| "powershell.exe".into()),
                    "Windows PowerShell 5.1 (no `&&` or `||`: use `;` and `if ($?) { ... }`)",
                ),
            };
            let base = ["-NoLogo", "-NoProfile"];
            Self {
                program,
                args: base
                    .iter()
                    .chain(&["-NonInteractive", "-Command"])
                    .map(|arg| arg.to_string())
                    .collect(),
                job_args: base
                    .iter()
                    .chain(&["-Command"])
                    .map(|arg| arg.to_string())
                    .collect(),
                name,
                kind: ShellKind::PowerShell,
            }
        } else {
            let (program, name) = match find_program("bash") {
                Some(path) => (path, "bash"),
                None => (PathBuf::from("/bin/sh"), "sh (POSIX)"),
            };
            Self {
                program,
                args: vec!["-c".into()],
                job_args: vec!["-c".into()],
                name,
                kind: ShellKind::Posix,
            }
        }
    }

    /// cmd.exe at `program`, as the terminal module runs it: `/D /S /C`
    /// and the command line as written.
    fn cmd(program: PathBuf) -> Self {
        let args: Vec<String> = ["/D", "/S", "/C"].map(String::from).to_vec();
        Self {
            program,
            job_args: args.clone(),
            args,
            name: CMD_NAME,
            kind: ShellKind::Cmd,
        }
    }

    /// The command as it is run: in `directory` when the call names one
    /// (a `cd` the command guard follows, so the guard still judges paths
    /// against the working directory), and on Unix with standard input
    /// closed, so a program that would wait for input ends instead of
    /// hanging until the time limit. (cmd.exe has no way to close it for a
    /// whole command line.)
    fn wrap(&self, command: &str, directory: Option<&Path>) -> String {
        if self.kind == ShellKind::Cmd {
            // `&` comes last: a failed `cd` exits before the command runs.
            match directory {
                Some(directory) => {
                    format!("cd /d \"{}\" || exit 1 & {command}", directory.display())
                }
                None => command.to_string(),
            }
        } else if self.kind == ShellKind::PowerShell {
            match directory {
                Some(directory) => format!(
                    "Set-Location -LiteralPath '{}' -ErrorAction Stop\n{command}",
                    directory.display().to_string().replace('\'', "''")
                ),
                None => command.to_string(),
            }
        } else {
            let cd = directory
                .map(|directory| {
                    format!(
                        "cd -- '{}' || exit 1\n",
                        directory.display().to_string().replace('\'', "'\\''")
                    )
                })
                .unwrap_or_default();
            format!("exec </dev/null\n{cd}{command}")
        }
    }
}

/// The first `name` on the `PATH`.
fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// When the shell tool's commands move to the background on their own (the
/// session's "background after"), the commands running, which the user
/// can move at once (Ctrl+B), and the jobs in the background.
#[derive(Debug, Default)]
pub struct Background {
    /// After how many seconds a command still running moves; 0: never.
    after: AtomicU64,
    /// The commands running, by job.
    running: Mutex<HashMap<Uuid, Running>>,
    /// The session's terminal jobs, whose manager owns them.
    jobs: std::sync::Weak<TerminalManager>,
}

/// A command running, as [`Background`] keeps it.
#[derive(Debug)]
struct Running {
    /// The run of the agent whose call it is.
    run: Uuid,
    /// Moves it to the background.
    moved: Arc<tokio::sync::Notify>,
}

impl Background {
    /// Commands move after `after` seconds (0: never), as jobs of `jobs`.
    pub fn new(after: u64, jobs: &Arc<TerminalManager>) -> Arc<Self> {
        Arc::new(Self {
            after: after.into(),
            running: Mutex::default(),
            jobs: Arc::downgrade(jobs),
        })
    }

    /// The jobs running in the background, oldest first: the commands moved
    /// there and those the terminal tools started. They stop when Arko
    /// exits.
    pub fn jobs(&self) -> Vec<BackgroundJob> {
        self.jobs
            .upgrade()
            .map(|jobs| jobs.background_jobs())
            .unwrap_or_default()
    }

    /// Readies the jobs for Arko's exit, unless a job runs in the
    /// background that isn't one of `known` (those the user agreed to
    /// stop): then they come back. Readied, no job starts or moves to the
    /// background, so the exit stops none the user didn't know of.
    pub fn close(&self, known: &[Uuid]) -> Result<(), Vec<BackgroundJob>> {
        self.jobs.upgrade().map_or(Ok(()), |jobs| jobs.close(known))
    }

    /// After how long a command still running moves; `None`: never.
    pub fn after(&self) -> Option<Duration> {
        match self.after.load(Ordering::Relaxed) {
            0 => None,
            seconds => Some(Duration::from_secs(seconds)),
        }
    }

    /// Commands started from now on move after `after` seconds (0: never).
    pub fn set_after(&self, after: u64) {
        self.after.store(after, Ordering::Relaxed);
    }

    /// The runs of the agents running a command.
    pub fn running(&self) -> Vec<Uuid> {
        let running = self.running.lock().unwrap();
        let mut runs: Vec<Uuid> = running.values().map(|command| command.run).collect();
        runs.sort();
        runs.dedup();
        runs
    }

    /// Moves the commands of `run` to the background; returns how many.
    pub fn move_run(&self, run: Uuid) -> usize {
        let running = self.running.lock().unwrap();
        let mut moved = 0;
        for command in running.values().filter(|command| command.run == run) {
            command.moved.notify_one();
            moved += 1;
        }
        moved
    }

    /// Keeps `job`, a command of `run`, among the commands running until
    /// the guard is dropped.
    fn hold(&self, job: Uuid, run: Uuid) -> Held<'_> {
        let moved = Arc::new(tokio::sync::Notify::new());
        self.running.lock().unwrap().insert(
            job,
            Running {
                run,
                moved: moved.clone(),
            },
        );
        Held {
            background: self,
            job,
            moved,
        }
    }
}

/// A command among those running (see [`Background::hold`]).
struct Held<'a> {
    background: &'a Background,
    job: Uuid,
    moved: Arc<tokio::sync::Notify>,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.background.running.lock().unwrap().remove(&self.job);
    }
}

/// `text` as a time after which commands move to the background, in
/// seconds, as `/background-after` takes it: a whole number of seconds,
/// minutes or hours (`90s`, `10m`, `1h`), from 1 second to
/// [`MAX_TIMEOUT_SECONDS`] (an hour), or `off` (0: never), in either case.
pub fn parse_background_after(text: &str) -> Result<u64, String> {
    let text = text.trim().to_ascii_lowercase();
    if text == "off" {
        return Ok(0);
    }
    let invalid = || {
        format!(
            "`{text}` isn't a time: give one such as 90s, 10m or 1h (at most {}m), or off",
            MAX_TIMEOUT_SECONDS / 60
        )
    };
    let (number, unit) = text.split_at(text.len().saturating_sub(1));
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        _ => return Err(invalid()),
    };
    let seconds = number
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(scale))
        .ok_or_else(invalid)?;
    if seconds == 0 || seconds > MAX_TIMEOUT_SECONDS {
        return Err(invalid());
    }
    Ok(seconds)
}

/// A time in seconds as `/background-after` takes it: `90s`, `10m`, `1h`,
/// or `off` for 0.
pub fn shown_background_after(seconds: u64) -> String {
    match seconds {
        0 => "off".into(),
        seconds if seconds % 3_600 == 0 => format!("{}h", seconds / 3_600),
        seconds if seconds % 60 == 0 => format!("{}m", seconds / 60),
        seconds => format!("{seconds}s"),
    }
}

/// A time in seconds in words: `90 seconds`, `10 minutes`, `1 hour`.
pub fn background_after_words(seconds: u64) -> String {
    let (count, unit) = match seconds {
        seconds if seconds % 3_600 == 0 => (seconds / 3_600, "hour"),
        seconds if seconds % 60 == 0 => (seconds / 60, "minute"),
        seconds => (seconds, "second"),
    };
    format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
}

#[derive(Debug)]
pub struct ShellTool {
    manager: Arc<TerminalManager>,
    shell: Shell,
    /// Where the runs it is bound to keep their command channels.
    runs: Option<Arc<crate::handover::Runs>>,
    /// When commands move to the background, and those running.
    background: Arc<Background>,
    /// When commands moved to the background as the tool was made, which
    /// its description says: it stays the same through a conversation, so
    /// the prompt cache holds (a change reaches the agents as a note).
    described_after: u64,
}

impl ShellTool {
    pub fn new(manager: Arc<TerminalManager>, shell: Shell, background: Arc<Background>) -> Self {
        Self {
            manager,
            shell,
            runs: None,
            described_after: background.after.load(Ordering::Relaxed),
            background,
        }
    }

    /// Keeps the command channel of each run the tool is bound to in
    /// `runs`: every agent has the shell, so Arko reaches every run there
    /// (to ask for a handover).
    pub fn keeping_runs(mut self, runs: Arc<crate::handover::Runs>) -> Self {
        self.runs = Some(runs);
        self
    }
}

/// Cancels a job when the call running it is dropped (the task was
/// stopped), unless the job finished.
struct CancelOnDrop<'a> {
    manager: &'a TerminalManager,
    owner: Uuid,
    job: Uuid,
    armed: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.manager.cancel(self.owner, self.job);
        }
    }
}

fn parameter(name: &str, description: &str, required: bool) -> ToolParameterDefinition {
    ToolParameterDefinition {
        name: name.into(),
        description: description.into(),
        param_type: ToolParameterType::String,
        required,
        is_path: false,
        flag: None,
        default_value: None,
    }
}

/// The directory the call names (`cwd`), absolute or relative to the
/// working directory; `None` for the working directory itself.
fn directory(
    params: &HashMap<String, Value>,
    workspace: &Path,
) -> Result<Option<PathBuf>, ToolError> {
    let invalid = |message: String| ToolError::ParameterValidation {
        param_name: "cwd".into(),
        message,
    };
    let text = match params.get("cwd") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => return Ok(None),
        Some(Value::String(text)) => text.trim(),
        Some(_) => return Err(invalid("expected a directory path".into())),
    };
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    let path = match (text.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            PathBuf::from(home).join(rest.trim_start_matches(['/', '\\']))
        }
        _ => workspace.join(text),
    };
    let path = arko_agents::utils::normalize_path(&path);
    if !path.is_dir() {
        return Err(invalid(format!("{} is not a directory", path.display())));
    }
    Ok((path != arko_agents::utils::normalize_path(workspace)).then_some(path))
}

/// The time a command may run when the call doesn't say: longer while
/// commands move to the background (after `after` seconds, 0 for never),
/// so that one isn't stopped as it should move.
pub fn default_timeout(after: u64) -> u64 {
    if after == 0 {
        DEFAULT_TIMEOUT_SECONDS
    } else {
        BACKGROUND_TIMEOUT_SECONDS
    }
}

fn timeout_seconds(params: &HashMap<String, Value>, default: u64) -> Result<u64, ToolError> {
    let invalid = || ToolError::ParameterValidation {
        param_name: "timeout_seconds".into(),
        message: format!("expected a whole number of seconds from 1 to {MAX_TIMEOUT_SECONDS}"),
    };
    let seconds = match params.get("timeout_seconds") {
        None | Some(Value::Null) => return Ok(default),
        Some(Value::Number(number)) => number.as_u64().ok_or_else(invalid)?,
        Some(Value::String(text)) if text.trim().is_empty() => return Ok(default),
        Some(Value::String(text)) => text.trim().parse().map_err(|_| invalid())?,
        Some(_) => return Err(invalid()),
    };
    if seconds == 0 || seconds > MAX_TIMEOUT_SECONDS {
        return Err(invalid());
    }
    Ok(seconds)
}

#[async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SHELL_TOOL_NAME.into(),
            description: format!(
                "Runs a command in {} and returns its exit code and output (standard output and error together) once it finishes. Each call starts a new shell in the working directory, so a `cd` doesn't carry over to the next call: to run a command somewhere else, give the call that directory as `cwd`. {} A command still running after timeout_seconds is stopped.{} Output over {MAX_OUTPUT_CHARS} characters keeps its start and end, and the result names a log file with all of it.",
                self.shell.name,
                if self.shell.kind == ShellKind::Posix {
                    "Standard input is closed: a program that asks for input fails instead of waiting."
                } else {
                    "A program that waits for input waits until timeout_seconds: give it its input (`echo y| command`), or run it with terminal_start."
                },
                match self.described_after {
                    0 => String::new(),
                    after => format!(
                        " A command still running after {} moves to the background, where it goes on as a job: the result gives its job ID and output so far, and a terminal update tells you when it finishes.",
                        background_after_words(after)
                    ),
                }
            ),
            parameters: vec![
                parameter("command", "The command.", true),
                parameter(
                    "cwd",
                    "The directory to run this command in, absolute or relative to the working directory (default: the working directory). It applies to this call only.",
                    false,
                ),
                ToolParameterDefinition {
                    param_type: ToolParameterType::Integer,
                    ..parameter(
                        "timeout_seconds",
                        &format!(
                            "How long the command may run, in seconds (default {}, at most {MAX_TIMEOUT_SECONDS}).",
                            default_timeout(self.described_after)
                        ),
                        false,
                    )
                },
            ],
            output_based_example: None,
            parallel_safe: false,
            tool_type_details: ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: None,
        }
    }

    /// The call enforces its own time limit (`timeout_seconds`).
    fn uses_host_timeout(&self) -> bool {
        false
    }

    fn bind_runtime(
        &self,
        task_id: Uuid,
        sender: tokio::sync::mpsc::WeakSender<arko_agents::UserCommand>,
    ) {
        // Its commands moved to the background tell the agent how they end,
        // as the terminal tools' jobs do.
        self.manager.bind_runtime(task_id, sender.clone());
        if let Some(runs) = &self.runs {
            runs.bind(task_id, sender);
        }
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let command = params
            .get("command")
            .and_then(Value::as_str)
            .filter(|command| !command.trim().is_empty())
            .ok_or_else(|| ToolError::MissingParameter {
                param_name: "command".into(),
            })?;
        // cmd.exe runs the first line of a command and drops the rest.
        let command = if self.shell.kind == ShellKind::Cmd {
            let command = command.trim_end();
            if command.contains(['\n', '\r']) {
                return Err(ToolError::ParameterValidation {
                    param_name: "command".into(),
                    message: "cmd.exe runs one line: join the steps with && (or &), or write them to a .cmd file and run it".into(),
                });
            }
            command
        } else {
            command
        };
        let after = self.background.after();
        let timeout = timeout_seconds(
            &params,
            default_timeout(after.map_or(0, |after| after.as_secs())),
        )?;
        let directory = directory(&params, context.current_working_directory)?;
        let started = Instant::now();
        // Held: the agent hears of it only if it moves to the background.
        let job = self
            .manager
            .start(
                context,
                &self.shell.wrap(command, directory.as_deref()),
                StartOptions {
                    terminal: false,
                    detect_prompts: false,
                    timeout: Some(Duration::from_secs(timeout)),
                    held: true,
                    shell_args: Some(self.shell.args.clone()),
                    // Without the lines `wrap` adds.
                    label: Some(command.to_string()),
                },
            )
            .map_err(|error| match error {
                TerminalError::Refused(refusal) => ToolError::ExecutionFailed(refusal.message),
                error => ToolError::ExecutionFailed(error.to_string()),
            })?;
        let mut guard = CancelOnDrop {
            manager: &self.manager,
            owner: context.task_id,
            job: job.job_id,
            armed: true,
        };
        let held = self.background.hold(job.job_id, context.task_id);
        let moves_at = after.map(|after| tokio::time::Instant::from_std(started + after));
        let failed = |error: TerminalError| ToolError::ExecutionFailed(error.to_string());
        let mut revision = job.revision;
        let done = loop {
            let moved_by_user = tokio::select! {
                snapshot = self.manager.wait(
                    context.task_id,
                    job.job_id,
                    revision,
                    Duration::from_secs(30),
                ) => {
                    let snapshot = snapshot.map_err(failed)?;
                    if snapshot.state.is_terminal() {
                        break snapshot;
                    }
                    revision = snapshot.revision;
                    continue;
                }
                () = tokio::time::sleep_until(moves_at.unwrap_or_else(tokio::time::Instant::now)), if moves_at.is_some() => false,
                () = held.moved.notified() => true,
            };
            match self.manager.release(context.task_id, job.job_id) {
                Ok(snapshot) => {
                    // The command goes on: a stopped turn no longer stops it.
                    guard.armed = false;
                    return Ok(moved(&snapshot, started.elapsed(), after, moved_by_user));
                }
                // It finished as it was to move: its result is here.
                Err(TerminalError::Finished) => {
                    break self
                        .manager
                        .status(context.task_id, job.job_id, None)
                        .map_err(failed)?;
                }
                Err(error) => return Err(failed(error)),
            }
        };
        drop(held);
        guard.armed = false;
        let elapsed = started.elapsed();
        let output = std::fs::read(&done.log_path)
            .map(|bytes| String::from_utf8_lossy(&bytes).replace("\r\n", "\n"))
            .unwrap_or_default();
        // The job's log stays; the catalog forgets the job, so the terminal
        // tools' listing shows only background jobs.
        let _ = self.manager.forget(context.task_id, job.job_id);
        let status = match done.state {
            JobState::Succeeded | JobState::Failed => match done.exit_code {
                Some(code) => format!("exit code {code}"),
                None => "ended by a signal".to_string(),
            },
            JobState::TimedOut => format!("stopped after {timeout} s (timeout_seconds)"),
            JobState::Cancelled => "cancelled".to_string(),
            JobState::Interrupted => "interrupted".to_string(),
            JobState::Running => unreachable!("the loop ends at a terminal state"),
        };
        let error = done
            .error
            .map(|error| format!(" ({error})"))
            .unwrap_or_default();
        Ok(format!(
            "{status}{error}, {}\n{}",
            seconds(elapsed),
            body(&output, &done.log_path, done.log_truncated)
        ))
    }
}

/// The result of a command that moved to the background after `elapsed`,
/// by the user (`by_user`) or as it ran past `after`: its job, what to do
/// next, and its output so far.
fn moved(
    job: &TerminalSnapshot,
    elapsed: Duration,
    after: Option<Duration>,
    by_user: bool,
) -> String {
    let output = std::fs::read(&job.log_path)
        .map(|bytes| String::from_utf8_lossy(&bytes).replace("\r\n", "\n"))
        .unwrap_or_default();
    let why = match after.filter(|_| !by_user) {
        Some(after) => format!(
            "It ran longer than {}, so it moved to the background.",
            background_after_words(after.as_secs())
        ),
        None => "The user moved it to the background.".to_string(),
    };
    format!(
        "{IN_THE_BACKGROUND}{}, {}\n{why} It goes on running, and timeout_seconds still stops it. A terminal update tells you when it finishes. terminal_status reads its output, and terminal_control waits for it or stops it. Its output so far:\n{}",
        job.job_id,
        seconds(elapsed),
        body(&output, &job.log_path, job.log_truncated)
    )
}

fn seconds(elapsed: Duration) -> String {
    format!("{:.1} s", elapsed.as_secs_f64())
}

/// The output as the agent sees it: all of it, or its start and end with a
/// line saying what was left out and where the rest is.
fn body(output: &str, log: &Path, log_truncated: bool) -> String {
    let output = output.trim_end();
    if output.is_empty() {
        return "(no output)".into();
    }
    let total = output.chars().count();
    let truncated_log = if log_truncated {
        " (the log holds the first 4 MiB)"
    } else {
        ""
    };
    if total <= MAX_OUTPUT_CHARS {
        return output.to_string();
    }
    let head_end = output
        .char_indices()
        .nth(HEAD_CHARS)
        .map_or(output.len(), |(index, _)| index);
    let tail_start = output
        .char_indices()
        .nth(total - (MAX_OUTPUT_CHARS - HEAD_CHARS))
        .map_or(output.len(), |(index, _)| index);
    format!(
        "{}\n[... {} characters left out, with all of the output in {}{truncated_log} ...]\n{}",
        &output[..head_end],
        total - MAX_OUTPUT_CHARS,
        log.display(),
        &output[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use arko_agents::terminal::{CommandGuard, ShellDialect, TerminalConfig};

    fn context<'a>(task_id: Uuid, directory: &'a Path) -> TaskContext<'a> {
        TaskContext::new(task_id, "main", "test", directory)
    }

    fn tool(directory: &Path) -> ShellTool {
        tool_moving_after(directory, 0)
    }

    /// The tool, with commands moving to the background after `after`
    /// seconds (0: never).
    fn tool_moving_after(directory: &Path, after: u64) -> ShellTool {
        let shell = Shell::detect(WindowsShell::Cmd);
        let mut config = TerminalConfig::new(directory.join("jobs"));
        config.shell = shell.program.clone();
        config.shell_args = shell.job_args.clone();
        let manager = TerminalManager::new(config).unwrap();
        ShellTool::new(manager.clone(), shell, Background::new(after, &manager))
    }

    async fn run(tool: &ShellTool, directory: &Path, params: Value) -> Result<String, ToolError> {
        let params = serde_json::from_value(params).unwrap();
        tool.execute(&context(Uuid::new_v4(), directory), params)
            .await
    }

    #[tokio::test]
    async fn a_command_returns_its_exit_code_and_output() {
        let temp = tempfile::tempdir().unwrap();
        let tool = tool(temp.path());
        let result = run(
            &tool,
            temp.path(),
            serde_json::json!({"command": "echo hello"}),
        )
        .await
        .unwrap();
        assert!(result.starts_with("exit code 0, "), "{result}");
        assert!(result.ends_with("\nhello"), "{result}");
        let result = run(&tool, temp.path(), serde_json::json!({"command": "exit 3"}))
            .await
            .unwrap();
        assert!(result.starts_with("exit code 3, "), "{result}");
        assert!(result.ends_with("(no output)"), "{result}");
    }

    #[tokio::test]
    async fn the_command_guard_refuses_and_the_call_says_why() {
        let temp = tempfile::tempdir().unwrap();
        let tool = tool(temp.path());
        let command = if cfg!(windows) {
            "rd /s /q C:\\Windows"
        } else {
            "rm -rf /usr"
        };
        let error = run(&tool, temp.path(), serde_json::json!({"command": command}))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::ExecutionFailed(_)), "{error:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn standard_input_is_closed_and_a_slow_command_is_stopped() {
        let temp = tempfile::tempdir().unwrap();
        let tool = tool(temp.path());
        // `cat` with no file reads standard input: closed, it ends at once.
        let result = run(
            &tool,
            temp.path(),
            serde_json::json!({"command": "cat; echo done"}),
        )
        .await
        .unwrap();
        assert!(result.ends_with("\ndone"), "{result}");
        let result = run(
            &tool,
            temp.path(),
            serde_json::json!({"command": "sleep 30", "timeout_seconds": "1"}),
        )
        .await
        .unwrap();
        assert!(
            result.starts_with("stopped after 1 s (timeout_seconds)"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn long_output_keeps_its_start_and_end_and_names_the_log() {
        let temp = tempfile::tempdir().unwrap();
        let tool = tool(temp.path());
        let command = if cfg!(windows) {
            "for /L %i in (1,1,20000) do @echo line %i"
        } else {
            "seq 1 20000 | sed 's/^/line /'"
        };
        let result = run(&tool, temp.path(), serde_json::json!({"command": command}))
            .await
            .unwrap();
        assert!(result.contains("\nline 1\nline 2\n"), "{}", &result[..200]);
        assert!(result.ends_with("line 20000"));
        assert!(result.contains("characters left out, with all of the output in"));
        assert!(result.chars().count() < MAX_OUTPUT_CHARS + 500);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_runs_in_cwd_and_the_next_in_the_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir_all(workspace.join("sub dir/it's")).unwrap();
        let tool = tool(&workspace);
        let result = run(
            &tool,
            &workspace,
            serde_json::json!({"command": "pwd", "cwd": "sub dir/it's"}),
        )
        .await
        .unwrap();
        assert!(
            result.ends_with(&format!("\n{}/sub dir/it's", workspace.display())),
            "{result}"
        );
        let result = run(&tool, &workspace, serde_json::json!({"command": "pwd"}))
            .await
            .unwrap();
        assert!(
            result.ends_with(&format!("\n{}", workspace.display())),
            "{result}"
        );
        let error = run(
            &tool,
            &workspace,
            serde_json::json!({"command": "pwd", "cwd": "missing"}),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, ToolError::ParameterValidation { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn cwd_is_resolved_against_the_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path();
        std::fs::create_dir(workspace.join("src")).unwrap();
        let params = |value: Value| {
            serde_json::from_value::<HashMap<String, Value>>(serde_json::json!({"cwd": value}))
                .unwrap()
        };
        assert_eq!(directory(&HashMap::new(), workspace).unwrap(), None);
        assert_eq!(directory(&params("".into()), workspace).unwrap(), None);
        assert_eq!(directory(&params(".".into()), workspace).unwrap(), None);
        assert_eq!(
            directory(&params("src".into()), workspace).unwrap(),
            Some(arko_agents::utils::normalize_path(&workspace.join("src")))
        );
        assert_eq!(
            directory(&params("src/..".into()), workspace).unwrap(),
            None
        );
        assert!(directory(&params(3.into()), workspace).is_err());
    }

    #[test]
    fn timeouts_are_whole_seconds_within_the_limit() {
        let params = |value: Value| {
            serde_json::from_value::<HashMap<String, Value>>(
                serde_json::json!({"timeout_seconds": value}),
            )
            .unwrap()
        };
        let check = |params: &HashMap<String, Value>| timeout_seconds(params, 300);
        assert_eq!(check(&HashMap::new()).unwrap(), 300);
        assert_eq!(check(&params("60".into())).unwrap(), 60);
        assert_eq!(check(&params(90.into())).unwrap(), 90);
        assert!(check(&params("0".into())).is_err());
        assert!(check(&params("3601".into())).is_err());
        assert!(check(&params("soon".into())).is_err());
        // While commands move to the background, the default is longer.
        assert_eq!(default_timeout(0), 300);
        assert_eq!(default_timeout(600), 1800);
    }

    /// The text the tool adds around a command passes the guard, which
    /// still refuses what the command itself would do.
    #[test]
    fn the_wrapper_passes_the_guard() {
        let guard = CommandGuard::default();
        let directory = std::env::temp_dir().join("arko-guard");
        let posix = |command: &str| format!("exec </dev/null\n{command}");
        assert!(
            guard
                .check(&posix("ls -la"), ShellDialect::Posix, &directory)
                .is_ok()
        );
        assert!(
            guard
                .check(&posix("rm -rf build"), ShellDialect::Posix, &directory)
                .is_ok()
        );
        assert!(
            guard
                .check(&posix("rm -rf ~"), ShellDialect::Posix, &directory)
                .is_err()
        );
        // In another directory, paths are judged from there, against the
        // same working directory.
        let shell = Shell {
            program: "/bin/bash".into(),
            args: vec!["-c".into()],
            job_args: vec!["-c".into()],
            name: "bash",
            kind: ShellKind::Posix,
        };
        let inside = directory.join("sub dir").join("it's");
        // Not in the temporary directory, which the guard allows.
        let outside = Path::new("/opt/arko-guard-elsewhere");
        let check = |command: &str, cwd: &Path| {
            guard.check(
                &shell.wrap(command, Some(cwd)),
                ShellDialect::Posix,
                &directory,
            )
        };
        assert!(check("rm -rf build", &inside).is_ok());
        assert!(check("rm -rf ../../build", &inside).is_ok());
        assert!(check("rm -rf ../../..", &inside).is_err());
        assert!(check("rm -rf build", outside).is_err());
        assert!(check("ls -la", outside).is_ok());
    }

    /// cmd.exe's `cd /d` before the command is followed the same way.
    #[cfg(unix)]
    #[test]
    fn the_cmd_wrapper_passes_the_guard() {
        let guard = CommandGuard::default();
        let directory = std::env::temp_dir().join("arko-guard");
        let shell = Shell::cmd("cmd.exe".into());
        let inside = directory.join("sub dir").join("it's");
        let outside = Path::new("/opt/arko-guard-elsewhere");
        let check = |command: &str, cwd: Option<&Path>| {
            guard.check(&shell.wrap(command, cwd), ShellDialect::Cmd, &directory)
        };
        assert_eq!(
            shell.wrap("dir", Some(&inside)),
            format!("cd /d \"{}\" || exit 1 & dir", inside.display())
        );
        assert!(check("rd /s /q build", None).is_ok());
        assert!(check("rd /s /q build", Some(&inside)).is_ok());
        assert!(check("del /q *.obj && rd /s /q out", Some(&inside)).is_ok());
        assert!(check("rd /s /q build", Some(outside)).is_err());
        assert!(check("dir", Some(outside)).is_ok());
    }

    #[tokio::test]
    async fn cmd_takes_one_line() {
        let temp = tempfile::tempdir().unwrap();
        let config = TerminalConfig::new(temp.path().join("jobs"));
        let tool = ShellTool::new(
            TerminalManager::new(config).unwrap(),
            Shell::cmd("cmd.exe".into()),
            Arc::default(),
        );
        let error = run(
            &tool,
            temp.path(),
            serde_json::json!({"command": "echo one\necho two"}),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, ToolError::ParameterValidation { message, .. } if message.contains("one line")),
            "{error:?}"
        );
        let description = tool.definition().description;
        assert!(
            description.contains("cmd.exe (a command is one line"),
            "{description}"
        );
        assert!(
            description.contains("waits until timeout_seconds"),
            "{description}"
        );
    }

    /// A command still running at the set time moves to the background: the
    /// call returns its job, which runs on, and the agent's runtime hears
    /// when it ends.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_moves_to_the_background_after_the_set_time() {
        let temp = tempfile::tempdir().unwrap();
        let tool = tool_moving_after(temp.path(), 1);
        let task = Uuid::new_v4();
        let (sender, mut updates) = tokio::sync::mpsc::channel(8);
        tool.bind_runtime(task, sender.downgrade());
        let mut finished = tool.manager.subscribe_finished();
        let params = serde_json::from_value(
            serde_json::json!({"command": "echo early; sleep 2; echo late"}),
        )
        .unwrap();
        let result = tool
            .execute(&context(task, temp.path()), params)
            .await
            .unwrap();
        let mut lines = result.lines();
        let first = lines.next().unwrap();
        assert!(first.starts_with(IN_THE_BACKGROUND), "{result}");
        assert!(
            lines
                .next()
                .unwrap()
                .starts_with("It ran longer than 1 second, so it moved to the background."),
            "{result}"
        );
        assert!(result.ends_with("Its output so far:\nearly"), "{result}");
        let job: Uuid = first[IN_THE_BACKGROUND.len()..]
            .split(',')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(tool.background.running().is_empty());
        assert_eq!(
            tool.manager.status(task, job, None).unwrap().state,
            JobState::Running
        );
        let done = tokio::time::timeout(Duration::from_secs(10), finished.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((done.job_id, done.state), (job, JobState::Succeeded));
        let Some(arko_agents::UserCommand::InjectContext { content }) =
            tokio::time::timeout(Duration::from_secs(5), updates.recv())
                .await
                .unwrap()
        else {
            panic!("no terminal update")
        };
        assert!(content.contains("\"succeeded\""), "{content}");
        assert!(content.contains("late"), "{content}");
    }

    /// The user moves a command at once (Ctrl+B), also with the setting off.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_user_moves_a_command_to_the_background() {
        let temp = tempfile::tempdir().unwrap();
        let tool = Arc::new(tool(temp.path()));
        let task = Uuid::new_v4();
        let call = {
            let (tool, directory) = (tool.clone(), temp.path().to_path_buf());
            tokio::spawn(async move {
                let params =
                    serde_json::from_value(serde_json::json!({"command": "sleep 30"})).unwrap();
                tool.execute(&context(task, &directory), params).await
            })
        };
        while tool.background.running().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(tool.background.running(), [task]);
        assert_eq!(tool.background.move_run(Uuid::new_v4()), 0);
        assert_eq!(tool.background.move_run(task), 1);
        let result = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(result.starts_with(IN_THE_BACKGROUND), "{result}");
        assert!(
            result.contains("\nThe user moved it to the background."),
            "{result}"
        );
        assert!(
            result.ends_with("Its output so far:\n(no output)"),
            "{result}"
        );
        // A command that finishes quickly is still returned whole.
        let result = run(
            &tool,
            temp.path(),
            serde_json::json!({"command": "echo hi"}),
        )
        .await
        .unwrap();
        assert!(result.starts_with("exit code 0, "), "{result}");
    }

    /// The description says when commands move to the background, as they
    /// did when the tool was made, and the longer default time.
    #[test]
    fn the_description_says_when_commands_move() {
        let temp = tempfile::tempdir().unwrap();
        let description = |tool: &ShellTool| {
            let definition = tool.definition();
            let timeout = definition
                .parameters
                .iter()
                .find(|parameter| parameter.name == "timeout_seconds")
                .unwrap()
                .description
                .clone();
            (definition.description, timeout)
        };
        let (text, timeout) = description(&tool_moving_after(&temp.path().join("on"), 600));
        assert!(
            text.contains("A command still running after 10 minutes moves to the background"),
            "{text}"
        );
        assert!(timeout.contains("default 1800"), "{timeout}");
        let tool = tool(&temp.path().join("off"));
        let (text, timeout) = description(&tool);
        assert!(!text.contains("background"), "{text}");
        assert!(timeout.contains("default 300"), "{timeout}");
        // A change while the tool runs leaves its description as it was.
        tool.background.set_after(60);
        assert_eq!(description(&tool).0, text);
    }

    #[test]
    fn background_times_read_and_show() {
        assert_eq!(parse_background_after("10m"), Ok(600));
        assert_eq!(parse_background_after(" 90S "), Ok(90));
        assert_eq!(parse_background_after("1h"), Ok(3600));
        assert_eq!(parse_background_after("off"), Ok(0));
        for wrong in ["", "10", "0m", "61m", "2h", "m", "soon", "-5s"] {
            assert!(parse_background_after(wrong).is_err(), "{wrong}");
        }
        assert_eq!(shown_background_after(600), "10m");
        assert_eq!(shown_background_after(90), "90s");
        assert_eq!(shown_background_after(3600), "1h");
        assert_eq!(shown_background_after(0), "off");
        assert_eq!(background_after_words(60), "1 minute");
        assert_eq!(background_after_words(90), "90 seconds");
    }
}

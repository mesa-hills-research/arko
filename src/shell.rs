//! The `shell` tool: runs one command and returns its output once it
//! finishes. It runs commands through its own [`TerminalManager`], so the
//! command guard, process-group cleanup and logs are the terminal module's;
//! long-running and interactive programs use the terminal tools instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arko_agents::terminal::{JobState, StartOptions, TerminalError, TerminalManager};
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
/// The most a call may ask for.
pub const MAX_TIMEOUT_SECONDS: u64 = 3_600;
/// Output longer than this many characters keeps its start and end.
const MAX_OUTPUT_CHARS: usize = 30_000;
const HEAD_CHARS: usize = 10_000;

/// The shells fin can run commands in on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum WindowsShell {
    /// cmd.exe.
    Cmd,
    /// PowerShell 7 (`pwsh`) where installed, else Windows PowerShell.
    Powershell,
}

/// A shell's syntax, for what fin adds around a command.
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

#[derive(Debug)]
pub struct ShellTool {
    manager: Arc<TerminalManager>,
    shell: Shell,
}

impl ShellTool {
    pub fn new(manager: Arc<TerminalManager>, shell: Shell) -> Self {
        Self { manager, shell }
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

fn timeout_seconds(params: &HashMap<String, Value>) -> Result<u64, ToolError> {
    let invalid = || ToolError::ParameterValidation {
        param_name: "timeout_seconds".into(),
        message: format!("expected a whole number of seconds from 1 to {MAX_TIMEOUT_SECONDS}"),
    };
    let seconds = match params.get("timeout_seconds") {
        None | Some(Value::Null) => return Ok(DEFAULT_TIMEOUT_SECONDS),
        Some(Value::Number(number)) => number.as_u64().ok_or_else(invalid)?,
        Some(Value::String(text)) if text.trim().is_empty() => return Ok(DEFAULT_TIMEOUT_SECONDS),
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
                "Run a command in {} in the working directory (or in `cwd`), wait for it to finish, and return its exit code and output (standard output and error together). Each call starts a new shell in the working directory, or in `cwd` when the call gives one: a `cd` doesn't carry over to the next call. {} A command still running after timeout_seconds is stopped; run servers, watchers and interactive programs with terminal_start instead. Output over {MAX_OUTPUT_CHARS} characters keeps its start and end, and the result names a log file with all of it. Commands that would delete, move or overwrite the filesystem root, the home directory, system directories, devices, or paths outside the working directory are refused.",
                self.shell.name,
                if self.shell.kind == ShellKind::Posix {
                    "Standard input is closed: a program that asks for input fails instead of waiting."
                } else {
                    "A program that waits for input waits until timeout_seconds: give it its input (`echo y| command`), or run it with terminal_start."
                }
            ),
            parameters: vec![
                parameter("command", "The command.", true),
                parameter(
                    "cwd",
                    "The directory to run this command in, absolute or relative to the working directory (default: the working directory). It applies to this call only.",
                    false,
                ),
                parameter(
                    "timeout_seconds",
                    &format!(
                        "How long the command may run, in seconds (default {DEFAULT_TIMEOUT_SECONDS}, at most {MAX_TIMEOUT_SECONDS})."
                    ),
                    false,
                ),
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
        let timeout = timeout_seconds(&params)?;
        let directory = directory(&params, context.current_working_directory)?;
        let started = Instant::now();
        let job = self
            .manager
            .start(
                context,
                &self.shell.wrap(command, directory.as_deref()),
                StartOptions {
                    terminal: false,
                    detect_prompts: false,
                    timeout: Some(Duration::from_secs(timeout)),
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
        let mut revision = job.revision;
        let done = loop {
            let snapshot = self
                .manager
                .wait(
                    context.task_id,
                    job.job_id,
                    revision,
                    Duration::from_secs(30),
                )
                .await
                .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
            if snapshot.state.is_terminal() {
                break snapshot;
            }
            revision = snapshot.revision;
        };
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
        "{}\n[... {} characters left out; all of the output is in {}{truncated_log} ...]\n{}",
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
        let shell = Shell::detect(WindowsShell::Cmd);
        let mut config = TerminalConfig::new(directory.join("jobs"));
        config.shell = shell.program.clone();
        config.shell_args = shell.args.clone();
        ShellTool::new(TerminalManager::new(config).unwrap(), shell)
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
        assert!(result.contains("characters left out; all of the output is in"));
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
        assert_eq!(timeout_seconds(&HashMap::new()).unwrap(), 300);
        assert_eq!(timeout_seconds(&params("60".into())).unwrap(), 60);
        assert_eq!(timeout_seconds(&params(90.into())).unwrap(), 90);
        assert!(timeout_seconds(&params("0".into())).is_err());
        assert!(timeout_seconds(&params("3601".into())).is_err());
        assert!(timeout_seconds(&params("soon".into())).is_err());
    }

    /// The text the tool adds around a command passes the guard, which
    /// still refuses what the command itself would do.
    #[test]
    fn the_wrapper_passes_the_guard() {
        let guard = CommandGuard::default();
        let directory = std::env::temp_dir().join("fin-guard");
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
        let outside = Path::new("/opt/fin-guard-elsewhere");
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
        let directory = std::env::temp_dir().join("fin-guard");
        let shell = Shell::cmd("cmd.exe".into());
        let inside = directory.join("sub dir").join("it's");
        let outside = Path::new("/opt/fin-guard-elsewhere");
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
}

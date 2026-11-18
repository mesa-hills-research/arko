//! `run_code`, the tool. It checks the call, runs the code in a child
//! process (see [`super::child`]), and words how the run ended for the
//! model.

use std::collections::HashMap;

use arko_agents::{
    TaskContext, Tool, ToolDefinition, ToolError, ToolParameterDefinition, ToolParameterType,
    ToolTypeDetails,
};
use async_trait::async_trait;
use serde_json::Value;

use super::{COMPUTE_SECONDS, HEAP_BYTES, MAX_CALLS, MAX_CODE_BYTES, MAX_RESULT_CHARS, RUN_CODE};
use crate::shell::{DEFAULT_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS};

/// `run_code`, which calls `callable`, the tools of [`super::CALLABLE`]
/// the session has.
#[derive(Debug)]
pub struct RunCodeTool {
    callable: Vec<&'static str>,
}

impl RunCodeTool {
    pub fn new(callable: &[&'static str]) -> Self {
        Self {
            callable: callable
                .iter()
                .copied()
                .filter(|name| super::CALLABLE.contains(name))
                .collect(),
        }
    }

    /// The description, the only place that teaches the model the API.
    fn description(&self) -> String {
        let names = match self.callable.as_slice() {
            [] => "none".to_string(),
            [one] => one.to_string(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        };
        format!(
            "Runs JavaScript that calls your tools and returns only what the code returns. Use it when a step would otherwise take many calls or put a large result in front of you, such as the same call over many files, a result to count or filter before you read it, or calls that feed each other. For one read, one edit or one command, call the tool itself.\n\n\
             The code is the body of an async function. `await tools.read_file({{ path }})` calls a tool with the arguments it takes as a tool, and gives its result as text. The tools it can call are {names}. A call that fails or is refused throws an Error with the tool's message. `return` what you need (objects come back as JSON), and `console.log` adds lines above it. The result is cut at {} characters.\n\n\
             The code has no files, network, imports or timers of its own, and TypeScript syntax is an error. A run may take timeout_seconds, compute for {COMPUTE_SECONDS} seconds, hold {} MB and make {MAX_CALLS} tool calls.",
            thousands(MAX_RESULT_CHARS),
            HEAP_BYTES / (1024 * 1024),
        )
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

#[async_trait]
impl Tool for RunCodeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: RUN_CODE.into(),
            description: self.description(),
            output_based_example: None,
            parameters: vec![
                parameter(
                    "code",
                    "The body of an async function, in JavaScript.",
                    true,
                ),
                parameter(
                    "purpose",
                    "What the code is for, in a few words that finish \"Ran code to …\", such as \"count TODO comments per crate\".",
                    true,
                ),
                ToolParameterDefinition {
                    param_type: ToolParameterType::Integer,
                    ..parameter(
                        "timeout_seconds",
                        &format!(
                            "How long the whole run may take, in seconds (default {DEFAULT_TIMEOUT_SECONDS}, at most {MAX_TIMEOUT_SECONDS})."
                        ),
                        false,
                    )
                },
            ],
            parallel_safe: false,
            read_only: false,
            tool_type_details: ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: None,
        }
    }

    /// The run keeps its own time limit (`timeout_seconds`).
    fn uses_host_timeout(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let code = params
            .get("code")
            .and_then(Value::as_str)
            .filter(|code| !code.trim().is_empty())
            .ok_or_else(|| ToolError::MissingParameter {
                param_name: "code".into(),
            })?;
        if code.len() > MAX_CODE_BYTES {
            return Err(ToolError::ParameterValidation {
                param_name: "code".into(),
                message: format!(
                    "the code is {} KB, over the {} KB run_code takes",
                    code.len().div_ceil(1024),
                    MAX_CODE_BYTES / 1024
                ),
            });
        }
        let timeout = timeout_seconds(&params)?;
        run(self, context, code, timeout).await
    }
}

/// The run's time limit, from 1 second to the shell's most.
fn timeout_seconds(params: &HashMap<String, Value>) -> Result<u64, ToolError> {
    let invalid = || ToolError::ParameterValidation {
        param_name: "timeout_seconds".into(),
        message: format!("expected a whole number of seconds from 1 to {MAX_TIMEOUT_SECONDS}"),
    };
    let seconds = match params.get("timeout_seconds") {
        None | Some(Value::Null) => return Ok(DEFAULT_TIMEOUT_SECONDS),
        Some(value) => {
            arko_agents::tool::definition::integer_argument(value).ok_or_else(invalid)?
        }
    };
    u64::try_from(seconds)
        .ok()
        .filter(|seconds| (1..=MAX_TIMEOUT_SECONDS).contains(seconds))
        .ok_or_else(invalid)
}

#[cfg(not(feature = "code-mode"))]
async fn run(_: &RunCodeTool, _: &TaskContext<'_>, _: &str, _: u64) -> Result<String, ToolError> {
    Err(ToolError::ExecutionFailed(
        "This build of Arko has no code mode, so run_code can't run. Call the tools directly."
            .into(),
    ))
}

#[cfg(feature = "code-mode")]
async fn run(
    tool: &RunCodeTool,
    context: &TaskContext<'_>,
    code: &str,
    timeout: u64,
) -> Result<String, ToolError> {
    use super::child::{self, Runner};
    use super::protocol::{Limits, Start};
    let failed = |text: String| ToolError::ExecutionFailed(text);
    let nested = context.nested.as_ref().ok_or_else(|| {
        failed("run_code runs only where its calls can go through the agent's checks.".into())
    })?;
    let runner = Runner::this_program().map_err(|error| {
        failed(format!(
            "run_code can't start its code runner here, as {error}."
        ))
    })?;
    let start = Start {
        code: code.to_string(),
        tools: tool
            .callable
            .iter()
            .filter(|name| nested.may_call(name))
            .map(|name| name.to_string())
            .collect(),
        limits: Limits {
            compute_ms: super::COMPUTE_SECONDS * 1000,
            heap_bytes: HEAP_BYTES,
            stack_bytes: super::STACK_BYTES,
            calls: MAX_CALLS,
            os_memory_bytes: super::OS_MEMORY_BYTES,
            os_address_space_bytes: super::OS_ADDRESS_SPACE_BYTES,
            cpu_seconds: super::CPU_SECONDS,
        },
    };
    let run = child::run(
        &runner,
        start,
        nested,
        std::time::Duration::from_secs(timeout),
        &context.stop,
    )
    .await;
    log::info!(
        "Task {}: run_code made {} calls in {:?}, holding at most {} bytes of them",
        context.task_id,
        run.calls,
        run.took,
        run.most_held
    );
    words::result(&run, timeout).map_err(failed)
}

/// How a run's end reads to the model.
#[cfg(feature = "code-mode")]
pub(super) mod words {
    use super::super::child::{Changes, Outcome, Run};
    use super::super::protocol::{Done, Ended};
    use super::super::{COMPUTE_SECONDS, HEAP_BYTES, MAX_CALLS, MAX_REFUSALS, MAX_RESULT_CHARS};
    use super::thousands;

    /// The result of `run`, or with `Err` the error result.
    pub fn result(run: &Run, timeout: u64) -> Result<String, String> {
        let done = match &run.outcome {
            Outcome::Done(done) => done,
            other => return Err(failure(other, &run.changes, timeout)),
        };
        match &done.ended {
            Ended::Returned { value, unread } => {
                let mut lines = log_lines(done);
                if let Some(value) = value {
                    lines.push(value.clone());
                }
                if !unread.is_empty() {
                    let calls = if unread.len() == 1 {
                        "A call the code didn't await failed".to_string()
                    } else {
                        format!("{} calls the code didn't await failed", unread.len())
                    };
                    lines.push(format!("{calls}:"));
                    lines.extend(unread.iter().cloned());
                }
                if lines.is_empty() {
                    return Ok("The code returned nothing.".into());
                }
                Ok(cut(&lines.join("\n"), done.value_left_out))
            }
            ended => {
                let mut text = ended_words(ended);
                if let Some(changed) = changed(&run.changes) {
                    text.push(' ');
                    text.push_str(&changed);
                }
                let lines = log_lines(done);
                if !lines.is_empty() {
                    text.push_str("\nIts log so far:\n");
                    text.push_str(&lines.join("\n"));
                }
                Err(cut(&text, 0))
            }
        }
    }

    /// The log, with a line for what was left out.
    fn log_lines(done: &Done) -> Vec<String> {
        let mut lines = done.log.clone();
        if done.log_left_out > 0 {
            lines.push(format!(
                "[{} more log lines left out]",
                thousands(usize::try_from(done.log_left_out).unwrap_or(usize::MAX))
            ));
        }
        lines
    }

    fn ended_words(ended: &Ended) -> String {
        match ended {
            Ended::Returned { .. } => String::new(),
            Ended::Threw {
                message,
                line,
                compiling: true,
                ..
            } => format!(
                "The code has a syntax error{} ({message}). It runs as JavaScript.",
                on_line(*line)
            ),
            Ended::Threw {
                message,
                line,
                refused,
                tool,
                ..
            } => {
                let call = match (tool, refused) {
                    (Some(tool), true) => format!(", at a call of {tool} that was refused"),
                    (Some(tool), false) => format!(", at a call of {tool} that failed"),
                    (None, _) => String::new(),
                };
                format!("The code threw{}{call}: {message}", on_line(*line))
            }
            Ended::OutOfMemory { line } => format!(
                "The code ran out of memory{}. A run may hold {} MB.",
                on_line(*line),
                HEAP_BYTES / (1024 * 1024)
            ),
            Ended::ComputeLimit => format!(
                "The code computed for {COMPUTE_SECONDS} seconds, the most a run may, and was stopped."
            ),
            Ended::CallLimit => format!(
                "The code made more than {MAX_CALLS} tool calls, the most a run may make, and was stopped."
            ),
            Ended::Unsettled => {
                "The code waits on a promise that nothing settles, so the run ended.".into()
            }
            Ended::Failed { message } => format!("The code runner failed. {message}"),
        }
    }

    fn on_line(line: Option<u32>) -> String {
        line.map(|line| format!(" on line {line}"))
            .unwrap_or_default()
    }

    fn failure(outcome: &Outcome, changes: &Changes, timeout: u64) -> String {
        let mut text = match outcome {
            Outcome::Done(_) => String::new(),
            Outcome::TimedOut => {
                format!("The run took longer than timeout_seconds ({timeout} s) and was stopped.")
            }
            Outcome::Refusals { last } => format!(
                "{MAX_REFUSALS} of the code's calls were refused, so the run was stopped. The last: {last}"
            ),
            Outcome::Halted { tool, message } => {
                format!("The run stopped at a call of {tool}. {message}")
            }
            Outcome::Stopped => "The run was stopped.".into(),
            Outcome::Died { status, stderr } => {
                let mut text = format!(
                    "The code runner ended before the code did ({status}). Code that builds very large values can end it."
                );
                if let Some(line) = stderr.lines().find(|line| !line.trim().is_empty()) {
                    text.push_str(&format!(" It said: {line}"));
                }
                text
            }
            Outcome::OverMemory => format!(
                "The code runner held more than {} MB, so Arko ended it.",
                super::super::OS_MEMORY_BYTES / (1024 * 1024)
            ),
            Outcome::Broken(message) => message.clone(),
        };
        if let Some(changed) = changed(changes) {
            text.push(' ');
            text.push_str(&changed);
        }
        text
    }

    /// What the run had changed before it ended, when it changed anything.
    pub fn changed(changes: &Changes) -> Option<String> {
        let mut parts = Vec::new();
        for (count, verb, noun) in [
            (changes.files_written, "wrote", "file"),
            (changes.files_edited, "edited", "file"),
            (changes.commands, "ran", "command"),
        ] {
            match count {
                0 => {}
                1 => parts.push(format!("{verb} 1 {noun}")),
                count => parts.push(format!("{verb} {count} {noun}s")),
            }
        }
        let list = match parts.as_slice() {
            [] => return None,
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        };
        Some(format!("Before it ended, the code {list}."))
    }

    /// `text` cut at the result's most characters, with a line saying how
    /// much was left out, counting the `left_out` characters the runner
    /// already cut.
    pub fn cut(text: &str, left_out: u64) -> String {
        let (kept, cut_here) = match text.char_indices().nth(MAX_RESULT_CHARS) {
            Some((end, _)) => (&text[..end], text[end..].chars().count()),
            None => (text, 0),
        };
        // (The runner's count is the child's word, so it can't overflow this.)
        let left_out = cut_here.saturating_add(usize::try_from(left_out).unwrap_or(usize::MAX));
        if left_out == 0 {
            return kept.to_string();
        }
        format!("{kept}\n[{} more characters left out]", thousands(left_out))
    }
}

/// `30000` as `30,000`.
fn thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut text = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            text.push(',');
        }
        text.push(digit);
    }
    text
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;

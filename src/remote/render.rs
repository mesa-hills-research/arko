//! How the agents' calls and results read in the feed: as the terminal
//! shows them, without its styles, with the long parts (a worker's task, a
//! multi-line command, a handover report) kept whole as details, and with
//! each result told in a few words on its call's line.

use std::collections::HashMap;
use std::sync::Arc;

use arko_agents::builtins::{
    APPLY_DIFF_TOOL_NAME, ASK_USER_TOOL_NAME, EDIT_FILE_TOOL_NAME, LIST_FILES_TOOL_NAME,
    READ_FILE_TOOL_NAME, WRITE_FILE_TOOL_NAME,
};
use arko_agents::handover::{HANDOVER_REPORT_PARAM_NAME, HANDOVER_REPORT_TOOL_NAME};
use arko_workflows::services::RUN_WORKFLOW_AGENT_TOOL_NAME;
use serde_json::Value;
use uuid::Uuid;

use super::feed::{CallStatus, Item, Kind, Level};
use crate::crew;
use crate::message;
use crate::shell::{IN_THE_BACKGROUND, SHELL_TOOL_NAME};
use crate::ui;

type Parameters = HashMap<String, Value>;

/// The lines of a command's output shown under its status.
const SHOWN_LINES: usize = 4;

pub use crate::ui::cut;

fn text<'a>(parameters: &'a Parameters, name: &str) -> &'a str {
    parameters
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn flag(parameters: &Parameters, name: &str) -> bool {
    match parameters.get(name) {
        Some(Value::Bool(value)) => *value,
        Some(Value::String(text)) => text.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// How a call of an agent (the main one or a worker) shows; `None` for
/// calls not shown (a question is shown as Arko asks it, a reply or a
/// worker's report as it ends). A call's item names its tool and keeps its
/// parameters, for an app's view of it.
pub fn call(tool: &str, parameters: &Parameters) -> Option<Item> {
    let mut item = line(tool, parameters)?;
    if item.kind == Kind::Call {
        item.tool = Some(tool.to_string());
        // (In the order of their names: a `HashMap` has none.)
        let sorted: std::collections::BTreeMap<_, _> = parameters.iter().collect();
        item.parameters = serde_json::to_string_pretty(&sorted).ok().map(Into::into);
    }
    Some(item)
}

/// A call's item, as the terminal shows it.
fn line(tool: &str, parameters: &Parameters) -> Option<Item> {
    let call = |line: String| Item::new(Kind::Call, line);
    Some(match tool {
        "complete_task" | ASK_USER_TOOL_NAME => return None,
        HANDOVER_REPORT_TOOL_NAME => call("handover report".into())
            .detail(text(parameters, HANDOVER_REPORT_PARAM_NAME).trim()),
        message::MESSAGE_USER => Item::new(
            Kind::Message,
            text(parameters, message::MESSAGE_PARAM).trim(),
        ),
        crew::START_AGENT => {
            let mut notes = Vec::new();
            if flag(parameters, "isolated") {
                notes.push("worktree".to_string());
            }
            let cache = text(parameters, crew::CACHE_PARAM).trim();
            if !cache.is_empty() {
                notes.push(format!("{cache} cache"));
            }
            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", notes.join(", "))
            };
            let task = text(parameters, "task").trim();
            call(format!("start worker{notes} “{}”", cut(task, 160))).detail(long(task, 160))
        }
        crew::MESSAGE_AGENT => {
            let message = text(parameters, "message").trim();
            call(format!(
                "message {} “{}”",
                text(parameters, "to").trim(),
                cut(message, 160)
            ))
            .detail(long(message, 160))
        }
        crew::WAIT_FOR_AGENTS => {
            let which = text(parameters, "runs").trim();
            call(if which.is_empty() {
                "waiting for the workers".to_string()
            } else {
                format!("waiting for {which}")
            })
        }
        crew::LIST_AGENTS => call("list workers".into()),
        crew::STOP_AGENT => call(format!(
            "stop {}{}",
            text(parameters, "run").trim(),
            if flag(parameters, "remove_workspace") {
                " and remove its worktree"
            } else {
                ""
            }
        )),
        SHELL_TOOL_NAME => {
            let cwd = text(parameters, "cwd").trim();
            let place = if cwd.is_empty() || cwd == "." {
                String::new()
            } else {
                format!("(in {cwd}) ")
            };
            let command = text(parameters, "command").trim();
            let item = call(format!("{place}{}", ui::brief(tool, parameters)));
            if command.lines().count() > 1 {
                item.detail(command)
            } else {
                item
            }
        }
        RUN_WORKFLOW_AGENT_TOOL_NAME => {
            let prompt = text(parameters, "prompt").trim();
            let resumed = if text(parameters, "resume_run").is_empty() {
                ""
            } else {
                " (resumed)"
            };
            call(format!("worker{resumed} “{}”", cut(prompt, 160))).detail(long(prompt, 160))
        }
        other => call(ui::brief(other, parameters)),
    })
}

/// The message of a `message_agent` call: the worker that gets it shows it
/// in its own transcript.
pub fn message(tool: &str, parameters: &Parameters) -> Option<String> {
    (tool == crew::MESSAGE_AGENT).then(|| text(parameters, "message").trim().to_string())
}

/// The worker a `message_agent` call's result says got the message: "Sent:
/// r1 reads it at its next step." or "Sent to r2." (not the lead's "Sent.
/// Your lead's answer ...").
pub fn sent_to(output: &str) -> Option<&str> {
    let rest = output
        .strip_prefix("Sent: ")
        .or_else(|| output.strip_prefix("Sent to "))?;
    let id = rest.split_whitespace().next()?.trim_end_matches('.');
    (!id.is_empty()).then_some(id)
}

/// `text` when `cut` would shorten it: the detail of a line that shows
/// only its start.
fn long(text: &str, limit: usize) -> String {
    if cut(text, limit) == text {
        String::new()
    } else {
        text.to_string()
    }
}

/// How the line of a call that moved to the background starts, before
/// its job's first characters: `in the background · job 4d2c1a9b`.
pub const MOVED_OUTCOME: &str = "in the background · job ";

/// The job a call's result says it moved to, when it moved to the
/// background (`in the background as job 4d2c…, 95.3 s`).
pub fn moved_job(output: &str) -> Option<Uuid> {
    output
        .lines()
        .next()?
        .strip_prefix(IN_THE_BACKGROUND)?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// A moved call's line: `in the background · job 4d2c1a9b`.
fn moved_outcome(job: Uuid) -> String {
    let job = job.to_string();
    format!("{MOVED_OUTCOME}{}", &job[..8])
}

/// How long something ran, in a few words: `40 s`, `4 min`, `2 h 5 min`.
pub fn duration_words(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min", seconds / 60),
        _ if seconds % 3600 < 60 => format!("{} h", seconds / 3600),
        _ => format!("{} h {} min", seconds / 3600, seconds % 3600 / 60),
    }
}

/// What became of a call, from its result.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub status: CallStatus,
    /// A few words for the call's own line: "12 passed", "exit 1", "r2".
    pub text: Option<String>,
    /// A block to show under the call: a command's first lines of output.
    pub block: Option<Item>,
    /// The call's whole result (or why it failed), for an app's view of it
    /// (the call item's `output`). A command's is its block's `whole`.
    pub output: Option<Arc<str>>,
    /// The job the call moved to, when it moved to the background.
    pub job: Option<Uuid>,
}

impl Outcome {
    fn ok(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            status: CallStatus::Ok,
            text: (!text.trim().is_empty()).then_some(text),
            block: None,
            output: None,
            job: None,
        }
    }

    /// A call that failed with `message` (an error, a rejection): its
    /// first words on the call's line, and the whole of a longer message
    /// as a block under it.
    pub fn failed(message: &str) -> Self {
        // arko's "Tool 'read_file' execution failed: I/O error during tool
        // execution for tool 'read_file': ..." names what the line already
        // shows.
        let mut message = message.trim();
        if let Some((_, why)) = message
            .strip_prefix("Tool '")
            .and_then(|rest| rest.split_once("' execution failed: "))
        {
            message = why.trim();
        }
        if let Some((_, why)) = message
            .split_once(" for tool '")
            .and_then(|(_, rest)| rest.split_once("': "))
        {
            message = why.trim();
        }
        let text = cut(message.lines().next().unwrap_or_default(), OUTCOME_CHARS);
        let block = (message != text)
            .then(|| Item::new(Kind::Result, clip(message, 600)).level(Level::Error));
        Self {
            status: CallStatus::Failed,
            text: (!text.is_empty()).then_some(text),
            block,
            output: (!message.is_empty()).then(|| message.into()),
            job: None,
        }
    }
}

/// `text`, at most `limit` characters of it, with its line breaks.
fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// The longest outcome on a call's line, in characters.
const OUTCOME_CHARS: usize = 60;

/// What the result `output` of a call of `tool` says, in brief, with the
/// whole of it for an app's view of the call.
pub fn outcome(tool: &str, output: &str) -> Outcome {
    let job = moved_job(output);
    if tool != SHELL_TOOL_NAME
        && let Some(job) = job
    {
        // Any tool's call that moved, as the shell's line says it.
        let mut outcome = Outcome::ok(moved_outcome(job));
        outcome.output = Some(output.into());
        outcome.job = Some(job);
        return outcome;
    }
    let mut outcome = brief(tool, output);
    outcome.job = job;
    if tool != SHELL_TOOL_NAME && outcome.output.is_none() && !output.trim().is_empty() {
        outcome.output = Some(output.into());
    }
    outcome
}

/// What the result `output` of a call of `tool` says, in brief.
fn brief(tool: &str, output: &str) -> Outcome {
    let lines = || {
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
    };
    let plural = |count: usize, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    match tool {
        SHELL_TOOL_NAME => command_outcome(output),
        READ_FILE_TOOL_NAME => Outcome::ok(plural(lines(), "line", "lines")),
        LIST_FILES_TOOL_NAME => Outcome::ok(plural(lines(), "entry", "entries")),
        WRITE_FILE_TOOL_NAME | HANDOVER_REPORT_TOOL_NAME => Outcome::ok(""),
        // "Edited a.rs at lines 3-5: ..." or "Replaced 4 occurrences in
        // a.rs, at lines ...".
        EDIT_FILE_TOOL_NAME => match output.strip_prefix("Replaced ").and_then(|rest| {
            rest.split_whitespace()
                .next()
                .and_then(|count| count.parse::<usize>().ok())
        }) {
            Some(count) => Outcome::ok(plural(count, "replacement", "replacements")),
            None if output.starts_with("Edited ") => Outcome::ok("1 edit"),
            None => Outcome::ok(cut(
                output.lines().next().unwrap_or_default(),
                OUTCOME_CHARS,
            )),
        },
        // In sessions saved before edit_file.
        APPLY_DIFF_TOOL_NAME => {
            // "Successfully applied all 2 diff block(s) ..." or "Applied 1
            // of 2 diff block(s) ...".
            let first = output.lines().next().unwrap_or_default();
            let numbers: Vec<usize> = first
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|word| word.parse().ok())
                .collect();
            match numbers.as_slice() {
                [all] if first.starts_with("Successfully") => {
                    Outcome::ok(plural(*all, "edit", "edits"))
                }
                [done, all, ..] if first.starts_with("Applied") => Outcome {
                    status: CallStatus::Failed,
                    text: Some(format!("{done} of {all} edits")),
                    block: Some(
                        Item::new(Kind::Result, cut(first, 160))
                            .detail(output.lines().skip(1).collect::<Vec<_>>().join("\n"))
                            .level(Level::Warn),
                    ),
                    output: None,
                    job: None,
                },
                _ => Outcome::ok(cut(first, OUTCOME_CHARS)),
            }
        }
        crew::START_AGENT => {
            // "Started r2 (agent `worker`)." or "Queued r2 ...".
            let mut words = output.split_whitespace();
            match (words.next(), words.next()) {
                (Some("Started"), Some(id)) => Outcome::ok(id),
                (Some("Queued"), Some(id)) => Outcome::ok(format!("{id} queued")),
                _ => Outcome::ok(cut(output, OUTCOME_CHARS)),
            }
        }
        crew::MESSAGE_AGENT if output.starts_with("Sent") => Outcome::ok("sent"),
        // Its result is the mail that came: "[mail 2] r1 ... finished ...".
        crew::WAIT_FOR_AGENTS => {
            let mail = output
                .lines()
                .filter(|line| line.starts_with("[mail "))
                .count();
            if mail > 0 {
                Outcome::ok(plural(mail, "message", "messages"))
            } else {
                Outcome::ok(cut(
                    output.lines().next().unwrap_or_default(),
                    OUTCOME_CHARS,
                ))
            }
        }
        RUN_WORKFLOW_AGENT_TOOL_NAME => {
            Outcome::ok(format!("reported ({})", plural(lines(), "line", "lines")))
        }
        _ => Outcome::ok(
            output
                .lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| cut(line, OUTCOME_CHARS))
                .unwrap_or_default(),
        ),
    }
}

/// A shell command's outcome: its tests' counts when it ran tests, else
/// its time, or how it failed; with its first lines of output as a block
/// (and its whole output, when the block leaves out any of it: lines, or
/// the end of a long line).
fn command_outcome(output: &str) -> Outcome {
    let mut lines = output.lines();
    let status = lines.next().unwrap_or_default();
    let mut rest: Vec<&str> = lines.collect();
    // "in the background as job 1f0c…, 600.0 s", then what the agent can do
    // with it (left out), then the output so far.
    let job = status
        .strip_prefix(IN_THE_BACKGROUND)
        .and_then(|rest| rest.split(',').next());
    if job.is_some() && !rest.is_empty() {
        rest.remove(0);
    }
    let ok = status.starts_with("exit code 0,") || job.is_some();
    // "exit code 1, 2.0 s", "cancelled, 0.4 s", "stopped after 60 s (timeout_seconds), 60.0 s"
    let (how, time) = status.rsplit_once(", ").unwrap_or((status, ""));
    let tests = tests(&rest);
    let text = if let Some(job) = job {
        format!("{MOVED_OUTCOME}{}", job.get(..8).unwrap_or(job))
    } else if ok {
        tests.unwrap_or_else(|| time.to_string())
    } else {
        let how = match how.strip_prefix("exit code ") {
            Some(code) => format!("exit {code}"),
            None => how.to_string(),
        };
        match tests {
            Some(tests) => format!("{how} · {tests}"),
            None => how,
        }
    };
    let mut shown: Vec<String> = Vec::new();
    let mut left_out = rest.len() > SHOWN_LINES;
    if !(rest.len() == 1 && rest[0] == "(no output)") {
        shown.extend(rest.iter().take(SHOWN_LINES).map(|line| {
            let shown = cut(line, 300);
            left_out |= shown != *line;
            shown
        }));
        if rest.len() > SHOWN_LINES {
            shown.push(format!("… {} more lines", rest.len() - SHOWN_LINES));
        }
    }
    let block = (!shown.is_empty()).then(|| {
        let mut block = Item::new(Kind::Result, status).detail(shown.join("\n"));
        if left_out {
            block.whole = Some(rest.join("\n").into());
        }
        if ok { block } else { block.level(Level::Warn) }
    });
    Outcome {
        status: if ok {
            CallStatus::Ok
        } else {
            CallStatus::Failed
        },
        text: Some(cut(&text, OUTCOME_CHARS)).filter(|text| !text.is_empty()),
        block,
        output: None,
        job: None,
    }
}

/// The counts of a test run's summary lines (cargo's `test result:`,
/// pytest's `=== 3 passed in 0.2s ===`, jest's `Tests: 3 passed`), added
/// up: "12 passed", or "1 failed, 11 passed".
fn tests(lines: &[&str]) -> Option<String> {
    let (mut passed, mut failed, mut found) = (0usize, 0usize, false);
    for line in lines {
        let line = line.trim();
        let summary = line.starts_with("test result:")
            || line.starts_with("Tests:")
            || (line.starts_with("==") && line.ends_with("==") && line.contains(" in "));
        if !summary {
            continue;
        }
        let words: Vec<&str> = line
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect();
        for pair in words.windows(2) {
            if let Ok(count) = pair[0].parse::<usize>() {
                match pair[1] {
                    "passed" => (passed, found) = (passed + count, true),
                    "failed" => (failed, found) = (failed + count, true),
                    _ => {}
                }
            }
        }
    }
    found.then(|| {
        if failed > 0 {
            format!("{failed} failed, {passed} passed")
        } else {
            format!("{passed} passed")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, Value)]) -> Parameters {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn calls_read_as_in_the_terminal_with_their_long_parts_whole() {
        let task = "Fix the parser. ".repeat(20);
        let item = call(
            crew::START_AGENT,
            &params(&[("task", task.clone().into()), ("isolated", true.into())]),
        )
        .unwrap();
        assert!(
            item.text
                .starts_with("start worker (worktree) “Fix the parser. Fix")
        );
        assert_eq!(item.detail.as_deref(), Some(task.trim()));
        let item = call(
            SHELL_TOOL_NAME,
            &params(&[("command", "cd a\nmake".into()), ("cwd", "src".into())]),
        )
        .unwrap();
        assert_eq!(item.text, "(in src) $ cd a  (+1 lines)");
        assert_eq!(item.detail.as_deref(), Some("cd a\nmake"));
        let item = call(
            message::MESSAGE_USER,
            &params(&[("message", "Halfway.".into())]),
        )
        .unwrap();
        assert_eq!((item.kind, item.text.as_str()), (Kind::Message, "Halfway."));
        assert!(call(ASK_USER_TOOL_NAME, &params(&[])).is_none());
        let item = call(
            crew::MESSAGE_AGENT,
            &params(&[("to", "lead".into()), ("message", "Done?".into())]),
        )
        .unwrap();
        assert_eq!(item.text, "message lead “Done?”");
    }

    /// A result is told in a few words on its call's line; a command's
    /// output also comes as a block.
    #[test]
    fn results_are_told_in_a_few_words() {
        let told = |tool: &str, output: &str| {
            let outcome = outcome(tool, output);
            (outcome.status, outcome.text.unwrap_or_default())
        };
        assert_eq!(
            told("read_file", "1 | fn main() {}\n2 | }\n"),
            (CallStatus::Ok, "2 lines".into())
        );
        assert_eq!(
            told(crew::START_AGENT, "Started r2 (agent `worker`)."),
            (CallStatus::Ok, "r2".into())
        );
        assert_eq!(
            told(
                crew::WAIT_FOR_AGENTS,
                "[mail 2] r1 (agent `worker`) finished. Its report:\n\nDone.\n[mail 3] r2 (agent `worker`) finished."
            ),
            (CallStatus::Ok, "2 messages".into())
        );
        assert_eq!(
            told(
                "apply_diff",
                "Applied 1 of 2 diff block(s) to 'a.rs'.\nDetails:\n  Block 2: FAILURE"
            ),
            (CallStatus::Failed, "1 of 2 edits".into())
        );
        assert_eq!(
            told(
                EDIT_FILE_TOOL_NAME,
                "Edited a.rs at lines 2-3:\n1 | a\n2 | b\n3 | c"
            ),
            (CallStatus::Ok, "1 edit".into())
        );
        assert_eq!(
            told(
                EDIT_FILE_TOOL_NAME,
                "Replaced 4 occurrences in a.rs, at lines 1, 2 and 4."
            ),
            (CallStatus::Ok, "4 replacements".into())
        );
        assert_eq!(
            told(
                EDIT_FILE_TOOL_NAME,
                "Replaced 1 occurrence in a.rs, at line 5."
            ),
            (CallStatus::Ok, "1 replacement".into())
        );
        // How a call shows, in the feed and the full screen.
        let item = call(
            EDIT_FILE_TOOL_NAME,
            &params(&[
                ("path", "a.rs".into()),
                ("old_string", "x".into()),
                ("new_string", "y".into()),
                ("replace_all", true.into()),
            ]),
        )
        .unwrap();
        assert_eq!(item.text, "edit a.rs (all)");
        let cargo = "exit code 0, 9.1 s\nrunning 12 tests\ntest result: ok. 10 passed; 0 failed; 1 ignored\ntest result: ok. 2 passed; 0 failed";
        assert_eq!(
            told(SHELL_TOOL_NAME, cargo),
            (CallStatus::Ok, "12 passed".into())
        );
        let failed = outcome(
            SHELL_TOOL_NAME,
            "exit code 101, 3.0 s\n===== 1 failed, 4 passed in 0.31s =====",
        );
        assert_eq!(failed.status, CallStatus::Failed);
        assert_eq!(
            failed.text.as_deref(),
            Some("exit 101 · 1 failed, 4 passed")
        );
        let block = failed.block.unwrap();
        assert_eq!(
            (block.kind, block.level, block.text.as_str()),
            (Kind::Result, Some(Level::Warn), "exit code 101, 3.0 s")
        );
        assert_eq!(
            told(SHELL_TOOL_NAME, "exit code 0, 0.2 s\n(no output)"),
            (CallStatus::Ok, "0.2 s".into())
        );
        assert!(
            outcome(SHELL_TOOL_NAME, "exit code 0, 0.2 s\n(no output)")
                .block
                .is_none(),
            "no output, no block"
        );
        let failed = Outcome::failed(
            "Tool 'read_file' execution failed: I/O error during tool execution for tool 'read_file': No such file",
        );
        assert_eq!(failed.text.as_deref(), Some("No such file"));
        let failed = Outcome::failed("Tool 'x' execution failed: boom\nat line 3");
        assert_eq!(failed.text.as_deref(), Some("boom"));
        assert_eq!(failed.block.unwrap().text, "boom\nat line 3");
        assert!(Outcome::failed("short").block.is_none());
    }

    /// A command's block keeps its whole output when it shows less of it:
    /// fewer lines, or a long line's start only.
    #[test]
    fn a_commands_whole_output_is_kept_when_any_of_it_is_left_out() {
        let whole = |output: &str| {
            outcome(SHELL_TOOL_NAME, output)
                .block
                .and_then(|block| block.whole)
                .map(|whole| whole.to_string())
        };
        assert_eq!(
            whole(
                "exit code 0, 0.1 s
short
lines"
            ),
            None
        );
        let long = "x".repeat(500);
        assert_eq!(
            whole(&format!(
                "exit code 0, 0.1 s
short
{long}"
            )),
            Some(format!("short\n{long}"))
        );
        assert_eq!(
            whole(
                "exit code 0, 0.1 s
1
2
3
4
5"
            ),
            Some("1\n2\n3\n4\n5".into())
        );
    }

    /// A command moved to the background shows as such, with its output so
    /// far, not as a failure.
    #[test]
    fn a_command_moved_to_the_background() {
        let output = "in the background as job 1f0c2d3e-0000-4000-8000-000000000000, 600.0 s\nIt ran longer than 10 minutes, so it moved to the background. It goes on running.\nCompiling arko\nCompiling arko_agents";
        let moved = outcome(SHELL_TOOL_NAME, output);
        assert_eq!(moved.status, CallStatus::Ok);
        assert_eq!(
            moved.text.as_deref(),
            Some("in the background · job 1f0c2d3e")
        );
        let block = moved.block.unwrap();
        assert_eq!(block.level, None);
        assert_eq!(
            block.detail.as_deref(),
            Some("Compiling arko\nCompiling arko_agents")
        );
    }
}

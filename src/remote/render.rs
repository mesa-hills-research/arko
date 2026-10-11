//! How the agents' calls and results read on the page: as the terminal
//! shows them, without its styles, and with the long parts (a worker's
//! task, a multi-line command, a handover report) kept whole as details.

use std::collections::HashMap;

use arko_agents::builtins::ASK_USER_TOOL_NAME;
use arko_agents::handover::{HANDOVER_REPORT_PARAM_NAME, HANDOVER_REPORT_TOOL_NAME};
use arko_workflows::services::RUN_WORKFLOW_AGENT_TOOL_NAME;
use serde_json::Value;

use super::feed::{Item, Kind, Level};
use crate::crew;
use crate::message;
use crate::shell::SHELL_TOOL_NAME;
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

/// How a call of the main agent shows; `None` for calls not shown (its
/// question is shown as fin asks it, its reply as the turn ends).
pub fn call(tool: &str, parameters: &Parameters) -> Option<Item> {
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
            call(format!("start worker{notes}: {}", cut(task, 160))).detail(long(task, 160))
        }
        crew::MESSAGE_AGENT => {
            let message = text(parameters, "message").trim();
            call(format!(
                "message {}: {}",
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
            call(format!("worker{resumed}: {}", cut(prompt, 160))).detail(long(prompt, 160))
        }
        other => call(ui::brief(other, parameters)),
    })
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

/// Whether the result of the main agent's `tool` is shown.
pub fn shows_result(tool: &str) -> bool {
    !matches!(
        tool,
        "complete_task" | ASK_USER_TOOL_NAME | HANDOVER_REPORT_TOOL_NAME | crew::START_AGENT
    ) && tool != message::MESSAGE_USER
}

/// A result of the main agent's `tool`, in brief: a command's status with
/// its first lines of output, else the first line and a count of the
/// others.
pub fn result(tool: &str, output: &str) -> Option<Item> {
    if tool == SHELL_TOOL_NAME {
        let mut lines = output.lines();
        let status = lines.next().unwrap_or_default();
        let rest: Vec<&str> = lines.collect();
        let mut shown: Vec<String> = Vec::new();
        if !(rest.len() == 1 && rest[0] == "(no output)") {
            shown.extend(rest.iter().take(SHOWN_LINES).map(|line| cut(line, 300)));
            if rest.len() > SHOWN_LINES {
                shown.push(format!("… {} more lines", rest.len() - SHOWN_LINES));
            }
        }
        let item = Item::new(Kind::Result, status).detail(shown.join("\n"));
        return Some(if status.starts_with("exit code 0,") {
            item
        } else {
            item.level(Level::Warn)
        });
    }
    let first = output.lines().find(|line| !line.trim().is_empty())?;
    let count = output.lines().count();
    let more = if count > 1 && tool != RUN_WORKFLOW_AGENT_TOOL_NAME {
        format!(" ({count} lines)")
    } else {
        String::new()
    };
    Some(Item::new(
        Kind::Result,
        if tool == RUN_WORKFLOW_AGENT_TOOL_NAME {
            format!(
                "the worker reported ({count} line{})",
                if count == 1 { "" } else { "s" }
            )
        } else {
            format!("{}{more}", cut(first, 160))
        },
    ))
}

/// A worker's call, briefly, with what it says in full; `None` for calls
/// not shown (its report is shown as it finishes).
pub fn worker_call(tool: &str, parameters: &Parameters) -> Option<(String, Option<String>)> {
    match tool {
        "complete_task" => None,
        crew::MESSAGE_AGENT => Some((
            format!("message to {}", text(parameters, "to")),
            Some(text(parameters, "message").trim().to_string()),
        )),
        _ => Some((ui::brief(tool, parameters), None)),
    }
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
                .starts_with("start worker (worktree): Fix the parser.")
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
        assert_eq!(
            result("read_file", "1 | fn main() {}\n2 | }\n")
                .unwrap()
                .text,
            "1 | fn main() {} (2 lines)"
        );
        assert_eq!(
            worker_call(
                crew::MESSAGE_AGENT,
                &params(&[("to", "lead".into()), ("message", "Done?".into())])
            ),
            Some(("message to lead".into(), Some("Done?".into())))
        );
    }
}

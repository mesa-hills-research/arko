//! How the agents' calls and results read in the feed, as the terminal
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

use arko_agents::LOAD_SKILL_TOOL_NAME;
use arko_agents::NestedCallOutcome;
use arko_agents::terminal;
use arko_agents::tool::ToolReceipt;

use super::content;
use super::feed::{Act, ActKind, CallStatus, Came, ContentKind, Item, Kind};
use super::plural;
use super::protocol::CallLine;
use super::quiet;
use crate::crew;
use crate::message;
use crate::shell::{IN_THE_BACKGROUND, SHELL_TOOL_NAME};
use crate::ui;

type Parameters = HashMap<String, Value>;

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

/// A worker's title, the first line of its task that has words, without a
/// heading's or a list's marks.
pub(crate) fn worker_title(task: &str) -> String {
    let line = task
        .lines()
        .map(|line| line.trim().trim_start_matches(['#', '-', '*', ' ']))
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    cut(line, 72)
}

/// How a call of an agent (the main one or a worker) shows, and `None` for
/// calls not shown (a question is shown as Arko asks it, a reply or a
/// worker's report as it ends). A call's item names its tool and keeps its
/// parameters, for an app's view of it.
///
/// `lead` is who the calling agent's lead is ("main", or the worker that
/// started it), which a message to its lead names. What a call says comes
/// from the call and `lead` alone, so a restored session says the same.
pub fn call(tool: &str, parameters: &Parameters, lead: &str) -> Option<Item> {
    let mut item = line(tool, parameters, lead)?;
    if item.kind == Kind::Call {
        item.tool = Some(tool.to_string());
        // (In the order of their names, because a `HashMap` has none.)
        let sorted: std::collections::BTreeMap<_, _> = parameters.iter().collect();
        item.parameters = serde_json::to_string_pretty(&sorted).ok().map(Into::into);
        let act = act(tool, parameters, lead);
        if tool == crew::START_AGENT {
            // A start with no task says the call's own words ("Start a worker").
            let title = worker_title(text(parameters, "task"));
            item.task_title = Some(if title.is_empty() {
                upper_first(&act.verb)
            } else {
                title
            });
        }
        // Full's line says the call as written, and its detail is what
        // the Quiet row's says that the line doesn't.
        if item.detail.is_none() {
            let left: Vec<&str> = act
                .detail
                .lines()
                .filter(|line| !item.text.contains(line))
                .collect();
            item = item.detail(left.join("\n"));
        }
        item.act = Some(act);
    }
    Some(item)
}

/// The code a code run runs, as the block its row opens to from the start,
/// which the block of its result follows once that comes. `None` for a call
/// of any other tool.
pub fn code_block(tool: &str, parameters: &Parameters) -> Option<Item> {
    (tool == crate::code_mode::RUN_CODE)
        .then(|| {
            Content::new(
                ContentKind::Code,
                text(parameters, "code"),
                Some("js".into()),
            )
        })
        .flatten()
        .map(Content::block)
}

/// A call a code run's code started, as its line under the run says it,
/// and what it does, which the run's row says while it runs.
pub fn nested_call(tool: &str, parameters: &Parameters) -> (CallLine, Option<Act>) {
    let item = call(tool, parameters, super::feed::MAIN_AGENT);
    let text = item
        .as_ref()
        .map_or_else(|| tool.to_string(), |item| item.text.clone());
    let line = CallLine {
        text,
        status: CallStatus::Running,
        outcome: String::new(),
        failed: String::new(),
    };
    (line, item.and_then(|item| item.act))
}

/// How a call a code run's code made ended (`ended`), with its result or
/// why it failed (`output`, the first `output.len()` of its `whole` bytes).
/// Its `line` says so, and `act` (what it did) learns what the result
/// adds. Returns whether it went well.
pub fn nested_ended(
    line: &mut CallLine,
    act: &mut Option<Act>,
    tool: &str,
    parameters: &Parameters,
    ended: NestedCallOutcome,
    output: &str,
    whole: usize,
) -> bool {
    let outcome = match ended {
        NestedCallOutcome::Finished => outcome(tool, parameters, output),
        NestedCallOutcome::Refused => Outcome::failed("refused"),
        NestedCallOutcome::Stopped => Outcome::failed(quiet::STOPPED),
        NestedCallOutcome::Failed | NestedCallOutcome::Halted => Outcome::failed(output),
    };
    // Counts made from a cut result would be wrong, so a cut one says
    // nothing more than how it went.
    let cut = whole > output.len();
    let words = match outcome.status {
        CallStatus::Ok if cut => None,
        _ => outcome.text.filter(|text| !text.trim().is_empty()),
    };
    line.status = outcome.status;
    line.outcome = words.clone().unwrap_or_default();
    if !cut {
        outcome.settled.apply(act);
    }
    let failed = outcome.status == CallStatus::Failed && ended != NestedCallOutcome::Stopped;
    if failed && let Some(act) = act.as_ref() {
        let said = quiet::alone(act, outcome.status, words.as_deref()).said();
        line.failed = match words.filter(|words| !said.contains(words.as_str())) {
            Some(words) => format!("{said} · {words}"),
            None => said,
        };
    }
    outcome.status == CallStatus::Ok
}

/// `value` as text, a string as it is and anything else as JSON.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string_pretty(value).unwrap_or_default()
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The longest search query a Quiet row names, in characters.
const ROW_QUERY_CHARS: usize = 60;

/// Whether calls of `tool` edit a file in place, as their act says.
fn edits_in_place(tool: &str) -> bool {
    act(tool, &Parameters::new(), super::feed::MAIN_AGENT).kind == ActKind::Edit
}

/// What a call of `tool` does, for the Quiet view's row ([`quiet`]), with
/// what its words cut whole ([`Act::detail`]).
fn act(tool: &str, parameters: &Parameters, lead: &str) -> Act {
    // A parameter as a row names it, at most as long as Full shows a path.
    let raw = |name: &str| text(parameters, name).trim().to_string();
    let text = |name: &str| cut(&raw(name), 160);
    let on = |kind: ActKind, target: String| Act {
        kind,
        target,
        ..Act::default()
    };
    // A call of another kind, in its own words.
    let other = |doing: String, done: String, verb: String| Act {
        doing,
        done,
        verb,
        ..Act::default()
    };
    let path = |name: &str| {
        let path = ui::cut_path(&raw(name), 160);
        if path.is_empty() {
            ".".to_string()
        } else {
            path
        }
    };
    let job = || quiet::job_name(&text("job_id"));
    // (A look at a job and a wait on it keep its whole ID, by which their
    // rows merge.)
    let job_id = || text("job_id");
    match tool {
        READ_FILE_TOOL_NAME | "Read" => {
            let name = if tool == "Read" { "file_path" } else { "path" };
            on(ActKind::Read, path(name))
        }
        LIST_FILES_TOOL_NAME => on(ActKind::List, path("directory")),
        WRITE_FILE_TOOL_NAME => on(ActKind::Write, path("path")),
        "Write" => on(ActKind::Write, path("file_path")),
        EDIT_FILE_TOOL_NAME => on(ActKind::Edit, path("path")),
        // (Each block is an edit, and a call of one is like edit_file's.)
        APPLY_DIFF_TOOL_NAME => Act {
            edits: match crate::ui::diff_edits(parameters) {
                0 | 1 => 0,
                many => u32::try_from(many).unwrap_or(u32::MAX),
            },
            ..on(ActKind::Edit, path("path"))
        },
        "Edit" | "MultiEdit" => on(ActKind::Edit, path("file_path")),
        SHELL_TOOL_NAME | "Bash" => {
            let command = text_of(parameters, "command");
            let cwd = text_of(parameters, "cwd").trim();
            let words = command_words_kept(command, cwd);
            // The whole command, with the folder the words named by its
            // last part.
            let whole = command.trim();
            let detail = match words.folder {
                _ if words.kept => String::new(),
                Some(Folder::Cwd) => format!("(in {cwd}) {whole}"),
                _ => whole.to_string(),
            };
            Act {
                alone: runs_tests(&raw("command")),
                detail,
                ..on(ActKind::Command, words.words)
            }
        }
        // Its row names the indexes, and opening it shows the query once.
        "index_search" => Act {
            detail: raw("query"),
            ..on(ActKind::Search, index_name(&text("index")))
        },
        "Grep" | "Glob" => Act {
            query: cut(&text("pattern"), ROW_QUERY_CHARS),
            detail: long(&raw("pattern"), ROW_QUERY_CHARS),
            ..on(ActKind::Search, String::new())
        },
        LOAD_SKILL_TOOL_NAME => on(ActKind::Skill, text("name")),
        crew::MESSAGE_AGENT => on(ActKind::Message, addressee(&text("to"), lead)),
        // The runs it names. One that names none waits for the agent's
        // workers, which its row calls so ([`quiet`]).
        crew::WAIT_FOR_AGENTS => on(ActKind::Wait, ui::waited_runs(parameters, &[]).join(", ")),
        "terminal_status" => on(ActKind::Check, job_id()),
        "terminal_control" => match text("action").as_str() {
            "wait" => on(ActKind::JobWait, job_id()),
            "list" => other(
                "Listing jobs".into(),
                "Listed jobs".into(),
                "list jobs".into(),
            ),
            "cancel" => other(
                format!("Stopping {}", job()),
                format!("Stopped {}", job()),
                format!("stop {}", job()),
            ),
            "forget" => other(
                format!("Forgetting {}", job()),
                format!("Forgot {}", job()),
                format!("forget {}", job()),
            ),
            action => other(
                format!("Using {action} on {}", job()),
                format!("Used {action} on {}", job()),
                format!("use {action} on {}", job()),
            ),
        },
        "terminal_input" => other(
            format!("Typing into {}", job()),
            format!("Typed into {}", job()),
            format!("type into {}", job()),
        ),
        "terminal_start" => {
            let words = command_words_kept(text_of(parameters, "command"), "");
            let command = words.words;
            Act {
                detail: if words.kept {
                    String::new()
                } else {
                    raw("command")
                },
                ..other(
                    format!("Starting {command} in a terminal"),
                    format!("Started {command} in a terminal"),
                    format!("start {command} in a terminal"),
                )
            }
        }
        "monitor_start" => {
            let words = command_words_kept(text_of(parameters, "command"), "");
            let command = words.words;
            let mut detail = vec![monitor_settings(parameters)];
            if !words.kept {
                detail.insert(0, raw("command"));
            }
            Act {
                detail: detail.join("\n"),
                ..other(
                    format!("Starting to watch {command}"),
                    format!("Started watching {command}"),
                    format!("watch {command}"),
                )
            }
        }
        "monitor_control" => match text("action").as_str() {
            "list" => other(
                "Listing monitors".into(),
                "Listed monitors".into(),
                "list monitors".into(),
            ),
            "rearm" => other(
                "Keeping a monitor going".into(),
                "Kept a monitor going".into(),
                "keep a monitor going".into(),
            ),
            _ => other(
                "Stopping a monitor".into(),
                "Stopped a monitor".into(),
                "stop a monitor".into(),
            ),
        },
        "keep_note"
            if parameters
                .get("replaces")
                .is_some_and(|value| !value.is_null()) =>
        {
            let number = match parameters.get("replaces") {
                Some(Value::Number(number)) => number.to_string(),
                _ => text("replaces"),
            };
            // (Its words name the note, not its text or audience.)
            other(
                format!("Changing note {number}"),
                format!("Changed note {number}"),
                format!("change note {number}"),
            )
        }
        "keep_note" => {
            let audience = match text("audience").as_str() {
                "" | "lead" => String::new(),
                audience => audience.to_string(),
            };
            on(ActKind::Note, audience)
        }
        "forget_note" => {
            let number = match parameters.get("id") {
                Some(Value::Number(number)) => number.to_string(),
                _ => text("id"),
            };
            other(
                format!("Forgetting note {number}"),
                format!("Forgot note {number}"),
                format!("forget note {number}"),
            )
        }
        crate::mcp::MCP_TOOL_NAME => {
            let server = text("server");
            let action = text("action");
            match action.as_str() {
                "call" => on(ActKind::Mcp, format!("{} on {server}", text("tool"))),
                "servers" => other(
                    "Listing the MCP servers".into(),
                    "Listed the MCP servers".into(),
                    "list the MCP servers".into(),
                ),
                "tools" => other(
                    format!("Listing {server}’s tools"),
                    format!("Listed {server}’s tools"),
                    format!("list {server}’s tools"),
                ),
                "start" => other(
                    format!("Starting {server}"),
                    format!("Started {server}"),
                    format!("start {server}"),
                ),
                "stop" => other(
                    format!("Stopping {server}"),
                    format!("Stopped {server}"),
                    format!("stop {server}"),
                ),
                "add" => other(
                    format!("Adding the {server} server"),
                    format!("Added the {server} server"),
                    format!("add the {server} server"),
                ),
                "remove" => other(
                    format!("Removing the {server} server"),
                    format!("Removed the {server} server"),
                    format!("remove the {server} server"),
                ),
                action => other(
                    format!("Using {action} on {server}"),
                    format!("Used {action} on {server}"),
                    format!("use {action} on {server}"),
                ),
            }
        }
        crew::START_AGENT => other(
            "Starting a worker".into(),
            "Started a worker".into(),
            "start a worker".into(),
        ),
        crew::STOP_AGENT => {
            let run = text("run");
            // A worktree it removes, in every form of its words.
            let (removing, removed, remove) = if flag(parameters, "remove_workspace") {
                (
                    " and removing its worktree",
                    " and removed its worktree",
                    " and remove its worktree",
                )
            } else {
                ("", "", "")
            };
            other(
                format!("Stopping {run}{removing}"),
                format!("Stopped {run}{removed}"),
                format!("stop {run}{remove}"),
            )
        }
        crew::LIST_AGENTS => other(
            "Listing workers".into(),
            "Listed workers".into(),
            "list workers".into(),
        ),
        "link_agents" => other(
            "Linking workers".into(),
            "Linked workers".into(),
            "link workers".into(),
        ),
        crew::WORKER_OPTIONS => other(
            "Checking worker options".into(),
            "Checked worker options".into(),
            "check worker options".into(),
        ),
        crew::SAVE_WORKER_PROFILE => {
            let name = text("name");
            other(
                format!("Saving the {name} profile"),
                format!("Saved the {name} profile"),
                format!("save the {name} profile"),
            )
        }
        crate::skill_install::ADD_SKILL
        | crate::skill_install::EDIT_SKILL
        | crate::skill_install::REMOVE_SKILL => {
            let name = text("name");
            let (doing, done, verb) = match tool {
                crate::skill_install::ADD_SKILL => ("Saving", "Saved", "save"),
                crate::skill_install::EDIT_SKILL => ("Changing", "Changed", "change"),
                _ => ("Removing", "Removed", "remove"),
            };
            other(
                format!("{doing} the {name} skill"),
                format!("{done} the {name} skill"),
                format!("{verb} the {name} skill"),
            )
        }
        "close_questions" => other(
            "Closing questions".into(),
            "Closed questions".into(),
            "close questions".into(),
        ),
        HANDOVER_REPORT_TOOL_NAME => other(
            "Writing a handover report".into(),
            "Wrote a handover report".into(),
            "write a handover report".into(),
        ),
        crate::earlier::TOOL_NAME => other(
            "Reading the earlier conversation".into(),
            "Read the earlier conversation".into(),
            "read the earlier conversation".into(),
        ),
        RUN_WORKFLOW_AGENT_TOOL_NAME => other(
            "Running a worker".into(),
            "Ran a worker".into(),
            "run a worker".into(),
        ),
        "index_status" => {
            let index = index_name(&text("index"));
            let which = if index.is_empty() {
                "the indexes".to_string()
            } else {
                format!("the {index} index")
            };
            other(
                format!("Checking {which}"),
                format!("Checked {which}"),
                format!("check {which}"),
            )
        }
        "index_create" => {
            let name = index_name(&path("path"));
            other(
                format!("Indexing {name}"),
                format!("Indexed {name}"),
                format!("index {name}"),
            )
        }
        "index_manage" => {
            let index = index_name(&text("index"));
            let the = format!("the {index} index");
            let (doing, done, verb) = match text("action").as_str() {
                "rebuild" => ("Rebuilding", "Rebuilt", "rebuild"),
                "embed" => ("Embedding", "Embedded", "embed"),
                "estimate" => (
                    "Estimating embedding",
                    "Estimated embedding",
                    "estimate embedding",
                ),
                "monitor_on" => (
                    "Turning on watching for",
                    "Turned on watching for",
                    "turn on watching for",
                ),
                "monitor_off" => (
                    "Turning off watching for",
                    "Turned off watching for",
                    "turn off watching for",
                ),
                "remove" => ("Removing", "Removed", "remove"),
                _ => ("Changing", "Changed", "change"),
            };
            // (Watching and estimating name the index, not "the … index".)
            let the = if matches!(
                text("action").as_str(),
                "estimate" | "monitor_on" | "monitor_off"
            ) {
                index.clone()
            } else {
                the
            };
            other(
                format!("{doing} {the}"),
                format!("{done} {the}"),
                format!("{verb} {the}"),
            )
        }
        "WebFetch" => {
            let url = text("url");
            let host = url
                .split("://")
                .nth(1)
                .unwrap_or(&url)
                .split('/')
                .next()
                .unwrap_or_default()
                .to_string();
            Act {
                detail: if url.trim_end_matches('/').ends_with(&host) {
                    String::new()
                } else {
                    raw("url")
                },
                ..other(
                    format!("Fetching {host}"),
                    format!("Fetched {host}"),
                    format!("fetch {host}"),
                )
            }
        }
        "WebSearch" => {
            let query = format!("“{}”", cut(&text("query"), 60));
            Act {
                detail: long(&raw("query"), 60),
                ..other(
                    format!("Searching the web for {query}"),
                    format!("Searched the web for {query}"),
                    format!("search the web for {query}"),
                )
            }
        }
        "TodoWrite" => other(
            "Updating the plan".into(),
            "Updated the plan".into(),
            "update the plan".into(),
        ),
        crate::code_mode::RUN_CODE => on(ActKind::Code, text("purpose")),
        other_tool => match crate::mcp::split_tool_name(other_tool) {
            Some((server, name)) => on(ActKind::Mcp, format!("{name} on {server}")),
            None => other(
                format!("Using {other_tool}"),
                format!("Used {other_tool}"),
                format!("use {other_tool}"),
            ),
        },
    }
}

/// How a `monitor_start` call's monitor watches, as its detail says it
/// ("every 15 s, when it starts succeeding, for 30 min").
fn monitor_settings(parameters: &Parameters) -> String {
    let number = |name: &str| match parameters.get(name) {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(text)) => text.trim().parse().ok(),
        _ => None,
    };
    let mut parts = Vec::new();
    match number("every_seconds") {
        Some(every) => {
            parts.push(format!("every {every} s"));
            parts.push(
                match text(parameters, "alert_when").trim() {
                    "success" => "when it starts succeeding",
                    "failure" => "when it starts failing",
                    "output" => "when its output changes",
                    _ => "when its result changes",
                }
                .to_string(),
            );
        }
        None => match text(parameters, "pattern").trim() {
            "" => parts.push("every line".to_string()),
            pattern => parts.push(format!("lines that match {pattern}")),
        },
    }
    if flag(parameters, "once") {
        parts.push("once".to_string());
    }
    if text(parameters, "lasts").trim() == "while_workers_run" {
        parts.push("while workers run".to_string());
    } else {
        let minutes = number("expires_minutes").unwrap_or(30);
        parts.push(format!("for {minutes} min"));
    }
    parts.join(", ")
}

/// The text of the parameter `name`, as given (empty when it isn't text).
fn text_of<'a>(parameters: &'a Parameters, name: &str) -> &'a str {
    parameters
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// A command's words, with what they keep of it ([`command_words_kept`]).
struct CommandWords {
    words: String,
    /// They state the command as it is, one line with no variables, no `cd`
    /// and no program by its path.
    kept: bool,
    /// The folder they name, the `cwd`'s or the one a `cd` goes to.
    folder: Option<Folder>,
}

/// Where the folder a command's words name comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Folder {
    Cwd,
    Cd,
}

/// A command as its row names it, by its first line whole (each client
/// cuts the row to its width), with a program run by its path named by its
/// file name, and after a `cd dir &&` that becomes "in dir", as the folder
/// `cwd` does, with what the words keep of the command and which folder
/// they name.
fn command_words_kept(command: &str, cwd: &str) -> CommandWords {
    let mut line = command.trim().lines().next().unwrap_or_default().trim();
    let mut folder = match cwd.trim() {
        "" | "." => String::new(),
        cwd => last_part(cwd),
    };
    let mut from = (!folder.is_empty()).then_some(Folder::Cwd);
    // (After `&&` or `;`, whichever comes first.)
    if let Some(rest) = line.strip_prefix("cd ")
        && let Some((at, length)) = ["&&", ";"]
            .iter()
            .filter_map(|then| rest.find(then).map(|at| (at, then.len())))
            .min()
    {
        folder = last_part(rest[..at].trim().trim_matches(['"', '\'']));
        from = (!folder.is_empty()).then_some(Folder::Cd);
        line = rest[at + length..].trim();
    }
    // From its program, past the variables set for it and the shell's own
    // settings, so `CARGO_INCREMENTAL=0 cargo test` shows as `cargo test`.
    loop {
        let rest = line.trim_start();
        let skipped = rest
            .strip_prefix("set -o pipefail;")
            .or_else(|| rest.strip_prefix("set -e;"))
            .or_else(|| {
                let (word, after) = rest.split_once(char::is_whitespace)?;
                let assigns = word.contains('=') && !word.starts_with(['=', '-']);
                (assigns || word == "env" || word == "export").then_some(after)
            });
        match skipped {
            Some(after) if !after.trim().is_empty() => line = after.trim_start(),
            _ => break,
        }
    }
    // `target/debug/arko --resume x` as `arko --resume x`.
    let line = match line.split_once(char::is_whitespace) {
        Some((program, rest)) if program.contains('/') && !program.contains('=') => {
            format!("{} {rest}", last_part(program))
        }
        None if line.contains('/') && !line.contains('=') => last_part(line),
        _ => line.to_string(),
    };
    let kept = !command.is_empty() && line == command;
    let words = line;
    let words = if folder.is_empty() {
        words
    } else {
        format!("{words} in {folder}")
    };
    CommandWords {
        words,
        kept,
        folder: from,
    }
}

/// The last part of a path (`web` of `../arko/web`).
fn last_part(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_string()
}

/// An index as a call names it (its name, or a folder inside it), by name.
fn index_name(index: &str) -> String {
    if index.contains(['/', '\\']) {
        last_part(index)
    } else {
        index.to_string()
    }
}

/// A call's item, as the terminal shows it.
fn line(tool: &str, parameters: &Parameters, lead: &str) -> Option<Item> {
    let call = |line: String| Item::new(Kind::Call, line);
    if let Some((text, path)) = ui::file_line(tool, parameters) {
        let mut item = call(text);
        item.path = Some(path);
        return Some(item);
    }
    Some(match tool {
        "complete_task" | ASK_USER_TOOL_NAME => return None,
        HANDOVER_REPORT_TOOL_NAME => call("handover report".into()),
        message::MESSAGE_USER => Item::new(
            Kind::Message,
            text(parameters, message::MESSAGE_PARAM).trim(),
        ),
        crew::START_AGENT => {
            let mut notes = Vec::new();
            let profile = text(parameters, crew::PROFILE_PARAM).trim();
            if !profile.is_empty() {
                notes.push(format!("profile {profile}"));
            }
            let model = text(parameters, crew::MODEL_PARAM).trim();
            if !model.is_empty() {
                notes.push(model.to_string());
            }
            let effort = text(parameters, crew::EFFORT_PARAM).trim();
            if !effort.is_empty() {
                notes.push(format!("{effort} effort"));
            }
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
            call(format!("start worker{notes} “{}”", cut(task, 160)))
        }
        crew::MESSAGE_AGENT => {
            let message = text(parameters, "message").trim();
            call(format!(
                "message {} “{}”",
                addressee(text(parameters, "to").trim(), lead),
                cut(message, 160)
            ))
        }
        crew::WAIT_FOR_AGENTS if ui::waited_runs(parameters, &[]).is_empty() => {
            call("waiting for workers".into())
        }
        crew::WAIT_FOR_AGENTS => call(ui::waiting_for(parameters, &[])),
        crew::LIST_AGENTS => call("list workers".into()),
        crew::WORKER_OPTIONS => call("list worker options".into()),
        crew::SAVE_WORKER_PROFILE => {
            let scope = match text(parameters, "scope").trim() {
                "" => format!(" for {}", crate::profiles::Place::Project.reach()),
                scope => match crate::profiles::Place::parse(scope) {
                    Some(place) => format!(" for {}", place.reach()),
                    None => format!(" for the {scope}"),
                },
            };
            call(format!(
                "save worker profile {}{scope}",
                text(parameters, "name").trim()
            ))
        }
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
            // (The whole command, when the line cut it.)
            if item.text.contains(command) {
                item
            } else {
                item.detail(command)
            }
        }
        RUN_WORKFLOW_AGENT_TOOL_NAME => {
            let prompt = text(parameters, "prompt").trim();
            let resumed = if text(parameters, "resume_run").is_empty() {
                ""
            } else {
                " (resumed)"
            };
            call(format!("worker{resumed} “{}”", cut(prompt, 160)))
        }
        other => call(ui::brief(other, parameters)),
    })
}

/// Who a `message_agent` call's `to` is, as the user reads it: the agent's
/// lead by its name ("main", "r5"), else as it is.
fn addressee(to: &str, lead: &str) -> String {
    if to.eq_ignore_ascii_case("lead") {
        lead.to_string()
    } else {
        to.to_string()
    }
}

/// The message of a `message_agent` call. The worker that gets it shows it
/// in its own transcript.
pub fn message(tool: &str, parameters: &Parameters) -> Option<String> {
    (tool == crew::MESSAGE_AGENT).then(|| text(parameters, "message").trim().to_string())
}

/// The worker a `message_agent` call's result says got the message, as "Sent:
/// r1 reads it at its next step." or "Sent to r2." (not the lead's "Sent.
/// Your lead's answer ...").
pub fn sent_to(output: &str) -> Option<&str> {
    let rest = output
        .strip_prefix("Sent: ")
        .or_else(|| output.strip_prefix("Sent to "))?;
    let id = rest.split_whitespace().next()?.trim_end_matches('.');
    (!id.is_empty()).then_some(id)
}

/// `text` when `cut` would shorten it, meaning the detail of a line that shows
/// only its start.
fn long(text: &str, limit: usize) -> String {
    if cut(text, limit) == text {
        String::new()
    } else {
        text.to_string()
    }
}

/// How the line of a call that moved to the background starts while its
/// job runs. (A line an earlier Arko made goes on with the job's first
/// characters, as `in the background · job 4d2c1a9b`.) The line names the
/// job's state, never its ID, and follows the job ([`super::Feed::job_ended`]).
pub const MOVED_OUTCOME: &str = "in the background";

/// The line of a `terminal_start` call while the job it started runs. It
/// follows the job, and says how the job ended once it does.
pub const JOB_RUNNING: &str = "running";

/// How the line of a `monitor_start` call starts while its monitor runs, as
/// `monitoring · 3 alerts`.
pub const MONITORING: &str = "monitoring";

/// The monitor a `monitor_start` call's result says it started ("Monitor
/// 7f3e… started." or "Monitor 7f3e… ended at once, …"), and whether it
/// still runs.
fn started_monitor(output: &str) -> Option<(Uuid, bool)> {
    let rest = output.strip_prefix("Monitor ")?;
    let (id, rest) = rest.split_once(' ')?;
    Some((id.parse().ok()?, rest.starts_with("started.")))
}

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

/// The job a `terminal_start` call's result says it started, as
/// `{"job_id": "4d2c…", "state": "running"}`.
fn started_job(output: &str) -> Option<Uuid> {
    let value: serde_json::Value = serde_json::from_str(output.trim()).ok()?;
    value.get("job_id")?.as_str()?.parse().ok()
}

/// What a call of a terminal tool (`terminal_status`, `terminal_control`,
/// `terminal_input`) did, from its result, which is the tool's JSON. A look
/// at a job and a wait on it say how the job stands, and open to its output
/// when they found any. The other calls' words already say what they did,
/// and a list says how many jobs there are. `None` for any other call, and
/// for a result that isn't the tool's JSON.
fn terminal_outcome(
    tool: &str,
    parameters: &Parameters,
    output: &str,
    seen: u64,
) -> Option<Outcome> {
    let mut outcome = match (tool, text(parameters, "action")) {
        ("terminal_status", _) | ("terminal_control", "wait") => {
            let job: terminal::TerminalSnapshot = serde_json::from_str(output.trim()).ok()?;
            let wait = tool == "terminal_control";
            let found = job_found(&job, wait, seen);
            let mut outcome = Outcome::ok(found.words());
            outcome.settled.job_output_end = Some(job.output.end);
            if wait {
                outcome.settled.news = Some(found.words());
            }
            if found != JobFound::Nothing {
                outcome.block =
                    Content::new(ContentKind::Plain, job_output(&job), None).map(Content::block);
            }
            outcome
        }
        ("terminal_control", "list") => {
            let jobs: Vec<Value> = serde_json::from_str(output.trim()).ok()?;
            Outcome::ok(match jobs.len() {
                0 => "no jobs".to_string(),
                count => plural(count, "job", "jobs"),
            })
        }
        ("terminal_control", "cancel" | "forget") | ("terminal_input", _) => {
            serde_json::from_str::<serde_json::Map<String, Value>>(output.trim()).ok()?;
            Outcome::ok("")
        }
        _ => return None,
    };
    outcome.output = Some(output.into());
    Some(outcome)
}

/// What a look at a job or a wait on it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobFound {
    /// It ended, in `state` with `code` when it exited.
    Ended(terminal::JobState, Option<i32>),
    /// It asks for input.
    Input,
    /// It runs, and a wait found it printed more.
    Output,
    /// It runs, as a look found it.
    Running,
    /// A wait found it as it was.
    Nothing,
}

impl JobFound {
    fn words(self) -> String {
        match self {
            JobFound::Ended(terminal::JobState::Succeeded, _) => "finished".into(),
            JobFound::Ended(terminal::JobState::Failed, Some(code)) => {
                format!("failed (exit {code})")
            }
            JobFound::Ended(terminal::JobState::Failed, None) => "failed".into(),
            JobFound::Ended(terminal::JobState::TimedOut, _) => "timed out".into(),
            JobFound::Ended(terminal::JobState::Interrupted, _) => "interrupted".into(),
            JobFound::Ended(terminal::JobState::Cancelled | terminal::JobState::Running, _) => {
                "stopped".into()
            }
            JobFound::Input => "asks for input".into(),
            JobFound::Output => "new output".into(),
            JobFound::Running => JOB_RUNNING.into(),
            JobFound::Nothing => "nothing new".into(),
        }
    }
}

/// What `job` says of the job, for a wait (`wait`) or a look, where the
/// output the agent got of it before ends at `seen`. Only output past that
/// is new, whatever else moved the job on (an input it was given).
fn job_found(job: &terminal::TerminalSnapshot, wait: bool, seen: u64) -> JobFound {
    if job.state.is_terminal() {
        return JobFound::Ended(job.state, job.exit_code);
    }
    if job.input.is_some() {
        return JobFound::Input;
    }
    match wait {
        false => JobFound::Running,
        true if job.output.end > seen => JobFound::Output,
        true => JobFound::Nothing,
    }
}

/// How far into its job's output the agent got before a call of `tool` with
/// `parameters` that looks at the job or waits on it, from its calls
/// `earlier`: the furthest any of them got (a look may ask for earlier
/// output), and 0 when none got any. 0 for any other call.
pub(crate) fn job_seen<'a>(
    tool: &str,
    parameters: &Parameters,
    earlier: impl Iterator<Item = &'a Item>,
) -> u64 {
    let looks = tool == "terminal_status"
        || (tool == "terminal_control" && text(parameters, "action") == "wait");
    let job = text(parameters, "job_id").trim();
    if !looks || job.is_empty() {
        return 0;
    }
    earlier
        .filter_map(|item| item.act.as_ref())
        .filter(|act| matches!(act.kind, ActKind::JobWait | ActKind::Check) && act.target == job)
        .filter_map(|act| act.job_output_end)
        .max()
        .unwrap_or(0)
}

/// The output of `job` as a look or a wait got it, as plain text
/// ([`terminal_text`]). Where it starts after the job's first output or
/// ends before its last, a line says where the rest is. The prompt of a job
/// that asks for input, and why a job failed, come at the end when the
/// output doesn't say them.
fn job_output(job: &terminal::TerminalSnapshot) -> String {
    let window = &job.output;
    let printed = terminal_text(&window.text);
    // (What the job printed is checked for the prompt and the error alone.)
    let prompt = job
        .input
        .as_ref()
        .map(|input| terminal_text(&input.prompt))
        .filter(|prompt| {
            !prompt.trim().is_empty() && !printed.trim_end().ends_with(prompt.trim_end())
        });
    let error = job
        .error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty() && !printed.contains(error));
    let log = job.log_path.display();
    let mut parts = Vec::new();
    if window.start > 0 || window.gap {
        parts.push(if job.log_truncated {
            "… The earlier output wasn’t kept.".to_string()
        } else {
            format!("… The earlier output is in {log}.")
        });
    }
    parts.push(printed.clone());
    if window.end < window.available_end {
        parts.push(format!("… The later output is in {log}."));
    }
    parts.extend(prompt);
    parts.extend(error.map(str::to_string));
    parts.retain(|part| !part.is_empty());
    parts.join("\n")
}

/// `text` a job printed in a terminal as plain lines, without the terminal's
/// styles and control characters, and a line a progress bar drew again as it
/// was drawn last.
fn terminal_text(text: &str) -> String {
    super::plain(text)
        .lines()
        .map(|line| {
            line.rsplit('\r')
                .find(|part| !part.trim().is_empty())
                .unwrap_or_default()
                .chars()
                .filter(|c| *c == '\t' || !c.is_control())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How long something ran, in a few words, as `40 s`, `4 min`, `2 h 5 min`.
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
    /// A few words for the call's own line, as "12 passed", "exit 1", "r2".
    pub text: Option<String>,
    /// The call's content, as a block (a `result`) that shows under it
    /// when it's open ([`content`](super::content)).
    pub block: Option<Item>,
    /// The call's whole result (or why it failed), for an app's view of it
    /// (the call item's `output`). A command's is its block's `whole`.
    pub output: Option<Arc<str>>,
    /// The job the call moved to, when it moved to the background.
    pub job: Option<Uuid>,
    /// The worker a `start_agent` call started at once (not one that waits
    /// for a place).
    pub started: Option<String>,
    /// The worker the call set going, which a start started or queued, or
    /// a message continued or restarted ([`Item::sets`]).
    pub sets: Option<String>,
    /// What the result adds to the call's act.
    pub settled: Settled,
}

/// What a call's result adds to its act ([`Act`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settled {
    /// What its row says of how it went, replacing what it said (empty, it
    /// says nothing more).
    news: Option<String>,
    ran: bool,
    /// The indexes a search searched, as the search says.
    index: Option<String>,
    came: Option<Came>,
    /// The call's words once it ended, when the result decides them.
    done: Option<String>,
    /// The places in its file an edit changed, as its result says.
    edits: Option<usize>,
    /// Where the output a look at a job or a wait on it got ends.
    job_output_end: Option<u64>,
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
            started: None,
            sets: None,
            settled: Settled::default(),
        }
    }

    /// A call that failed with `message` (an error, a rejection), meaning its
    /// first words on the call's line ("refused" for a rejection), and the
    /// whole message as its content, muted. A stop's and a compaction's
    /// words are all there is to say.
    pub fn failed(message: &str) -> Self {
        Self::failed_call(None, message)
    }

    /// [`Outcome::failed`] for a call of `tool` whose tool failed. A failed
    /// edit's line says only [`quiet::FAILED`], since its error names the
    /// file and why each block didn't apply, which the line can't hold. Its
    /// content is the whole error.
    pub fn error(tool: &str, message: &str) -> Self {
        Self::failed_call(Some(tool), message)
    }

    fn failed_call(tool: Option<&str>, message: &str) -> Self {
        // arko's "Tool 'read_file' execution failed: I/O error during tool
        // execution for tool 'read_file': ..." names what the line already
        // shows. (Older arko_agents said "Execution failed: " again after
        // it, and sessions saved then keep that.)
        let mut message = message.trim();
        if let Some((_, why)) = message
            .strip_prefix("Tool '")
            .and_then(|rest| rest.split_once("' execution failed: "))
        {
            message = why.trim();
            message = message
                .strip_prefix("Execution failed: ")
                .unwrap_or(message);
        }
        if let Some((_, why)) = message
            .split_once(" for tool '")
            .and_then(|(_, rest)| rest.split_once("': "))
        {
            message = why.trim();
        }
        // A refusal, as the feed words it ("refused (outside access is
        // off)"), says why as a sentence of its own.
        let why = message
            .strip_prefix("refused (")
            .and_then(|rest| rest.strip_suffix(')'));
        let said_all =
            [quiet::STOPPED, quiet::HELD, quiet::INTERRUPTED, "refused"].contains(&message);
        let (text, said) = match why {
            Some(why) => (
                "refused".to_string(),
                format!("Refused. {}", upper_first(why)),
            ),
            None if !said_all && tool.is_some_and(edits_in_place) => {
                (quiet::FAILED.to_string(), message.to_string())
            }
            None => (
                cut(message.lines().next().unwrap_or_default(), OUTCOME_CHARS),
                message.to_string(),
            ),
        };
        let block = (!said_all)
            .then(|| Content::new(ContentKind::Muted, said, None))
            .flatten()
            .map(Content::block);
        Self {
            status: CallStatus::Failed,
            text: (!text.is_empty()).then_some(text),
            block,
            output: (!message.is_empty()).then(|| message.into()),
            job: None,
            started: None,
            sets: None,
            settled: Settled::default(),
        }
    }
}

impl Settled {
    /// How a job a call started (a command that went on in the background,
    /// a monitor) went on, so what its result first said no longer stands.
    pub fn job() -> Self {
        Self {
            news: Some(String::new()),
            ..Self::default()
        }
    }

    /// What the search of a call found, the indexes it searched, and
    /// "no results" when it had none.
    fn found(&mut self, indexes: &[String], hits: usize) {
        if !indexes.is_empty() {
            self.index = Some(indexes.join(", "));
        }
        if hits == 0 {
            self.news = Some("no results".into());
        }
    }

    /// Completes `act`, the act of the call whose result this is.
    pub fn apply(&self, act: &mut Option<Act>) {
        let Some(act) = act.as_mut() else {
            return;
        };
        let settled = self;
        if let Some(news) = &settled.news {
            act.news.clone_from(news);
        }
        act.ran |= settled.ran;
        if let Some(index) = &settled.index {
            act.target.clone_from(index);
        }
        if settled.came.is_some() {
            act.came.clone_from(&settled.came);
        }
        if let Some(done) = &settled.done {
            act.done.clone_from(done);
        }
        if let Some(edits) = settled.edits {
            act.edits = match edits {
                0 | 1 => 0,
                many => u32::try_from(many).unwrap_or(u32::MAX),
            };
        }
        if settled.job_output_end.is_some() {
            act.job_output_end = settled.job_output_end;
        }
    }
}

/// `text` with its first letter in upper case.
fn upper_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The longest outcome on a call's line, in characters.
const OUTCOME_CHARS: usize = 60;

/// What the result `output` of a call of `tool` with `parameters` says, in
/// brief, with its content, and the whole of it for an app's view of the
/// call.
pub fn outcome(tool: &str, parameters: &Parameters, output: &str) -> Outcome {
    outcome_after(tool, parameters, output, None, 0)
}

/// [`outcome`], with the tool's `receipt` of what the call did, which an
/// edit's diff is made from, and for a look at a job or a wait on it where
/// the output the agent got of it before ends, `seen` ([`job_seen`]).
pub(crate) fn outcome_after(
    tool: &str,
    parameters: &Parameters,
    output: &str,
    receipt: Option<&ToolReceipt>,
    seen: u64,
) -> Outcome {
    let job = moved_job(output);
    if tool != SHELL_TOOL_NAME
        && let Some(job) = job
    {
        // Any tool's call that moved, as the shell's line says it.
        let mut outcome = Outcome::ok(MOVED_OUTCOME);
        outcome.output = Some(output.into());
        outcome.job = Some(job);
        return outcome;
    }
    // Its line says how the job it started goes, not what started it.
    if tool == "terminal_start" {
        let mut outcome = Outcome::ok(JOB_RUNNING);
        outcome.output = Some(output.into());
        outcome.job = started_job(output);
        return outcome;
    }
    if tool == "monitor_start"
        && let Some((monitor, running)) = started_monitor(output)
    {
        let mut outcome = Outcome::ok(if running { MONITORING } else { "ended at once" });
        outcome.output = Some(output.into());
        outcome.job = Some(monitor);
        return outcome;
    }
    if let Some(outcome) = terminal_outcome(tool, parameters, output, seen) {
        return outcome;
    }
    let mut outcome = brief(tool, output);
    // An apply_diff call's edits are its blocks, which applied together.
    if tool == APPLY_DIFF_TOOL_NAME && output.starts_with("Edited ") {
        outcome = edits_made(crate::ui::diff_edits(parameters).max(1));
    }
    outcome.job = job;
    if tool != SHELL_TOOL_NAME && outcome.output.is_none() && !output.trim().is_empty() {
        outcome.output = Some(output.into());
    }
    if outcome.block.is_none() && tool != SHELL_TOOL_NAME {
        outcome.block = content(tool, parameters, output, receipt).map(Content::block);
    }
    outcome
}

/// The outcome of an edit that changed `count` places in its file, each
/// an edit, which its act counts too.
fn edits_made(count: usize) -> Outcome {
    let mut outcome = Outcome::ok(plural(count, "edit", "edits"));
    outcome.settled.edits = Some(count);
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
    match tool {
        SHELL_TOOL_NAME => command_outcome(output),
        // (Not the line that says which lines it read.)
        READ_FILE_TOOL_NAME => {
            let read = output
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter(|line| !(line.starts_with("[Lines ") && line.ends_with(".]")))
                .count();
            Outcome::ok(plural(read, "line", "lines"))
        }
        LIST_FILES_TOOL_NAME => Outcome::ok(plural(lines(), "entry", "entries")),
        WRITE_FILE_TOOL_NAME | HANDOVER_REPORT_TOOL_NAME => Outcome::ok(""),
        // "Edited a.rs at lines 3-5: ..." or "Replaced 4 occurrences in
        // a.rs, at lines ...", from edit_file and apply_diff alike, where
        // each place it changed is an edit. (An apply_diff call's edits are
        // counted in `outcome_after`.)
        EDIT_FILE_TOOL_NAME | APPLY_DIFF_TOOL_NAME
            if output.starts_with("Edited ") || output.starts_with("Replaced ") =>
        {
            edits_made(
                output
                    .strip_prefix("Replaced ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|count| count.parse::<usize>().ok())
                    .unwrap_or(1),
            )
        }
        EDIT_FILE_TOOL_NAME => Outcome::ok(cut(
            output.lines().next().unwrap_or_default(),
            OUTCOME_CHARS,
        )),
        // apply_diff's results in sessions saved before edit_file.
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
                    block: None,
                    output: None,
                    job: None,
                    started: None,
                    sets: None,
                    settled: Settled::default(),
                },
                _ => Outcome::ok(cut(first, OUTCOME_CHARS)),
            }
        }
        crew::START_AGENT => {
            // "Started r2 (agent `worker`)." or "Queued r2 ...".
            let mut words = output.split_whitespace();
            match (words.next(), words.next()) {
                (Some("Started"), Some(id)) => Outcome {
                    started: Some(id.to_string()),
                    sets: Some(id.to_string()),
                    ..Outcome::ok(id)
                },
                (Some("Queued"), Some(id)) => Outcome {
                    sets: Some(id.to_string()),
                    settled: Settled {
                        done: Some(format!("Queued {id}")),
                        ..Settled::default()
                    },
                    ..Outcome::ok(format!("{id} queued"))
                },
                _ => Outcome::ok(cut(output, OUTCOME_CHARS)),
            }
        }
        // shortcut: reads the first line of the search's result, which
        // one function of the search writes and reads back, until the
        // framework gives a tool's result as data.
        "index_search" => {
            let first = output.lines().next().unwrap_or_default();
            // ("6 results from arko (code, hybrid)", without the colon that
            // leads the agent to them.)
            let mut outcome = Outcome::ok(cut(first.trim_end_matches(':'), OUTCOME_CHARS));
            if let Some((indexes, hits)) = crate::index::read_search_header(first) {
                outcome.settled.found(&indexes, hits);
            }
            outcome
        }
        // (Its text is its content, so the line needs no words of it.)
        LOAD_SKILL_TOOL_NAME if skill_text(output).is_some() => Outcome::ok(""),
        // A message that sets a worker that ended going again ("Sent: r2
        // continues its task", "restarts where it stopped", "goes on with
        // your message once an agent run ends") names it, as a start does.
        // shortcut: reads the words the workflow's board writes, as the
        // start's "Started r2" is read, until the framework gives a tool's
        // result as data.
        crew::MESSAGE_AGENT if output.starts_with("Sent") => {
            let going = [
                "continues its task",
                "restarts where it stopped",
                "goes on with",
            ]
            .iter()
            .any(|words| output.contains(words));
            Outcome {
                sets: sent_to(output).filter(|_| going).map(str::to_string),
                ..Outcome::ok("sent")
            }
        }
        // Its result is the mail that came ("[mail 2] r1 ... finished ..."),
        // worded for the agent that waited ("An update for you arrived"), so
        // its line says what came as the user reads it.
        crew::WAIT_FOR_AGENTS => {
            let came = came(output);
            let mut outcome = Outcome::ok(quiet::came_words(&came));
            outcome.settled.came = Some(came);
            outcome
        }
        RUN_WORKFLOW_AGENT_TOOL_NAME => {
            Outcome::ok(format!("reported ({})", plural(lines(), "line", "lines")))
        }
        crate::code_mode::RUN_CODE => Outcome::ok(plural(lines(), "line", "lines")),
        _ => Outcome::ok(
            output
                .lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| cut(line, OUTCOME_CHARS))
                .unwrap_or_default(),
        ),
    }
}

/// A call's content before it's a block: its kind, its whole text, and the
/// language of its file's lines.
struct Content {
    kind: ContentKind,
    text: String,
    language: Option<String>,
}

impl Content {
    /// `text` as a content of `kind`, and `None` when it says nothing. A
    /// file's lines and a diff stay as they are, their blank lines and
    /// spaces too, and code as well but for the line breaks it ends with.
    /// Text loses the blank lines it starts and ends with.
    fn new(kind: ContentKind, text: impl Into<String>, language: Option<String>) -> Option<Self> {
        let text = text.into();
        let text = match kind {
            ContentKind::Lines | ContentKind::Diff => text,
            _ if text.trim().is_empty() => String::new(),
            // Code as written, but for the line breaks it ends with.
            ContentKind::Code => text.trim_end_matches(['\n', '\r']).to_string(),
            _ => {
                let end = text.trim_end_matches(['\n', '\r']);
                let start = end.len() - end.trim_start().len();
                // (From the line the first that isn't blank starts.)
                let start = end[..start].rfind('\n').map_or(0, |at| at + 1);
                end[start..].to_string()
            }
        };
        (!text.is_empty()).then_some(Self {
            kind,
            text,
            language,
        })
    }

    /// Its block, which shows its preview, and keeps the whole when the
    /// preview leaves some out.
    fn block(self) -> Item {
        let (preview, more) = content::preview(self.kind, &self.text);
        let mut block = Item::new(Kind::Result, preview);
        block.content = Some(self.kind);
        block.number_width = more
            .is_some()
            .then(|| content::number_width(self.kind, &self.text))
            .filter(|width| *width > 0);
        block.whole = more.is_some().then(|| self.text.into());
        block.more = more;
        block.language = self.language;
        block
    }
}

/// The language of the file `path`, by its extension ("rs"), or by its
/// name for a file without one ("makefile").
fn language(path: &str) -> Option<String> {
    let name = path.trim().rsplit(['/', '\\']).next()?;
    let language = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension,
        _ => name,
    };
    (!language.is_empty()).then(|| language.to_lowercase())
}

/// The content of a call of `tool` with `parameters` that ended well with
/// the result `output`, in the form that fits the tool (a command's is its
/// output, [`command_outcome`]).
fn content(
    tool: &str,
    parameters: &Parameters,
    output: &str,
    receipt: Option<&ToolReceipt>,
) -> Option<Content> {
    let given = |name: &str| text(parameters, name);
    let result = |kind: ContentKind| Content::new(kind, output, None);
    match tool {
        READ_FILE_TOOL_NAME | "Read" => {
            let path = given(if tool == "Read" { "file_path" } else { "path" });
            if output.trim().is_empty() {
                return Content::new(ContentKind::Muted, "Empty", None);
            }
            match read_lines(output, tool == "Read") {
                Some(lines) => Content::new(ContentKind::Lines, lines, language(path)),
                // (An image, which shows as one.)
                None => result(ContentKind::Muted),
            }
        }
        WRITE_FILE_TOOL_NAME | "Write" => {
            let path = given(if tool == "Write" { "file_path" } else { "path" });
            let lines: Vec<String> = given("content")
                .lines()
                .enumerate()
                .map(|(at, line)| content::numbered(at + 1, line))
                .collect();
            if lines.is_empty() {
                return Content::new(ContentKind::Muted, "Empty", None);
            }
            Content::new(ContentKind::Lines, lines.join("\n"), language(path))
        }
        EDIT_FILE_TOOL_NAME => {
            let (old, new) = (given("old_string"), given("new_string"));
            let diff = receipt
                .and_then(receipt_diff)
                .unwrap_or_else(|| bare_diff(old, new));
            Content::new(ContentKind::Diff, diff, language(given("path")))
        }
        // (A session saved before receipts shows each block's lines.)
        APPLY_DIFF_TOOL_NAME => {
            let diff = receipt.and_then(receipt_diff).unwrap_or_else(|| {
                blocks_of(given("diff"))
                    .iter()
                    .map(|(old, new)| bare_diff(old, new))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            Content::new(ContentKind::Diff, diff, language(given("path")))
        }
        "Edit" => {
            let diff = bare_diff(given("old_string"), given("new_string"));
            Content::new(ContentKind::Diff, diff, language(given("file_path")))
        }
        "MultiEdit" => {
            let edits = parameters.get("edits").and_then(Value::as_array);
            let diff: Vec<String> = edits
                .into_iter()
                .flatten()
                .map(|edit| {
                    let part = |name: &str| edit.get(name).and_then(Value::as_str).unwrap_or("");
                    bare_diff(part("old_string"), part("new_string"))
                })
                .collect();
            Content::new(
                ContentKind::Diff,
                diff.join("\n"),
                language(given("file_path")),
            )
        }
        LIST_FILES_TOOL_NAME | "Grep" | "Glob" | "Bash" => result(ContentKind::Plain),
        // (Not its first line, how many it found and where, which the
        // call's line says.)
        "index_search" => {
            let hits = crate::index::read_search_hits(output)?;
            Content::new(ContentKind::Plain, hits[1..].join("\n"), None)
        }
        LOAD_SKILL_TOOL_NAME => match skill_text(output) {
            Some(skill) => Content::new(ContentKind::Plain, skill, None),
            None => result(ContentKind::Muted),
        },
        "monitor_start" | "terminal_start" => None,
        crew::MESSAGE_AGENT => Content::new(ContentKind::Plain, given("message").trim(), None),
        // (Nothing came, which its row says.)
        crew::WAIT_FOR_AGENTS if came(output) == Came::default() => None,
        crew::WAIT_FOR_AGENTS => Content::new(ContentKind::Plain, mail(output), None),
        crew::START_AGENT => Content::new(ContentKind::Muted, given("task").trim(), None),
        "keep_note" => Content::new(ContentKind::Plain, given("text").trim(), None),
        HANDOVER_REPORT_TOOL_NAME => Content::new(
            ContentKind::Plain,
            given(HANDOVER_REPORT_PARAM_NAME).trim(),
            None,
        ),
        crate::mcp::MCP_TOOL_NAME if given("action") == "call" => {
            let arguments = parameters
                .get("arguments")
                .map(json_text)
                .unwrap_or_default();
            Content::new(ContentKind::Plain, arguments_then(&arguments, output), None)
        }
        other => match crate::mcp::split_tool_name(other) {
            Some(_) => {
                let arguments = json_text(&Value::Object(
                    parameters
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect(),
                ));
                Content::new(ContentKind::Plain, arguments_then(&arguments, output), None)
            }
            None => result(ContentKind::Muted),
        },
    }
}

/// The text of the skill a call of load_skill loaded, without the lines that
/// mark its start and end for the agent. `None` when it loaded none.
// shortcut: reads the lines load_skill writes around a skill, until the
// framework gives a tool's result as data.
fn skill_text(output: &str) -> Option<&str> {
    let (begin, rest) = output.split_once('\n')?;
    if !begin.starts_with("--- BEGIN SKILL") {
        return None;
    }
    let end = rest.rfind("--- END SKILL")?;
    Some(&rest[..end])
}

/// An MCP call's `arguments`, then its result.
fn arguments_then(arguments: &str, output: &str) -> String {
    match (arguments.trim(), output.trim()) {
        ("" | "{}", output) => output.to_string(),
        (arguments, "") => arguments.to_string(),
        (arguments, output) => format!("{arguments}\n{output}"),
    }
}

/// A value as JSON, pretty, and a text of JSON the same way.
fn json_text(value: &Value) -> String {
    match value {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(parsed @ (Value::Object(_) | Value::Array(_))) => value_text(&parsed),
            _ => text.clone(),
        },
        other => value_text(other),
    }
}

/// A read's lines, numbered, from its result: `read_file`'s `12 | text`
/// lines (`claude_code`, Claude Code's `Read`, numbers its lines `12→text`).
/// The line that says which lines it read goes, since the call's line says
/// it. `None` for a result without numbered lines (an image).
fn read_lines(output: &str, claude_code: bool) -> Option<String> {
    let mut lines = Vec::new();
    let mut numbered = false;
    for line in output.lines() {
        let split = if claude_code {
            line.trim_start().split_once('→')
        } else {
            line.split_once(" | ")
        };
        match split
            .filter(|(number, _)| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
        {
            Some((number, text)) => {
                numbered = true;
                lines.push(format!("{number}\t{text}"));
            }
            // ("[Lines 3-4 of 10.]", and Claude Code's reminders.)
            None if claude_code || (line.starts_with("[Lines ") && line.ends_with(".]")) => {}
            None => lines.push(line.to_string()),
        }
    }
    numbered.then(|| lines.join("\n"))
}

/// What a diff says after its lines when the edit's receipt left some out.
const MORE_CHANGED: &str = "The edit changed more than this shows.";

/// The diff of an edit from the tool's receipt of it, its hunks with "…"
/// between them, each line numbered as the file has it now, a removed one
/// as the file had it.
fn receipt_diff(receipt: &ToolReceipt) -> Option<String> {
    use arko_agents::tool::LineChange;
    use content::Mark;
    let ToolReceipt::Edit(receipt) = receipt else {
        return None;
    };
    if receipt.hunks.is_empty() {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    for hunk in &receipt.hunks {
        if !out.is_empty() {
            out.push("…".to_string());
        }
        let (mut old, mut new) = (hunk.old_start, hunk.new_start);
        for line in &hunk.lines {
            let (mark, number) = match line.change {
                LineChange::Kept => (Mark::Kept, new),
                LineChange::Removed => (Mark::Removed, old),
                LineChange::Added => (Mark::Added, new),
            };
            out.push(content::diff_line(mark, Some(number), &line.text));
            if line.change != LineChange::Added {
                old += 1;
            }
            if line.change != LineChange::Removed {
                new += 1;
            }
        }
    }
    if receipt.truncated {
        out.push(MORE_CHANGED.to_string());
    }
    Some(out.join("\n"))
}

/// The search and replace lines of each block of an `apply_diff` call's
/// `diff`, as the tool reads them (a marker with a backslash before it is a
/// line of the file), for a call without a receipt.
fn blocks_of(diff: &str) -> Vec<(String, String)> {
    #[derive(Clone, Copy)]
    enum Part {
        Outside,
        Search,
        Replace,
    }
    const MARKERS: [&str; 3] = ["<<<<<<< SEARCH", "=======", ">>>>>>> REPLACE"];
    let is = |line: &str, marker: &str| line.trim_end() == marker;
    let text = |line: &str| match line.strip_prefix('\\') {
        Some(rest) if MARKERS.contains(&rest.trim_start_matches('\\').trim_end()) => {
            rest.to_string()
        }
        // (Before the backslash, a marker was written after `AD_ESC::`,
        // anywhere in a line.)
        _ => [
            MARKERS[0],
            MARKERS[1],
            MARKERS[2],
            "-------",
            ":start_line:",
        ]
        .iter()
        .fold(line.to_string(), |line, marker| {
            line.replace(&format!("AD_ESC::{marker}"), marker)
        }),
    };
    let (mut blocks, mut part) = (Vec::new(), Part::Outside);
    let (mut search, mut replace): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    for line in diff.lines() {
        match part {
            Part::Outside => {
                if is(line, MARKERS[0]) {
                    (search, replace) = (Vec::new(), Vec::new());
                    part = Part::Search;
                }
            }
            Part::Search if is(line, MARKERS[1]) => {
                // (The old form's header, `:start_line:` and `-------`.)
                if search.len() >= 2
                    && search[0].trim().starts_with(":start_line:")
                    && search[1].trim_end() == "-------"
                {
                    search.drain(..2);
                }
                part = Part::Replace;
            }
            Part::Search => search.push(text(line)),
            Part::Replace if is(line, MARKERS[2]) => {
                blocks.push((search.join("\n"), replace.join("\n")));
                part = Part::Outside;
            }
            Part::Replace => replace.push(text(line)),
        }
    }
    blocks
}

/// The diff of an edit without the tool's receipt (a restored session,
/// Claude Code's edits), which is its old text removed and its new text
/// added, without numbers.
fn bare_diff(old: &str, new: &str) -> String {
    use content::Mark;
    let removed = old
        .lines()
        .map(|line| content::diff_line(Mark::Removed, None, line));
    let added = new
        .lines()
        .map(|line| content::diff_line(Mark::Added, None, line));
    removed.chain(added).collect::<Vec<_>>().join("\n")
}

/// What came while a wait waited, from its result, as the user reads it:
/// each item of mail by its heading, from the user's side ("r3 finished.
/// Its report:"), and its text, without the markers that set it apart for
/// the agent.
// shortcut: reads the framework's markers and headings, as `came` does.
fn mail(output: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut inside: Option<&str> = None;
    for line in output.lines() {
        if let Some(number) = inside
            && line.trim() == format!("[end of mail {number}]")
        {
            inside = None;
            continue;
        }
        if inside.is_none()
            && let Some((number, heading)) = line
                .strip_prefix("[mail ")
                .and_then(|rest| rest.split_once("] "))
        {
            inside = Some(number);
            // "r1 (agent `worker`) finished" as "r1 finished".
            let heading = match heading.split_once(" (agent `") {
                Some((run, rest)) => match rest.split_once("`)") {
                    Some((_, rest)) => format!("{run}{rest}"),
                    None => heading.to_string(),
                },
                None => heading.to_string(),
            };
            out.push(heading.replace(" sent you a message", " sent a message"));
            continue;
        }
        // (Two blank lines read as one.)
        if line.trim().is_empty() && out.last().is_some_and(|last| last.trim().is_empty()) {
            continue;
        }
        // What woke the wait, without what it tells the agent to do.
        if inside.is_none() && line.starts_with("An update for you arrived") {
            out.push("An update came.".to_string());
            continue;
        }
        if inside.is_none() && line.starts_with("A note for you arrived") {
            out.push("A note came.".to_string());
            continue;
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

/// What reached a wait, from its result. That is the mail it holds, each
/// item between its `[mail N]` and `[end of mail N]` markers and known by
/// its heading ("r1 (agent `worker`) finished. Its report:"), and the
/// update (a note, a terminal update, a monitor's alert) that ended it
/// early. A report that quotes mail inside it counts once.
// shortcut: reads the framework's markers and headings, until
// wait_for_agents gives the mail it returns as data.
fn came(output: &str) -> Came {
    let mut came = Came::default();
    let first = output.lines().next().unwrap_or_default();
    if first.starts_with("An update for you arrived") || first.starts_with("A note for you arrived")
    {
        came.updates += 1;
    }
    // The item being read, by its number.
    let mut inside: Option<&str> = None;
    for line in output.lines() {
        if let Some(number) = inside {
            if line.trim() == format!("[end of mail {number}]") {
                inside = None;
            }
            continue;
        }
        let Some((number, heading)) = line
            .strip_prefix("[mail ")
            .and_then(|rest| rest.split_once("] "))
        else {
            continue;
        };
        inside = Some(number);
        let run = heading.split_whitespace().next().unwrap_or_default();
        if heading.contains(") finished. Its report") {
            came.finished.push(run.to_string());
        } else if heading.contains(") stopped ") {
            came.stopped.push(run.to_string());
        } else {
            came.messages += 1;
        }
    }
    came
}

/// Whether `command` runs tests (`cargo test`, `go test`, `pytest`, `npm
/// test` and the like), whose run has a row of its own in the Quiet view
/// whatever it prints. A runner's own options and a toolchain before its
/// subcommand (`cargo +nightly --locked test`, `go -C dir test`, `npm
/// --prefix web test`) don't hide it.
fn runs_tests(command: &str) -> bool {
    let words: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|word| !word.is_empty())
        .map(|word| word.rsplit('/').next().unwrap_or(word))
        .collect();
    let alone = [
        "pytest", "jest", "vitest", "mocha", "rspec", "phpunit", "ctest", "tox", "nox",
    ];
    if words.iter().any(|word| alone.contains(word)) {
        return true;
    }
    // The options that take a value as the next word.
    let valued = [
        "-C",
        "-Z",
        "-F",
        "-f",
        "--prefix",
        "--cwd",
        "--dir",
        "--directory",
        "--manifest-path",
        "--config",
        "--color",
        "--filter",
        "--workdir",
    ];
    // The runner's subcommands after `at`, past its options and toolchain.
    let subcommands = |at: usize| {
        let mut rest = words[at + 1..].iter();
        let mut found = Vec::new();
        while let Some(word) = rest.next() {
            if word.starts_with('+') {
                continue;
            }
            if word.starts_with('-') {
                if valued.contains(word) {
                    rest.next();
                }
                continue;
            }
            found.push(*word);
            if found.len() == 2 {
                break;
            }
        }
        found
    };
    words.iter().enumerate().any(|(at, word)| {
        let then = || subcommands(at);
        match *word {
            "cargo" => matches!(then().first(), Some(&("test" | "nextest"))),
            "npm" | "pnpm" | "yarn" | "bun" => {
                matches!(then().as_slice(), ["test", ..] | ["run", "test"])
            }
            "go" | "deno" | "dotnet" | "mvn" | "gradle" | "gradlew" | "swift" | "mix" => {
                then().first() == Some(&"test")
            }
            "make" => matches!(then().first(), Some(&("test" | "check"))),
            "zig" => matches!(then().as_slice(), ["test", ..] | ["build", "test"]),
            "python" | "python3" => {
                words.get(at + 1) == Some(&"-m")
                    && matches!(words.get(at + 2), Some(&("pytest" | "unittest")))
            }
            _ => false,
        }
    })
}

/// A shell command's outcome, meaning its tests' counts when it ran tests, else
/// its time, or how it failed, with its first lines of output as a block
/// (and its whole output, when the block leaves out any of it, meaning lines, or
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
    // Its row says how its tests went, and that it ran. (A ✗ says it
    // failed.)
    let settled = Settled {
        news: Some(tests.clone().unwrap_or_default()),
        ran: true,
        ..Settled::default()
    };
    let text = if job.is_some() {
        MOVED_OUTCOME.to_string()
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
    // Its content is its output.
    let block = if rest.len() == 1 && rest[0] == "(no output)" {
        None
    } else {
        Content::new(ContentKind::Plain, rest.join("\n"), None).map(Content::block)
    };
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
        started: None,
        sets: None,
        settled,
    }
}

/// The counts of a test run's summary lines (cargo's `test result:`,
/// pytest's `=== 3 passed in 0.2s ===`, jest's `Tests: 3 passed`), added
/// up, as "12 passed", or "1 failed, 11 passed".
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

    /// A call of an agent with no runs going.
    fn call(tool: &str, parameters: &Parameters) -> Option<Item> {
        super::call(tool, parameters, "main")
    }

    /// What a result of a call without parameters says.
    fn outcome(tool: &str, output: &str) -> Outcome {
        super::outcome(tool, &Parameters::new(), output)
    }

    /// Each call of the shared fixtures ([`quiet::fixtures`]) makes their
    /// item, its act once its result came, and its content: a read's lines
    /// numbered, an edit's diff, a command's output, a message's text, a
    /// search's hits, the mail a wait got, any other tool's result muted.
    #[test]
    fn calls_make_the_fixtures_items_and_contents() {
        for fixture in quiet::fixtures() {
            let name = fixture.name;
            let parameters: Parameters = serde_json::from_str(fixture.parameters).unwrap();
            let mut item = super::call(fixture.tool, &parameters, "main").unwrap();
            assert_eq!(item.text, fixture.full, "{name}");
            assert_eq!(item.detail.as_deref(), fixture.full_detail, "{name}");
            let outcome = if fixture.output.starts_with("refused") {
                Outcome::failed(fixture.output)
            } else if fixture.status == CallStatus::Failed && fixture.tool != SHELL_TOOL_NAME {
                Outcome::error(fixture.tool, fixture.output)
            } else {
                let receipt = fixture.before.map(|before| {
                    let given = |name: &str| text(&parameters, name);
                    let all = flag(&parameters, "replace_all");
                    match fixture.tool {
                        APPLY_DIFF_TOOL_NAME => applied(before, given("diff"), all),
                        _ => edited(before, given("old_string"), given("new_string"), all),
                    }
                });
                outcome_after(
                    fixture.tool,
                    &parameters,
                    fixture.output,
                    receipt.as_ref(),
                    0,
                )
            };
            // (Its result comes before the item 2.)
            item.settle(outcome.status, outcome.text.clone(), &outcome.settled, 2);
            assert_eq!(item.act.as_ref(), Some(&fixture.act), "{name}");
            assert_eq!(item.ended, fixture.ended, "{name}");
            assert_eq!(outcome.status, fixture.status, "{name}");
            assert_eq!(
                outcome.text.as_deref().unwrap_or(""),
                fixture.outcome,
                "{name}"
            );
            assert_eq!(outcome.job.is_some(), fixture.job, "{name}");
            let block = outcome.block.as_ref();
            assert_eq!(
                block.map(|block| (block.content.unwrap(), block.text.as_str())),
                fixture.content,
                "{name}"
            );
            assert_eq!(
                block.and_then(|block| block.more.as_deref()),
                fixture.more,
                "{name}"
            );
            // The whole content is kept when the preview leaves some out.
            assert_eq!(
                block.is_some_and(|block| block.whole.is_some()),
                fixture.more.is_some(),
                "{name}"
            );
        }
    }

    /// The receipt `edit_file` gives for replacing `old` with `new` (every
    /// match, with `all`) in a file whose text is `before`.
    pub(crate) fn edited(before: &str, old: &str, new: &str, all: bool) -> ToolReceipt {
        use arko_agents::tool::Tool;
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("f.txt"), before).unwrap();
        let parameters: HashMap<String, Value> = serde_json::from_value(serde_json::json!({
            "path": "f.txt", "old_string": old, "new_string": new, "replace_all": all,
        }))
        .unwrap();
        let context = arko_agents::TaskContext::new(Uuid::nil(), "main", "", folder.path());
        let run = arko_agents::builtins::EditFileTool.execute_with_output(&context, parameters);
        let output = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(run)
            .unwrap();
        output.receipt.expect("edit_file gives a receipt")
    }

    /// The receipt `apply_diff` gives for `diff` (every match, with `all`)
    /// in a file whose text is `before`.
    pub(crate) fn applied(before: &str, diff: &str, all: bool) -> ToolReceipt {
        use arko_agents::tool::Tool;
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("f.txt"), before).unwrap();
        let parameters: HashMap<String, Value> = serde_json::from_value(serde_json::json!({
            "path": "f.txt", "diff": diff, "replace_all": all,
        }))
        .unwrap();
        let context = arko_agents::TaskContext::new(Uuid::nil(), "main", "", folder.path());
        let run = arko_agents::builtins::ApplyDiffTool.execute_with_output(&context, parameters);
        let output = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(run)
            .unwrap();
        output.receipt.expect("apply_diff gives a receipt")
    }

    /// An apply_diff call saved before receipts shows each block's lines,
    /// removed and added, without the old form's header. A marker line of
    /// the file shows as the tool wrote it, whether the call escaped it
    /// with `AD_ESC::`, as calls before the backslash did, or with a
    /// backslash.
    #[test]
    fn a_saved_apply_diff_call_without_a_receipt_shows_its_blocks() {
        let parameters = params(&[
            ("path", "a.txt".into()),
            (
                "diff",
                "<<<<<<< SEARCH\n:start_line:2\n-------\nold\n=======\nAD_ESC::=======\nnew\n>>>>>>> REPLACE\n\
                 <<<<<<< SEARCH\n\\=======\n=======\n>>>>>>> REPLACE"
                    .into(),
            ),
        ]);
        let shown = content(
            APPLY_DIFF_TOOL_NAME,
            &parameters,
            "Successfully applied all 2 diff block(s) to 'a.txt'.",
            None,
        )
        .unwrap();
        let expected = [bare_diff("old", "=======\nnew"), bare_diff("=======", "")].join("\n");
        assert_eq!(shown.text, expected);
        assert_eq!(shown.kind, ContentKind::Diff);
    }

    /// An edit's diff is the tool's receipt of it, the whole lines it
    /// changed, removed as the file had them and added as it has them, with
    /// the lines around them numbered as the file has them now. Without a
    /// receipt it is the edit's old and new text, without numbers.
    #[test]
    fn an_edits_diff_is_the_tools_receipt_of_it() {
        let diff = |before, old, new, all| receipt_diff(&edited(before, old, new, all));
        assert_eq!(
            diff(
                "1\n2\n3\n4\nold\n6\n7\n8\n9\n",
                "old",
                "new a\nnew b",
                false
            )
            .as_deref(),
            Some(" 2\t2\n 3\t3\n 4\t4\n-5\told\n+5\tnew a\n+6\tnew b\n 7\t6\n 8\t7\n 9\t8")
        );
        // Inside a line, the line whole.
        assert_eq!(
            diff("fn a() {\n    let x = None;\n}\n", "None", "Some(1)", false).as_deref(),
            Some(" 1\tfn a() {\n-2\t    let x = None;\n+2\t    let x = Some(1);\n 3\t}")
        );
        // Twice in a line with every match, one change of it.
        assert_eq!(
            diff("a x x\n", "x", "y", true).as_deref(),
            Some("-1\ta x x\n+1\ta y y")
        );
        // A final blank line is a line.
        assert_eq!(
            diff("a\n\n", "a", "b", false).as_deref(),
            Some("-1\ta\n+1\tb\n 2\t")
        );
        // Changes far apart, with "…" between them.
        let far: String = (1..=12)
            .map(|n| match n {
                2 | 11 => "x\n".to_string(),
                n => format!("{n}\n"),
            })
            .collect();
        assert_eq!(
            diff(&far, "x\n", "y\n", true).as_deref(),
            Some(
                " 1\t1\n-2\tx\n+2\ty\n 3\t3\n 4\t4\n 5\t5\n…\n 8\t8\n 9\t9\n 10\t10\n-11\tx\n+11\ty\n 12\t12"
            )
        );
        // A receipt that left changes out says so.
        let many = diff(&"x\n".repeat(1_000), "x", "y", true).unwrap();
        assert!(many.ends_with(MORE_CHANGED), "{many}");
        assert_eq!(bare_diff("a\nb", "c"), "-\ta\n-\tb\n+\tc");
    }

    /// A content keeps a file's lines as they are, a blank line and one of
    /// spaces too, through its preview and its whole, and an empty file's
    /// write says so as a read of one does.
    #[test]
    fn a_contents_file_lines_are_kept_as_they_are() {
        let lines = |tool: &str, parameters: &[(&str, Value)], output: &str| {
            let block = super::outcome(tool, &params(parameters), output)
                .block
                .unwrap();
            (block.content.unwrap(), block.text)
        };
        let read = lines(
            READ_FILE_TOOL_NAME,
            &[("path", "a.rs".into())],
            "1 | x  \n2 |   \n3 | \n[Lines 1-3 of 3.]",
        );
        assert_eq!(read, (ContentKind::Lines, "1\tx  \n2\t  \n3\t".to_string()));
        assert_eq!(
            content::line(ContentKind::Lines, read.1.lines().last().unwrap()),
            content::Line::Numbered {
                number: "3",
                mark: content::Mark::None,
                text: ""
            }
        );
        let write = |text: &str| {
            lines(
                WRITE_FILE_TOOL_NAME,
                &[("path", "a.rs".into()), ("content", text.into())],
                "Wrote a.rs.",
            )
        };
        assert_eq!(write("\n"), (ContentKind::Lines, "1\t".to_string()));
        assert_eq!(
            write("a\n\n"),
            (ContentKind::Lines, "1\ta\n2\t".to_string())
        );
        assert_eq!(write(""), (ContentKind::Muted, "Empty".to_string()));
        // An edit whose last line is blank.
        let edit = outcome_after(
            EDIT_FILE_TOOL_NAME,
            &params(&[
                ("path", "a.rs".into()),
                ("old_string", "a".into()),
                ("new_string", "b".into()),
            ]),
            "Edited a.rs at line 1:\n1 | b\n2 | ",
            Some(&edited("a\n\n", "a", "b", false)),
            0,
        );
        assert_eq!(edit.block.unwrap().text, "-1\ta\n+1\tb\n 2\t");
    }

    /// A message of two long paragraphs previews as a start of the first,
    /// cut with its door under it, and the second paragraph stays behind
    /// the door, which counts everything after the cut.
    #[test]
    fn a_message_cut_in_its_first_paragraph_hides_the_second() {
        let first = format!("{} end.", "word ".repeat(80));
        let second = "Also add the patterns, and tell me if that belongs in the skill.";
        let message = format!("{first}\n\n{second}");
        let sent = super::outcome(
            crew::MESSAGE_AGENT,
            &params(&[("to", "r82".into()), ("message", message.as_str().into())]),
            "Sent to r82.",
        );
        let block = sent.block.unwrap();
        assert_eq!(
            block.text,
            format!("{}…", &first[..content::PREVIEW_LINE_CHARS])
        );
        let hidden = first.chars().count() - content::PREVIEW_LINE_CHARS + second.chars().count();
        assert_eq!(
            block.more.as_deref(),
            Some(format!("… {hidden} more characters").as_str())
        );
        assert_eq!(block.whole.as_deref(), Some(message.as_str()));
    }

    /// A skill's load opens to the skill's text, its first lines and the
    /// rest behind its door, with its line saying nothing more. One that
    /// loaded nothing opens to why.
    #[test]
    fn a_skill_load_opens_to_the_skill() {
        let name = [("name", Value::from("style"))];
        let skill: String = (1..=20).map(|n| format!("Rule {n}.\n")).collect();
        let loaded = super::outcome(
            LOAD_SKILL_TOOL_NAME,
            &params(&name),
            &format!(
                "--- BEGIN SKILL: style ---\n{skill}--- END SKILL: style ---\n\nThis skill has additional resources that can be loaded individually:\n- a.md"
            ),
        );
        assert_eq!((loaded.status, loaded.text), (CallStatus::Ok, None));
        let block = loaded.block.unwrap();
        assert_eq!(block.content, Some(ContentKind::Plain));
        assert_eq!(block.text.lines().next(), Some("Rule 1."));
        assert_eq!(block.text.lines().count(), content::PREVIEW_LINES);
        assert_eq!(block.whole.as_deref(), Some(skill.trim_end()));
        let missing = super::outcome(
            LOAD_SKILL_TOOL_NAME,
            &params(&name),
            "Skill 'style' not found. Available skills:\n- review",
        );
        let block = missing.block.unwrap();
        assert_eq!(
            (block.content, block.text.as_str()),
            (
                Some(ContentKind::Muted),
                "Skill 'style' not found. Available skills:\n- review"
            )
        );
    }

    /// A wait's content is the mail that came, each by its heading, from
    /// the user's side.
    #[test]
    fn a_waits_content_is_its_mail_as_the_user_reads_it() {
        assert_eq!(
            mail(
                "Still running: r4.\n\n[mail 2] r3 (agent `worker`) finished. Its report:\n\nDone.\n\n[end of mail 2]\n\n[mail 3] r4 (agent `worker`) sent you a message:\n\nHalfway.\n\n[end of mail 3]"
            ),
            "Still running: r4.\n\nr3 finished. Its report:\n\nDone.\n\nr4 sent a message:\n\nHalfway.\n"
        );
    }

    /// A wait says the runs it names, also when they ended. One that names
    /// none waits for the agent's workers, and says so the same live and
    /// on restore, since what a call says comes from the call alone.
    #[test]
    fn a_wait_names_the_runs_it_waits_for() {
        let wait = |pairs: &[(&str, Value)]| {
            let item = super::call(crew::WAIT_FOR_AGENTS, &params(pairs), "main").unwrap();
            assert_eq!(item.kind, Kind::Call);
            assert_eq!(item.tool.as_deref(), Some(crew::WAIT_FOR_AGENTS));
            (item.text, item.act.unwrap().target)
        };
        assert_eq!(wait(&[]), ("waiting for workers".into(), String::new()));
        assert_eq!(
            wait(&[("all", true.into())]),
            ("waiting for workers".into(), String::new())
        );
        assert_eq!(
            wait(&[("runs", "r3, r4".into())]),
            ("waiting for r3, r4".into(), "r3, r4".into())
        );
        assert_eq!(
            wait(&[("runs", serde_json::json!(["r1", "r2"]))]),
            ("waiting for r1, r2".into(), "r1, r2".into())
        );
        assert_eq!(
            quiet::doing(&super::act(crew::WAIT_FOR_AGENTS, &params(&[]), "main")),
            "Waiting for workers"
        );
        // A wait's line says what came, as the user reads it, not as its
        // result tells the agent that waited.
        let told = |output: &str| outcome(crew::WAIT_FOR_AGENTS, output).text;
        assert_eq!(
            told("Nothing to wait for: r1 is finished. Reports that came earlier reached you already.")
                .as_deref(),
            Some("nothing new")
        );
        assert_eq!(
            told("An update for you arrived while you waited. It follows. Wait again if you still need to. Still running: r3.")
                .as_deref(),
            Some("1 update")
        );
    }

    /// A wait on a job reads as a wait, not as a look at the job.
    #[test]
    fn a_wait_on_a_job_reads_as_a_wait() {
        let parameters = params(&[
            ("action", "wait".into()),
            ("job_id", "4d2c1a9b-0000".into()),
            ("wait_ms", "60000".into()),
        ]);
        let item = super::call("terminal_control", &parameters, "main").unwrap();
        assert_eq!(item.text, "waiting for job 4d2c1a9b");
        // (Its act has the job's whole ID, by which waits on it merge, and
        // names it by the ID's start.)
        let act = item.act.unwrap();
        assert_eq!(
            (act.kind, act.target.as_str()),
            (ActKind::JobWait, "4d2c1a9b-0000")
        );
        assert_eq!(quiet::doing(&act), "Waiting for job 4d2c1a9b");
        // Its other actions and a look at the job stay as they were.
        let status = super::act("terminal_status", &parameters, "main");
        assert_eq!(
            (status.kind, status.target.as_str()),
            (ActKind::Check, "4d2c1a9b-0000")
        );
        assert_eq!(quiet::doing(&status), "Checking job 4d2c1a9b");
        let list = super::call(
            "terminal_control",
            &params(&[("action", "list".into())]),
            "main",
        );
        assert_eq!(list.unwrap().text, "terminal control");
    }

    /// A job as the terminal tools return it, as JSON.
    fn snapshot(
        state: &str,
        exit_code: Option<i32>,
        revision: u64,
        input: Option<&str>,
        output: (&str, u64, u64, u64),
        error: Option<&str>,
    ) -> String {
        let (text, start, end, available_end) = output;
        serde_json::json!({
            "error": error,
            "exit_code": exit_code,
            "input": input.map(|prompt| serde_json::json!({
                "prompt": prompt,
                "prompt_truncated": false,
                "source": {"kind": "prompt_rule", "rule": "[y/N]"},
            })),
            "job_id": "c876812b-5f3e-4a1d-9b7c-2e8f0a6d4c13",
            "log_path": "/home/ada/proj/.arko/jobs/c876812b.log",
            "log_truncated": false,
            "output": {
                "available_end": available_end,
                "end": end,
                "gap": false,
                "start": start,
                "text": text,
            },
            "revision": revision,
            "state": state,
            "terminal": true,
        })
        .to_string()
    }

    /// A wait on a job, and a look at it, say what they found of the job in
    /// words, each finding its own, never the tool's JSON. What a wait found
    /// is its row's news. Output is new only past what the agent got of the
    /// job before, whatever else moved the job on. Each opens to the job's
    /// output when it found something, as plain text without the terminal's
    /// styles, and a wait that found nothing new has nothing to open.
    #[test]
    fn a_wait_on_a_job_says_what_it_found() {
        let wait = params(&[
            ("action", "wait".into()),
            ("job_id", "c876812b-5f3e-4a1d-9b7c-2e8f0a6d4c13".into()),
            ("revision", 3.into()),
        ]);
        let printed = (
            "   Compiling arko v0.24.0\r\n\u{1b}[32mok\u{1b}[0m 40%\r\u{1b}[32mok\u{1b}[0m 100%\r\n",
            0,
            60,
            60,
        );
        let found = [
            ("output", snapshot("running", None, 9, None, printed, None)),
            // (At a later revision, as an input given to it makes, with no
            // more output than the agent got.)
            ("nothing", snapshot("running", None, 9, None, printed, None)),
            (
                "input",
                snapshot("running", None, 9, Some("Apply? [y/N] "), printed, None),
            ),
            (
                "finished",
                snapshot("succeeded", Some(0), 9, None, printed, None),
            ),
            (
                "failed",
                snapshot("failed", Some(1), 9, None, printed, None),
            ),
            (
                "stopped",
                snapshot("cancelled", None, 9, None, printed, None),
            ),
            (
                "timed out",
                snapshot("timed_out", None, 9, None, printed, None),
            ),
            (
                "interrupted",
                snapshot("interrupted", None, 9, None, printed, None),
            ),
        ];
        let mut said = std::collections::HashSet::new();
        for (case, output) in &found {
            let seen = if *case == "nothing" { 60 } else { 40 };
            let outcome = outcome_after("terminal_control", &wait, output, None, seen);
            assert_eq!(outcome.status, CallStatus::Ok, "{case}");
            let text = outcome.text.clone().unwrap_or_default();
            assert!(
                !text.is_empty() && !text.contains(['{', '"']),
                "{case}: {text}"
            );
            assert!(said.insert(text.clone()), "{case}: {text} again");
            assert_eq!(
                outcome.settled.news.as_deref(),
                Some(text.as_str()),
                "{case}"
            );
            match (*case, &outcome.block) {
                ("nothing", block) => assert!(block.is_none(), "{case}"),
                // (The job's output as it last stood, and a prompt it asks
                // with after it.)
                (case, Some(block)) => {
                    let output = "   Compiling arko v0.24.0\nok 100%";
                    let shown = match case {
                        "input" => format!("{output}\nApply? [y/N] "),
                        _ => output.to_string(),
                    };
                    assert_eq!(block.content, Some(ContentKind::Plain), "{case}");
                    assert_eq!(block.text, shown, "{case}");
                }
                (_, None) => panic!("{case}: nothing to open"),
            }
        }
        // A failure names its exit code, and a success needn't.
        let code = |output: &str| super::outcome("terminal_control", &wait, output).text;
        assert!(code(&found[4].1).unwrap().contains('1'));
        assert!(!code(&found[3].1).unwrap().contains('0'));
        // A look says how the job stands, which a wait that found nothing
        // new or new output doesn't say.
        let status = params(&[("job_id", "c876812b-5f3e-4a1d-9b7c-2e8f0a6d4c13".into())]);
        let look = super::outcome("terminal_status", &status, &found[0].1);
        assert!(!said.contains(look.text.as_deref().unwrap()));
        assert!(look.settled.news.is_none());
        // The terminal's other calls say nothing more than their words, and
        // a list counts its jobs.
        for (tool, action, output) in [
            ("terminal_input", "", r#"{"input_written":true}"#),
            (
                "terminal_control",
                "cancel",
                r#"{"cancellation_requested":true}"#,
            ),
            ("terminal_control", "forget", r#"{"forgotten":true}"#),
        ] {
            let parameters = params(&[("action", action.into())]);
            let outcome = super::outcome(tool, &parameters, output);
            assert_eq!(
                (outcome.text, outcome.block),
                (None, None),
                "{tool} {action}"
            );
        }
        let list = super::outcome(
            "terminal_control",
            &params(&[("action", "list".into())]),
            r#"[{"job_id":"a","state":"running","revision":1},{"job_id":"b","state":"succeeded","revision":4}]"#,
        );
        assert!(list.text.unwrap().contains('2'));
        // A result that isn't the tool's JSON reads as any tool's.
        let other = super::outcome("terminal_control", &wait, "no such job");
        assert_eq!(other.text.as_deref(), Some("no such job"));
    }

    /// What the agent got of a job before is the furthest any look at it or
    /// wait on it got, whatever their order, and nothing of another job's.
    #[test]
    fn what_the_agent_got_of_a_job_is_the_furthest_it_got() {
        let job = "c876812b-5f3e-4a1d-9b7c-2e8f0a6d4c13";
        let got = |kind: ActKind, target: &str, end: u64| {
            let mut item = Item::new(Kind::Call, "");
            item.act = Some(Act {
                kind,
                target: target.into(),
                job_output_end: Some(end),
                ..Act::default()
            });
            item
        };
        let earlier = [
            got(ActKind::JobWait, job, 30),
            got(ActKind::Check, job, 12),
            got(ActKind::JobWait, "4d2c1a9b-0000-4000-8000-000000000001", 90),
        ];
        let wait = params(&[("action", "wait".into()), ("job_id", job.into())]);
        assert_eq!(job_seen("terminal_control", &wait, earlier.iter()), 30);
        let look = params(&[("job_id", job.into())]);
        assert_eq!(job_seen("terminal_status", &look, earlier.iter()), 30);
        assert_eq!(job_seen("terminal_control", &wait, [].iter()), 0);
        assert_eq!(job_seen(SHELL_TOOL_NAME, &wait, earlier.iter()), 0);
        // So a wait whose output ends before that found nothing new.
        let output = snapshot("running", None, 9, None, ("x", 0, 20, 20), None);
        let seen = job_seen("terminal_control", &wait, earlier.iter());
        let found = outcome_after("terminal_control", &wait, &output, None, seen);
        assert_eq!(found.block, None);
        assert_ne!(
            found.text,
            outcome_after("terminal_control", &wait, &output, None, 0).text
        );
    }

    /// A job's output, as a wait or a look opens to it, says where the
    /// output it leaves out is, and ends with the job's prompt and its error
    /// when the output doesn't hold them, once.
    #[test]
    fn a_jobs_output_says_what_it_leaves_out() {
        let wait = params(&[
            ("action", "wait".into()),
            ("job_id", "c876812b-5f3e-4a1d-9b7c-2e8f0a6d4c13".into()),
            ("revision", 3.into()),
        ]);
        let opened = |output: &str| {
            super::outcome("terminal_control", &wait, output)
                .block
                .map(|block| block.text)
                .unwrap_or_default()
        };
        let log = "/home/ada/proj/.arko/jobs/c876812b.log";
        // Output before and after the window it got.
        let window = opened(&snapshot(
            "running",
            None,
            9,
            None,
            ("middle", 4000, 4006, 9000),
            None,
        ));
        let lines: Vec<&str> = window.lines().collect();
        assert_eq!(lines.len(), 3, "{window}");
        assert!(lines[0].contains(log) && lines[2].contains(log), "{window}");
        assert_eq!(lines[1], "middle");
        // A prompt the output ends with, of several lines, shows once, also
        // when later output follows the window.
        let prompt = "Enter value:\r\n> ";
        for available in [40, 9000] {
            let shown = opened(&snapshot(
                "running",
                None,
                9,
                Some(prompt),
                ("Starting\r\nEnter value:\r\n> ", 0, 40, available),
                None,
            ));
            assert_eq!(shown.matches("Enter value:").count(), 1, "{shown}");
        }
        // One the output doesn't show comes at the end.
        let shown = opened(&snapshot(
            "running",
            None,
            9,
            Some("Apply? [y/N] "),
            ("Starting", 0, 8, 8),
            None,
        ));
        assert!(shown.trim_end().ends_with("Apply? [y/N]"), "{shown}");
        // Why it failed, when its output doesn't say it.
        let why = "couldn't read the process's output";
        let shown = opened(&snapshot("failed", None, 9, None, ("", 0, 0, 0), Some(why)));
        assert_eq!(shown, why);
        let shown = opened(&snapshot(
            "failed",
            None,
            9,
            None,
            (why, 0, 35, 35),
            Some(why),
        ));
        assert_eq!(shown.matches(why).count(), 1, "{shown}");
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
        // (Its task is its content.)
        assert_eq!(item.detail, None);
        // The worker's profile, model and effort show on its call.
        let item = call(
            crew::START_AGENT,
            &params(&[
                ("task", "Judge it.".into()),
                ("profile", "3d-judge".into()),
                ("model", "sol".into()),
                ("effort", "high".into()),
                ("cache", "1h".into()),
            ]),
        )
        .unwrap();
        assert_eq!(
            item.text,
            "start worker (profile 3d-judge, sol, high effort, 1h cache) “Judge it.”"
        );
        let item = call(crew::WORKER_OPTIONS, &params(&[])).unwrap();
        assert_eq!(item.text, "list worker options");
        let save = |pairs: &[(&str, serde_json::Value)]| {
            call(crew::SAVE_WORKER_PROFILE, &params(pairs))
                .unwrap()
                .text
        };
        assert_eq!(
            save(&[("name", "judge".into())]),
            "save worker profile judge for this project"
        );
        assert_eq!(
            save(&[("name", "judge".into()), ("scope", "session".into())]),
            "save worker profile judge for this session"
        );
        assert_eq!(
            save(&[("name", "judge".into()), ("scope", "user".into())]),
            "save worker profile judge for every project"
        );
        let item = call(
            SHELL_TOOL_NAME,
            &params(&[("command", "cd a\nmake".into()), ("cwd", "src".into())]),
        )
        .unwrap();
        assert_eq!(item.text, "(in src) $ cd a  (1 more line)");
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
        assert_eq!(item.text, "message main “Done?”");
    }

    /// A start that began or queued its worker sets it going, and so does a
    /// message that continued or restarted a worker that ended. A refusal, a
    /// message the worker reads as it runs and a result of another kind set
    /// none. A start keeps its task's title.
    #[test]
    fn a_call_names_the_worker_it_set_going() {
        let sets = |tool: &str, output: &str| outcome(tool, output).sets;
        for (output, worker) in [
            ("Started r5 (agent `worker`).", Some("r5")),
            (
                "Started r12 (agent `worker`) in the worktree /tmp/w.",
                Some("r12"),
            ),
            (
                "Queued r5 (agent `worker`): 4 agent runs are running, the most this workflow allows, so it starts when one ends.",
                Some("r5"),
            ),
            ("The task is empty.", None),
        ] {
            assert_eq!(
                sets(crew::START_AGENT, output).as_deref(),
                worker,
                "{output}"
            );
        }
        assert_eq!(Outcome::failed("refused (over budget)").sets, None);
        assert_eq!(sets("shell", "Started r5"), None);
        for (output, worker) in [
            (
                "Sent: r2 continues its task with your message, with its context; its new report reaches you when it finishes.",
                Some("r2"),
            ),
            (
                "Sent: r2 restarts where it stopped, with your message; its report reaches you when it finishes.",
                Some("r2"),
            ),
            (
                "Sent: r2 goes on with your message once an agent run ends (the most are running).",
                Some("r2"),
            ),
            ("Sent: r2 reads it at its next step.", None),
            ("Sent: r2 reads it when it starts.", None),
            ("Sent: r2 gets it as the answer to its question.", None),
            ("Sent to r3.", None),
        ] {
            let outcome = outcome(crew::MESSAGE_AGENT, output);
            assert_eq!(
                (outcome.sets.as_deref(), outcome.text.as_deref()),
                (worker, Some("sent")),
                "{output}"
            );
        }
        let start = super::call(
            crew::START_AGENT,
            &params(&[("task", "## Review\n\nRead the diff.".into())]),
            "main",
        )
        .unwrap();
        assert_eq!(start.task_title.as_deref(), Some("Review"));
        let empty = super::call(crew::START_AGENT, &params(&[("task", " ".into())]), "main");
        assert_eq!(empty.unwrap().task_title.as_deref(), Some("Start a worker"));
    }

    /// Only a start that began the worker at once names it as started. One
    /// that waits for a place, a refusal and a result of another kind do not.
    #[test]
    fn a_start_names_its_worker_only_when_it_began_at_once() {
        let started = |output: &str| outcome(crew::START_AGENT, output).started;
        assert_eq!(
            started("Started r5 (agent `worker`)."),
            Some("r5".to_string())
        );
        assert_eq!(
            started("Started r12 (agent `worker`) in the worktree /tmp/w."),
            Some("r12".to_string())
        );
        assert_eq!(
            started(
                "Queued r5 (agent `worker`): 4 agent runs are running, the most this workflow allows, so it starts when one ends."
            ),
            None
        );
        assert_eq!(started("The task is empty."), None);
        assert_eq!(Outcome::failed("refused (over budget)").started, None);
        // Another tool's result that starts with the same word.
        assert_eq!(outcome("shell", "Started r5").started, None);
    }

    /// A result is told in a few words on its call's line, and a command's
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
                "[mail 2] r1 (agent `worker`) finished. Its report:\n\nDone.\n\n[end of mail 2]\n\n[mail 3] r2 (agent `worker`) finished. Its report:\n\nDone.\n\n[end of mail 3]"
            ),
            (CallStatus::Ok, "r1 and r2 finished".into())
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
            (CallStatus::Ok, "4 edits".into())
        );
        assert_eq!(
            told(
                EDIT_FILE_TOOL_NAME,
                "Replaced 1 occurrence in a.rs, at line 5."
            ),
            (CallStatus::Ok, "1 edit".into())
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
            (block.kind, block.content, block.text.as_str()),
            (
                Kind::Result,
                Some(ContentKind::Plain),
                "===== 1 failed, 4 passed in 0.31s ====="
            )
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
        // Why it failed is its content, unless its line says it all.
        assert_eq!(
            Outcome::failed("short").block.map(|block| block.content),
            Some(Some(ContentKind::Muted))
        );
        assert!(Outcome::failed(quiet::STOPPED).block.is_none());
    }

    /// A tool's error reads as the tool said it, once, whether it came with
    /// "Execution failed: " doubled after the call's own words (as sessions
    /// saved with older arko_agents keep it) or not. A failed edit's line
    /// says one word, the same for every edit's error, which names no file,
    /// and its content is the whole error. Other tools' lines say the
    /// error's first line, and a refusal or a stop stays as it is.
    #[test]
    fn a_tools_error_reads_once_and_a_failed_edit_says_it_failed() {
        let why = "None of the 2 blocks applied, so src/app.rs is unchanged.\nBlock 1: not found.";
        let mut lines = Vec::new();
        for tool in [APPLY_DIFF_TOOL_NAME, EDIT_FILE_TOOL_NAME, "read_file"] {
            for said in [
                format!("Tool '{tool}' execution failed: Execution failed: {why}"),
                format!("Tool '{tool}' execution failed: {why}"),
            ] {
                let failed = Outcome::error(tool, &said);
                assert_eq!(failed.status, CallStatus::Failed);
                let block = failed.block.expect("its content");
                assert_eq!(block.text, why, "{tool}: {said}");
                assert_eq!(failed.output.as_deref(), Some(why));
                let line = failed.text.unwrap();
                assert!(!line.contains("Execution failed"), "{line}");
                lines.push((tool, line));
            }
        }
        let edits: Vec<&String> = lines
            .iter()
            .filter(|(tool, _)| *tool != "read_file")
            .map(|(_, line)| line)
            .collect();
        assert!(edits.iter().all(|line| *line == edits[0]), "{edits:?}");
        assert!(!edits[0].contains("app.rs") && !edits[0].contains("blocks"));
        let other = Outcome::error(
            EDIT_FILE_TOOL_NAME,
            "Tool 'edit_file' execution failed: boom",
        );
        assert_eq!(other.text.as_ref(), Some(edits[0]), "every edit's error");
        let read = Outcome::error(
            "read_file",
            "Tool 'read_file' execution failed: No such file",
        );
        assert_eq!(read.text.as_deref(), Some("No such file"));
        // A refusal and a stop of an edit say so, as any call's do.
        for (said, line) in [
            ("refused (outside access is off)", "refused"),
            (quiet::STOPPED, quiet::STOPPED),
        ] {
            assert_eq!(
                Outcome::error(APPLY_DIFF_TOOL_NAME, said).text.as_deref(),
                Some(line)
            );
        }
    }

    /// An edit is one place in its file that changed, so a call that
    /// changed every match of a block counts each place, on its Full line,
    /// in its Quiet row, and in a Quiet row that merges it with other edits.
    #[test]
    fn every_place_an_edit_changed_is_an_edit() {
        let number = |text: &str| {
            text.split_whitespace()
                .find_map(|word| word.parse::<usize>().ok())
        };
        let diff = "<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE\n";
        let all = params(&[
            ("path", "src/app.rs".into()),
            ("diff", diff.into()),
            ("old_string", "x".into()),
            ("new_string", "y".into()),
            ("replace_all", true.into()),
        ]);
        let two = params(&[
            ("path", "src/app.rs".into()),
            ("diff", diff.repeat(2).into()),
        ]);
        for tool in [APPLY_DIFF_TOOL_NAME, EDIT_FILE_TOOL_NAME] {
            let ended = |parameters: &Parameters, output: &str| {
                let mut item = call(tool, parameters).unwrap();
                let outcome = outcome_after(tool, parameters, output, None, 0);
                item.settle(outcome.status, outcome.text, &outcome.settled, 2);
                item
            };
            let replaced = ended(
                &all,
                "Replaced 5 occurrences in src/app.rs, at lines 1, 3, 5, 7 and 9.",
            );
            let line = replaced.outcome.clone().unwrap();
            assert_eq!(number(&line), Some(5), "{tool}: {line}");
            let once = ended(&all, "Replaced 1 occurrence in src/app.rs, at line 4.");
            assert_eq!(number(once.outcome.as_deref().unwrap()), Some(1));
            // The Quiet rows, of the call alone and merged with others.
            let said = |items: &[&Item]| {
                let steps: Vec<quiet::Step> = items
                    .iter()
                    .enumerate()
                    .map(|(at, item)| {
                        quiet::Step::Call(quiet::Call {
                            id: at as u64 + 1,
                            act: item.act.as_ref(),
                            status: item.status,
                            outcome: item.outcome.as_deref(),
                            turn: Some(1),
                            ..quiet::Call::default()
                        })
                    })
                    .collect();
                match quiet::rows(&steps).as_slice() {
                    [quiet::Piece::Row(row)] => row.words.text.clone(),
                    pieces => panic!("{pieces:?}"),
                }
            };
            assert_eq!(number(&said(&[&replaced])), Some(5), "{tool}");
            assert_eq!(number(&said(&[&replaced, &once])), Some(6), "{tool}");
            if tool == APPLY_DIFF_TOOL_NAME {
                let blocks = ended(&two, "Edited src/app.rs at lines 1 and 3:\n1 | y\n3 | y");
                assert_eq!(number(blocks.outcome.as_deref().unwrap()), Some(2));
                assert_eq!(number(&said(&[&replaced, &blocks])), Some(7), "{tool}");
            }
        }
    }

    /// An index search's query shows once. Its act names the index and
    /// keeps the whole query as its detail, which opening its Quiet row
    /// shows, and no query for the row's words. Its results leave out how
    /// many there were and where, which the Full line's outcome says.
    #[test]
    fn a_searchs_query_and_count_show_once() {
        let output = "2 results from arko (code, hybrid):\n\nsrc/a.rs:1-9 · fn a\n1 | fn a() {\nsrc/b.rs:3-4 · fn b\n3 | fn b() {";
        for query in [
            "branched message index",
            "terminal Full command detail duplicate whole command running call detail shown once",
        ] {
            let parameters = params(&[("index", "arko".into()), ("query", query.into())]);
            let mut item = call("index_search", &parameters).unwrap();
            let outcome = outcome_after("index_search", &parameters, output, None, 0);
            item.settle(outcome.status, outcome.text.clone(), &outcome.settled, 2);
            let act = item.act.as_ref().unwrap();
            assert_eq!(
                (act.target.as_str(), act.query.as_str(), act.detail.as_str()),
                ("arko", "", query)
            );
            let content = outcome.block.unwrap().text;
            assert!(!content.contains(query), "{content}");
            let said = outcome.text.unwrap();
            assert!(!content.contains(&said), "{content}");
            assert!(content.contains("src/a.rs:1-9") && content.contains("src/b.rs:3-4"));
            // Full's line holds the query, so its detail doesn't.
            assert!(item.text.contains(query) && item.detail.is_none());
        }
    }

    /// A file tool's line says where it names its file, whose folders and
    /// name a narrow line tells apart ([`FileLine`](super::super::FileLine)).
    #[test]
    fn a_file_tools_line_says_where_its_file_is() {
        let parts = |tool: &str, pairs: &[(&str, Value)]| {
            let item = call(tool, &params(pairs)).unwrap();
            let file = super::super::FileLine::of(&item.text, item.path).unwrap();
            (
                file.lead.to_string(),
                file.folders.to_string(),
                file.name.to_string(),
            )
        };
        let path = |path: &str| vec![("path", Value::from(path))];
        for tool in [
            READ_FILE_TOOL_NAME,
            WRITE_FILE_TOOL_NAME,
            EDIT_FILE_TOOL_NAME,
            APPLY_DIFF_TOOL_NAME,
        ] {
            let (lead, folders, name) = parts(tool, &path("src/remote/render.rs"));
            assert_eq!(
                (folders.as_str(), name.split_whitespace().next()),
                ("src/remote/", Some("render.rs")),
                "{tool}"
            );
            assert!(
                lead.ends_with(' ') && !lead.contains("src"),
                "{tool}: {lead:?}"
            );
        }
        let (_, folders, name) = parts(
            READ_FILE_TOOL_NAME,
            &[("path", "a/b.rs".into()), ("start_line", 3.into())],
        );
        assert_eq!(folders, "a/");
        assert!(name.starts_with("b.rs") && name.contains('3'), "{name}");
        let (_, folders, name) =
            parts(LIST_FILES_TOOL_NAME, &[("directory", "src/remote/".into())]);
        assert_eq!((folders.as_str(), name.as_str()), ("src/", "remote/"));
        let (_, folders, name) = parts(EDIT_FILE_TOOL_NAME, &path("C:\\Users\\ada\\a.rs"));
        assert_eq!(
            (folders.as_str(), name.as_str()),
            ("C:\\Users\\ada\\", "a.rs")
        );
        let (_, folders, name) = parts(WRITE_FILE_TOOL_NAME, &path("new.rs"));
        assert!(folders.is_empty() && name.starts_with("new.rs"));
        // A path too long for the line loses its start, never its name.
        let deep = format!("{}app.rs", "nested/".repeat(40));
        let (_, folders, name) = parts(APPLY_DIFF_TOOL_NAME, &path(&deep));
        assert!(
            folders.starts_with('…') && folders.ends_with("nested/"),
            "{folders}"
        );
        assert_eq!(name, "app.rs");
        let act = call(APPLY_DIFF_TOOL_NAME, &params(&path(&deep)))
            .unwrap()
            .act
            .unwrap();
        assert!(act.target.ends_with("/app.rs"), "{}", act.target);
        // A name longer than the line's limit stays whole too.
        let long = format!("{}.rs", "a".repeat(200));
        for folders in ["src/".to_string(), deep.replace("app.rs", "")] {
            let (_, kept, name) = parts(READ_FILE_TOOL_NAME, &path(&format!("{folders}{long}")));
            assert_eq!((kept.as_str(), name.as_str()), ("…/", long.as_str()));
        }
        // Other calls name no file, and a place outside the line is none.
        assert_eq!(
            call(SHELL_TOOL_NAME, &params(&[("command", "ls src/".into())]))
                .unwrap()
                .path,
            None
        );
        assert_eq!(super::super::FileLine::of("edit é.rs", Some((6, 9))), None);
        assert_eq!(super::super::FileLine::of("edit a.rs", Some((5, 99))), None);
    }

    /// Each call says what it does for the Quiet view, and its result
    /// completes that with a test run's counts, the index a search used, what
    /// reached a wait.
    #[test]
    fn calls_carry_their_act_and_results_complete_it() {
        let act = |tool: &str, pairs: &[(&str, Value)]| {
            call(tool, &params(pairs))
                .and_then(|item| item.act)
                .unwrap()
        };
        let read = act(READ_FILE_TOOL_NAME, &[("path", "src/tui/app.rs".into())]);
        assert_eq!(
            (read.kind, read.target.as_str()),
            (ActKind::Read, "src/tui/app.rs")
        );
        let shell = act(
            SHELL_TOOL_NAME,
            &[(
                "command",
                "cd ../arko/web && cargo test -p arko-web\nls".into(),
            )],
        );
        assert_eq!(
            (shell.kind, shell.target.as_str()),
            (ActKind::Command, "cargo test -p arko-web in web")
        );
        let program = act(
            SHELL_TOOL_NAME,
            &[(
                "command",
                "/home/ada/src/arko/target/debug/arko --resume 20261027 --remote 127.0.0.1:1"
                    .into(),
            )],
        );
        assert_eq!(
            program.target,
            "arko --resume 20261027 --remote 127.0.0.1:1"
        );
        let set = act(
            SHELL_TOOL_NAME,
            &[(
                "command",
                "set -o pipefail; CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only cargo test --lib".into(),
            )],
        );
        assert_eq!(set.target, "cargo test --lib");
        let then = act(
            SHELL_TOOL_NAME,
            &[("command", "cd ../arko; git log -3".into())],
        );
        assert_eq!(then.target, "git log -3 in arko");
        let message = act(
            crew::MESSAGE_AGENT,
            &[("to", "lead".into()), ("message", "Done.".into())],
        );
        assert_eq!(
            (message.kind, message.target.as_str()),
            (ActKind::Message, "main")
        );
        // A worker's message to its lead names the worker that started it.
        let nested = super::call(
            crew::MESSAGE_AGENT,
            &params(&[("to", "lead".into()), ("message", "Done.".into())]),
            "r5",
        )
        .unwrap();
        assert_eq!(nested.text, "message r5 “Done.”");
        assert_eq!(nested.act.map(|act| act.target).as_deref(), Some("r5"));
        let wait = super::call(
            crew::WAIT_FOR_AGENTS,
            &params(&[("runs", "r3, r4".into())]),
            "main",
        )
        .and_then(|item| item.act)
        .unwrap();
        assert_eq!((wait.kind, wait.target.as_str()), (ActKind::Wait, "r3, r4"));
        let start = act(crew::START_AGENT, &[("task", "Fix it.".into())]);
        assert_eq!(
            (start.kind, start.doing.as_str(), start.verb.as_str()),
            (ActKind::Other, "Starting a worker", "start a worker")
        );
        let forget = act(
            "terminal_control",
            &[
                ("action", "forget".into()),
                ("job_id", "4d2c1a9b-0000".into()),
            ],
        );
        assert_eq!(
            (forget.doing.as_str(), forget.done.as_str()),
            ("Forgetting job 4d2c1a9b", "Forgot job 4d2c1a9b")
        );
        let mcp = act("mcp__github__create_issue", &[]);
        assert_eq!(
            (mcp.kind, mcp.target.as_str()),
            (ActKind::Mcp, "create_issue on github")
        );
        // What results add.
        let settled = |tool: &str, output: &str, mut act: Act| {
            let mut act = Some(std::mem::take(&mut act));
            outcome(tool, output).settled.apply(&mut act);
            act.unwrap()
        };
        let tested = settled(
            SHELL_TOOL_NAME,
            "exit code 101, 3.0 s\n===== 1 failed, 4 passed in 0.31s =====",
            shell.clone(),
        );
        assert_eq!(
            (tested.news.as_str(), tested.alone, tested.ran),
            ("1 failed, 4 passed", true, true)
        );
        // A job that goes on in the background says how it ended instead.
        let mut moved = Some(tested.clone());
        Settled::job().apply(&mut moved);
        assert_eq!(moved.map(|act| act.news).as_deref(), Some(""));
        // (It runs tests, so it stands alone even when it prints no counts.)
        let built = settled(SHELL_TOOL_NAME, "exit code 2, 3.0 s\nerror", shell.clone());
        assert_eq!((built.news.as_str(), built.alone), ("", true));
        let quiet = settled(SHELL_TOOL_NAME, "exit code 0, 0.1 s\n(no output)", shell);
        assert_eq!((quiet.news.as_str(), quiet.ran), ("", true));
        // A search's indexes and its count come from the search itself.
        let search = act("index_search", &[("query", "state in session".into())]);
        let found = |indexes: &[&str], hits: usize| {
            let mut found = Settled::default();
            let indexes: Vec<String> = indexes.iter().map(|index| index.to_string()).collect();
            found.found(&indexes, hits);
            let mut act = Some(search.clone());
            found.apply(&mut act);
            act.unwrap()
        };
        let some = found(&["arko"], 6);
        assert_eq!((some.target.as_str(), some.news.as_str()), ("arko", ""));
        let none = found(&["gpui", "docs"], 0);
        assert_eq!(
            (none.target.as_str(), none.news.as_str()),
            ("gpui, docs", "no results")
        );
        // A command that runs tests has a row of its own whatever it prints.
        for (command, tests) in [
            ("cargo test -p arko > test.log", true),
            ("go test ./...", true),
            ("cd web && npm run test", true),
            ("python3 -m pytest -q", true),
            ("cargo +nightly test", true),
            ("cargo --locked test -p arko", true),
            ("go -C web test ./...", true),
            ("npm --prefix web test", true),
            ("pnpm -C web test", true),
            ("yarn --cwd web run test", true),
            ("make -C build check", true),
            ("zig test src/main.zig > test.log 2>&1", true),
            ("zig build test > out.log", true),
            ("zig build > out.log", false),
            ("cargo --locked build", false),
            ("npm --prefix web run build", false),
            ("cargo build", false),
            ("rg test src", false),
        ] {
            let shell = act(SHELL_TOOL_NAME, &[("command", command.into())]);
            assert_eq!(shell.alone, tests, "{command}");
        }
        let waited = settled(
            crew::WAIT_FOR_AGENTS,
            "Still running: r4.\n\n[mail 2] r3 (agent `worker`) finished. Its report:\n\nDone.\n\n[end of mail 2]\n\n[mail 3] r4 (agent `worker`) sent you a message:\n\nHalfway.\n\n[end of mail 3]",
            wait.clone(),
        );
        assert_eq!(
            waited.came,
            Some(Came {
                finished: vec!["r3".into()],
                messages: 1,
                ..Came::default()
            })
        );
        // Mail a report quotes is part of that report.
        let quoted = settled(
            crew::WAIT_FOR_AGENTS,
            "None of your agent runs is running now.\n\n[mail 4] r3 (agent `worker`) finished. Its report:\n\nThe wait printed:\n[mail 9] r7 (agent `worker`) finished. Its report:\n[end of mail 9]\n\n[end of mail 4]",
            wait.clone(),
        );
        assert_eq!(
            quoted.came,
            Some(Came {
                finished: vec!["r3".into()],
                ..Came::default()
            })
        );
        let woken = settled(
            crew::WAIT_FOR_AGENTS,
            "An update for you arrived while you waited. It follows. Wait again if you still need to. Still running: r3.",
            wait,
        );
        assert_eq!(
            woken.came,
            Some(Came {
                updates: 1,
                ..Came::default()
            })
        );
        let queued = settled(
            crew::START_AGENT,
            "Queued r5 (agent `worker`): 4 agent runs are running.",
            start,
        );
        assert_eq!(queued.done, "Queued r5");
    }

    /// A command's block previews its output, and keeps the whole when the
    /// preview leaves some of it out, lines or the end of a long line.
    #[test]
    fn a_commands_output_is_previewed_and_kept_whole() {
        let block = |output: &str| outcome(SHELL_TOOL_NAME, output).block.unwrap();
        let short = block("exit code 0, 0.1 s\nshort\nlines");
        assert_eq!(
            (short.text.as_str(), short.more, short.whole),
            ("short\nlines", None, None)
        );
        let long = "x".repeat(500);
        let cut = block(&format!("exit code 0, 0.1 s\nshort\n{long}"));
        assert_eq!(cut.more.as_deref(), Some("… 100 more characters"));
        assert_eq!(
            cut.whole.as_deref(),
            Some(format!("short\n{long}").as_str())
        );
        let lines: Vec<String> = (1..=17).map(|n| n.to_string()).collect();
        let many = block(&format!("exit code 0, 0.1 s\n{}", lines.join("\n")));
        assert_eq!(many.text.lines().count(), content::PREVIEW_LINES);
        assert_eq!(many.more.as_deref(), Some("… 1 more line"));
    }

    /// A command moved to the background shows as such, with its output so
    /// far, not as a failure.
    #[test]
    fn a_command_moved_to_the_background() {
        let output = "in the background as job 1f0c2d3e-0000-4000-8000-000000000000, 600.0 s\nIt ran longer than 10 minutes, so it moved to the background. It goes on running.\nCompiling arko\nCompiling arko_agents";
        let moved = outcome(SHELL_TOOL_NAME, output);
        assert_eq!(moved.status, CallStatus::Ok);
        assert_eq!(moved.text.as_deref(), Some("in the background"));
        let block = moved.block.unwrap();
        assert_eq!(block.text, "Compiling arko\nCompiling arko_agents");
    }

    /// A `terminal_start` call's line says the job it started runs, and the
    /// call knows the job, by its result's ID.
    #[test]
    fn a_started_job_says_it_runs() {
        let job = "1f0c2d3e-0000-4000-8000-000000000000";
        let started = super::outcome(
            "terminal_start",
            &params(&[("command", Value::from("npm run dev"))]),
            &format!("{{\"job_id\":\"{job}\",\"state\":\"running\"}}"),
        );
        assert_eq!(started.text.as_deref(), Some(JOB_RUNNING));
        assert_eq!(started.job.map(|job| job.to_string()).as_deref(), Some(job));
    }

    /// A command's words are its whole first line, however long, since each
    /// client cuts the row to its width.
    #[test]
    fn a_commands_words_are_whole() {
        let command = format!("cargo test -p arko-tui {}", "a_long_test_name_".repeat(6));
        let words = command_words_kept(&command, "");
        assert_eq!(words.words, command);
        assert!(words.kept);
    }
}

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

use super::content;
use super::feed::{Act, ActKind, CallStatus, Came, ContentKind, Item, Kind};
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
        let path = text(name);
        if path.is_empty() {
            ".".to_string()
        } else {
            path
        }
    };
    let job = || {
        let id = text("job_id");
        format!("job {}", id.get(..8).unwrap_or(&id))
    };
    match tool {
        READ_FILE_TOOL_NAME | "Read" => {
            let name = if tool == "Read" { "file_path" } else { "path" };
            on(ActKind::Read, path(name))
        }
        LIST_FILES_TOOL_NAME => on(ActKind::List, path("directory")),
        WRITE_FILE_TOOL_NAME => on(ActKind::Write, path("path")),
        "Write" => on(ActKind::Write, path("file_path")),
        EDIT_FILE_TOOL_NAME | APPLY_DIFF_TOOL_NAME => on(ActKind::Edit, path("path")),
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
        "index_search" => Act {
            query: cut(&text("query"), ROW_QUERY_CHARS),
            detail: long(&raw("query"), ROW_QUERY_CHARS),
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
        "terminal_status" => on(ActKind::Check, job()),
        "terminal_control" => match text("action").as_str() {
            "wait" => on(ActKind::Check, job()),
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
    /// They state the command as it is: one line, no variables, no `cd`, no
    /// program by its path, and it fit.
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

/// A command as its row names it, by its first line, at most
/// [`quiet::COMMAND_CHARS`] characters, with a program run by its path named
/// by its file name, and after a `cd dir &&` that becomes "in dir", as the
/// folder `cwd` does, with what the words keep of the command and which
/// folder they name.
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
    let words = cut(&line, quiet::COMMAND_CHARS);
    let kept = !command.is_empty() && words == command;
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

/// How the line of a call that moved to the background starts, before
/// its job's first characters, as `in the background · job 4d2c1a9b`.
pub const MOVED_OUTCOME: &str = "in the background · job ";

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

/// A moved call's line, as `in the background · job 4d2c1a9b`.
fn moved_outcome(job: Uuid) -> String {
    let job = job.to_string();
    format!("{MOVED_OUTCOME}{}", &job[..8])
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
            settled: Settled::default(),
        }
    }

    /// A call that failed with `message` (an error, a rejection), meaning its
    /// first words on the call's line ("refused" for a rejection), and the
    /// whole message as its content, muted. A stop's and a compaction's
    /// words are all there is to say.
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
        // A refusal, as the feed words it ("refused (outside access is
        // off)"), says why as a sentence of its own.
        let why = message
            .strip_prefix("refused (")
            .and_then(|rest| rest.strip_suffix(')'));
        let (text, said) = match why {
            Some(why) => (
                "refused".to_string(),
                format!("Refused. {}", upper_first(why)),
            ),
            None => (
                cut(message.lines().next().unwrap_or_default(), OUTCOME_CHARS),
                message.to_string(),
            ),
        };
        let block = (![quiet::STOPPED, quiet::HELD, quiet::INTERRUPTED, "refused"]
            .contains(&message))
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
    if tool == "monitor_start"
        && let Some((monitor, running)) = started_monitor(output)
    {
        let mut outcome = Outcome::ok(if running { MONITORING } else { "ended at once" });
        outcome.output = Some(output.into());
        outcome.job = Some(monitor);
        return outcome;
    }
    let mut outcome = brief(tool, output);
    outcome.job = job;
    if tool != SHELL_TOOL_NAME && outcome.output.is_none() && !output.trim().is_empty() {
        outcome.output = Some(output.into());
    }
    if outcome.block.is_none() && tool != SHELL_TOOL_NAME {
        outcome.block = content(tool, parameters, output).map(Content::block);
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
                    block: None,
                    output: None,
                    job: None,
                    started: None,
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
                    ..Outcome::ok(id)
                },
                (Some("Queued"), Some(id)) => Outcome {
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
        crew::MESSAGE_AGENT if output.starts_with("Sent") => Outcome::ok("sent"),
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
    /// `text` as a content of `kind`, without its blank lines at the end,
    /// and `None` when it's blank.
    fn new(kind: ContentKind, text: impl Into<String>, language: Option<String>) -> Option<Self> {
        let text = text.into();
        let text = text.trim_end().trim_start_matches(['\n', '\r']);
        (!text.is_empty()).then(|| Self {
            kind,
            text: text.to_string(),
            language,
        })
    }

    /// Its block, which shows its preview, and keeps the whole when the
    /// preview leaves some out.
    fn block(self) -> Item {
        let (preview, more) = content::preview(&self.text);
        let mut block = Item::new(Kind::Result, preview);
        block.content = Some(self.kind);
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
fn content(tool: &str, parameters: &Parameters, output: &str) -> Option<Content> {
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
            Content::new(ContentKind::Lines, lines.join("\n"), language(path))
        }
        EDIT_FILE_TOOL_NAME => {
            let diff = edit_diff(given("old_string"), given("new_string"), output);
            Content::new(ContentKind::Diff, diff, language(given("path")))
        }
        "Edit" => {
            let diff = edit_diff(given("old_string"), given("new_string"), "");
            Content::new(ContentKind::Diff, diff, language(given("file_path")))
        }
        "MultiEdit" => {
            let edits = parameters.get("edits").and_then(Value::as_array);
            let diff: Vec<String> = edits
                .into_iter()
                .flatten()
                .map(|edit| {
                    let part = |name: &str| edit.get(name).and_then(Value::as_str).unwrap_or("");
                    edit_diff(part("old_string"), part("new_string"), "")
                })
                .collect();
            Content::new(
                ContentKind::Diff,
                diff.join("\n"),
                language(given("file_path")),
            )
        }
        LIST_FILES_TOOL_NAME | "Grep" | "Glob" | "Bash" => result(ContentKind::Plain),
        "index_search" => {
            let hits = crate::index::read_search_hits(output)?;
            Content::new(ContentKind::Plain, hits.join("\n"), None)
        }
        LOAD_SKILL_TOOL_NAME | "monitor_start" | "terminal_start" => None,
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

/// The diff of an edit that replaced `old` with `new`, with the lines of
/// the file around it that its result `output` shows (`edit_file`'s
/// "Edited a.rs at lines 3-5:" and the lines numbered as `read_file` numbers
/// them). The removed lines have their numbers in the file before the edit
/// and the others theirs after it. An edit whose result shows no lines
/// (one that replaced every match, or Claude Code's) is its old and new
/// lines without numbers.
fn edit_diff(old: &str, new: &str, output: &str) -> String {
    use content::Mark;
    let bare = || {
        let removed = old
            .lines()
            .map(|line| content::diff_line(Mark::Removed, None, line));
        let added = new
            .lines()
            .map(|line| content::diff_line(Mark::Added, None, line));
        removed.chain(added).collect::<Vec<_>>().join("\n")
    };
    let mut lines = output.lines();
    let Some(range) = lines
        .next()
        .and_then(|first| first.strip_prefix("Edited "))
        .and_then(|rest| rest.rsplit_once(" at line"))
        .map(|(_, range)| range.trim_start_matches('s').trim().trim_end_matches(':'))
    else {
        return bare();
    };
    let Some(first) = range
        .split('-')
        .next()
        .and_then(|n| n.trim().parse::<usize>().ok())
    else {
        return bare();
    };
    // The file's lines it shows, by number.
    let shown: Vec<(usize, &str)> = lines
        .filter_map(|line| {
            let (number, text) = line.split_once(" | ")?;
            Some((number.parse().ok()?, text))
        })
        .collect();
    let at = |number: usize| {
        shown
            .iter()
            .find(|(n, _)| *n == number)
            .map(|(_, text)| *text)
    };
    // (One line break at the end of either names no line of its own.)
    let new_core = new.strip_suffix('\n').unwrap_or(new);
    let old_core = old.strip_suffix('\n').unwrap_or(old);
    // The new lines as the file has them, and the old ones they replaced,
    // whole lines both.
    let (removed, added): (Vec<String>, Vec<String>) = if new.is_empty() {
        (old.lines().map(str::to_string).collect(), Vec::new())
    } else {
        let count = new_core.split('\n').count();
        let span: Option<Vec<&str>> = (first..first + count).map(at).collect();
        match span {
            Some(span) => {
                let after = span.join("\n");
                match after.find(new_core) {
                    Some(start) => {
                        let before = format!(
                            "{}{old_core}{}",
                            &after[..start],
                            &after[start + new_core.len()..]
                        );
                        (
                            before.split('\n').map(str::to_string).collect(),
                            span.iter().map(|line| line.to_string()).collect(),
                        )
                    }
                    None => return bare(),
                }
            }
            None => return bare(),
        }
    };
    let last = first + added.len();
    let mut diff: Vec<String> = Vec::new();
    for (number, text) in shown.iter().filter(|(n, _)| *n < first) {
        diff.push(content::diff_line(Mark::Kept, Some(*number), text));
    }
    for (at, text) in removed.iter().enumerate() {
        diff.push(content::diff_line(Mark::Removed, Some(first + at), text));
    }
    for (at, text) in added.iter().enumerate() {
        diff.push(content::diff_line(Mark::Added, Some(first + at), text));
    }
    for (number, text) in shown.iter().filter(|(n, _)| *n >= last) {
        diff.push(content::diff_line(Mark::Kept, Some(*number), text));
    }
    diff.join("\n")
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
            "cargo" | "cargo-locked" => matches!(then().first(), Some(&("test" | "nextest"))),
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
            let outcome = if fixture.status == CallStatus::Failed && fixture.tool != SHELL_TOOL_NAME
                || fixture.output.starts_with("refused")
            {
                Outcome::failed(fixture.output)
            } else {
                super::outcome(fixture.tool, &parameters, fixture.output)
            };
            outcome.settled.apply(&mut item.act);
            assert_eq!(item.act.as_ref(), Some(&fixture.act), "{name}");
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

    /// An edit's diff has the lines around it the result shows, kept, the
    /// old lines, removed, numbered as the file had them, and the new ones,
    /// added, numbered as it has them now. An edit inside a line shows
    /// that line whole, and a deletion has no added lines.
    #[test]
    fn an_edits_diff_shows_whole_lines_around_it() {
        let output = "Edited a.rs at lines 3-4:\n1 | one\n2 | two\n3 | new a\n4 | new b\n5 | five";
        assert_eq!(
            edit_diff("old", "new a\nnew b", output),
            " 1\tone\n 2\ttwo\n-3\told\n+3\tnew a\n+4\tnew b\n 5\tfive"
        );
        let output = "Edited a.rs at line 2:\n1 | fn a() {\n2 |     let x = Some(1);\n3 | }";
        assert_eq!(
            edit_diff("None", "Some(1)", output),
            " 1\tfn a() {\n-2\t    let x = None;\n+2\t    let x = Some(1);\n 3\t}"
        );
        let output = "Edited a.rs at line 2:\n1 | one\n2 | three";
        assert_eq!(
            edit_diff("two\n", "", output),
            " 1\tone\n-2\ttwo\n 2\tthree"
        );
        // Without the file's lines (every match, Claude Code's edit).
        assert_eq!(edit_diff("a\nb", "c", ""), "-\ta\n-\tb\n+\tc");
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
        assert_eq!(item.text, "message main “Done?”");
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
                "/home/me/Desktop/arko/target/debug/arko --resume 20261027 --remote 127.0.0.1:1"
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
                "set -o pipefail; CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only cargo-locked test --lib".into(),
            )],
        );
        assert_eq!(set.target, "cargo-locked test --lib");
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
        assert_eq!(
            moved.text.as_deref(),
            Some("in the background · job 1f0c2d3e")
        );
        let block = moved.block.unwrap();
        assert_eq!(block.text, "Compiling arko\nCompiling arko_agents");
    }
}

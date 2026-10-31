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

use arko_agents::LOAD_SKILL_TOOL_NAME;

use super::feed::{Act, ActKind, CallStatus, Came, Item, Kind, Level};
use super::quiet;
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
///
/// `running` are the runs the calling agent has going, which a
/// `wait_for_agents` call that names none waits for.
pub fn call(tool: &str, parameters: &Parameters, running: &[String]) -> Option<Item> {
    let mut item = line(tool, parameters, running)?;
    if item.kind == Kind::Call {
        item.tool = Some(tool.to_string());
        // (In the order of their names: a `HashMap` has none.)
        let sorted: std::collections::BTreeMap<_, _> = parameters.iter().collect();
        item.parameters = serde_json::to_string_pretty(&sorted).ok().map(Into::into);
        item.act = Some(act(tool, parameters, running));
    }
    Some(item)
}

/// What a call of `tool` does, for the Quiet view's row ([`quiet`]).
fn act(tool: &str, parameters: &Parameters, running: &[String]) -> Act {
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
        READ_FILE_TOOL_NAME | "Read" => on(
            ActKind::Read,
            path(if tool == "Read" { "file_path" } else { "path" }),
        ),
        LIST_FILES_TOOL_NAME => on(ActKind::List, path("directory")),
        WRITE_FILE_TOOL_NAME => on(ActKind::Write, path("path")),
        "Write" => on(ActKind::Write, path("file_path")),
        EDIT_FILE_TOOL_NAME | APPLY_DIFF_TOOL_NAME => on(ActKind::Edit, path("path")),
        "Edit" | "MultiEdit" => on(ActKind::Edit, path("file_path")),
        SHELL_TOOL_NAME | "Bash" => Act {
            alone: runs_tests(&raw("command")),
            ..on(
                ActKind::Command,
                command_words(&raw("command"), &text("cwd")),
            )
        },
        "index_search" => Act {
            query: cut(&text("query"), 60),
            ..on(ActKind::Search, index_name(&text("index")))
        },
        "Grep" | "Glob" => Act {
            query: cut(&text("pattern"), 60),
            ..on(ActKind::Search, String::new())
        },
        LOAD_SKILL_TOOL_NAME => on(ActKind::Skill, text("name")),
        crew::MESSAGE_AGENT => on(ActKind::Message, text("to")),
        crew::WAIT_FOR_AGENTS => on(
            ActKind::Wait,
            ui::waited_runs(parameters, running).join(", "),
        ),
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
            let command = command_words(&raw("command"), "");
            other(
                format!("Starting {command} in a terminal"),
                format!("Started {command} in a terminal"),
                format!("start {command} in a terminal"),
            )
        }
        "monitor_start" => {
            let command = command_words(&raw("command"), "");
            other(
                format!("Starting to watch {command}"),
                format!("Started watching {command}"),
                format!("watch {command}"),
            )
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
            other(
                format!("Changing note {number}"),
                format!("Changed note {number}"),
                format!("change note {number}"),
            )
        }
        "keep_note" => on(
            ActKind::Note,
            match text("audience").as_str() {
                "" | "lead" => String::new(),
                audience => audience.to_string(),
            },
        ),
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
            match text("action").as_str() {
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
            let worktree = if flag(parameters, "remove_workspace") {
                " and removed its worktree"
            } else {
                ""
            };
            other(
                format!("Stopping {run}"),
                format!("Stopped {run}{worktree}"),
                format!("stop {run}"),
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
                "estimate" => {
                    return other(
                        format!("Estimating embedding {index}"),
                        format!("Estimated embedding {index}"),
                        format!("estimate embedding {index}"),
                    );
                }
                "monitor_on" => {
                    return other(
                        format!("Turning on watching for {index}"),
                        format!("Turned on watching for {index}"),
                        format!("turn on watching for {index}"),
                    );
                }
                "monitor_off" => {
                    return other(
                        format!("Turning off watching for {index}"),
                        format!("Turned off watching for {index}"),
                        format!("turn off watching for {index}"),
                    );
                }
                "remove" => ("Removing", "Removed", "remove"),
                _ => ("Changing", "Changed", "change"),
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
            other(
                format!("Fetching {host}"),
                format!("Fetched {host}"),
                format!("fetch {host}"),
            )
        }
        "WebSearch" => {
            let query = format!("“{}”", cut(&text("query"), 60));
            other(
                format!("Searching the web for {query}"),
                format!("Searched the web for {query}"),
                format!("search the web for {query}"),
            )
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

/// A command as its row names it: its first line, at most
/// [`quiet::COMMAND_CHARS`] characters, with a program run by its path named
/// by its file name, and after a `cd dir &&` that becomes "in dir", as the
/// folder `cwd` does.
fn command_words(command: &str, cwd: &str) -> String {
    let mut line = command.trim().lines().next().unwrap_or_default().trim();
    let mut folder = match cwd.trim() {
        "" | "." => String::new(),
        cwd => last_part(cwd),
    };
    if let Some(rest) = line.strip_prefix("cd ")
        && let Some((dir, then)) = rest.split_once("&&")
    {
        folder = last_part(dir.trim().trim_matches(['"', '\'']));
        line = then.trim();
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
    if folder.is_empty() {
        words
    } else {
        format!("{words} in {folder}")
    }
}

/// The last part of a path: `web` of `../arko/web`.
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
fn line(tool: &str, parameters: &Parameters, running: &[String]) -> Option<Item> {
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
        crew::WAIT_FOR_AGENTS => call(ui::waiting_for(parameters, running)),
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

/// How the line of a `monitor_start` call starts while its monitor runs:
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
    alone: bool,
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
            started: None,
            settled: Settled::default(),
        }
    }
}

impl Settled {
    /// How a job a call started (a command that went on in the background,
    /// a monitor) went on: what its result first said no longer stands.
    pub fn job() -> Self {
        Self {
            news: Some(String::new()),
            ..Self::default()
        }
    }

    /// What the search of a call found: the indexes it searched, and
    /// "no results" when it had none.
    pub fn found(&mut self, found: crate::index::Found) {
        if !found.indexes.is_empty() {
            self.index = Some(found.indexes.join(", "));
        }
        if found.hits == 0 {
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
        act.alone |= settled.alone;
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
        crew::MESSAGE_AGENT if output.starts_with("Sent") => Outcome::ok("sent"),
        // Its result is the mail that came: "[mail 2] r1 ... finished ...".
        crew::WAIT_FOR_AGENTS => {
            let mail = output
                .lines()
                .filter(|line| line.starts_with("[mail "))
                .count();
            let mut outcome = if mail > 0 {
                Outcome::ok(plural(mail, "message", "messages"))
            } else {
                Outcome::ok(cut(
                    output.lines().next().unwrap_or_default(),
                    OUTCOME_CHARS,
                ))
            };
            outcome.settled.came = Some(came(output));
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
/// whatever it prints.
fn runs_tests(command: &str) -> bool {
    let words: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|word| !word.is_empty())
        .map(|word| word.rsplit('/').next().unwrap_or(word))
        .collect();
    let alone = [
        "pytest", "jest", "vitest", "mocha", "rspec", "phpunit", "ctest", "tox", "nox",
    ];
    words.iter().any(|word| alone.contains(word))
        || words.windows(2).any(|pair| match pair {
            [runner, "test" | "nextest"] => matches!(
                *runner,
                "cargo"
                    | "cargo-locked"
                    | "go"
                    | "npm"
                    | "pnpm"
                    | "yarn"
                    | "bun"
                    | "deno"
                    | "dotnet"
                    | "mvn"
                    | "gradle"
                    | "./gradlew"
                    | "gradlew"
                    | "make"
                    | "swift"
                    | "mix"
                    | "zig"
            ),
            ["-m", "pytest" | "unittest"] => true,
            _ => false,
        })
        || words
            .windows(3)
            .any(|three| matches!(three, ["npm" | "pnpm" | "yarn" | "bun", "run", "test"]))
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
    // Its row says how its tests went, or how it failed, and that it ran.
    let settled = Settled {
        news: Some(match (&tests, how.strip_prefix("exit code ")) {
            (Some(tests), _) => tests.clone(),
            (None, _) if ok => String::new(),
            (None, Some(code)) => format!("exit {code}"),
            (None, None) => how.to_string(),
        }),
        alone: tests.is_some(),
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
    let mut shown: Vec<String> = Vec::new();
    let mut left_out = rest.len() > SHOWN_LINES;
    if !(rest.len() == 1 && rest[0] == "(no output)") {
        shown.extend(rest.iter().take(SHOWN_LINES).map(|line| {
            let shown = cut(line, 300);
            left_out |= shown != *line;
            shown
        }));
        if rest.len() > SHOWN_LINES {
            shown.push(ui::more_lines(rest.len() - SHOWN_LINES));
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
        started: None,
        settled,
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

    /// A call of an agent with no runs going.
    fn call(tool: &str, parameters: &Parameters) -> Option<Item> {
        super::call(tool, parameters, &[])
    }

    /// A wait says what it waits for, with the runs its agent has going
    /// when it names none (for no runs, a message).
    #[test]
    fn a_wait_names_the_runs_it_waits_for() {
        let wait = |pairs: &[(&str, Value)], running: &[&str]| {
            let running: Vec<String> = running.iter().map(|run| run.to_string()).collect();
            let item = super::call(crew::WAIT_FOR_AGENTS, &params(pairs), &running).unwrap();
            assert_eq!(item.kind, Kind::Call);
            assert_eq!(item.tool.as_deref(), Some(crew::WAIT_FOR_AGENTS));
            item.text
        };
        assert_eq!(wait(&[], &[]), "waiting for a message");
        assert_eq!(wait(&[("all", true.into())], &[]), "waiting for a message");
        assert_eq!(wait(&[], &["r1"]), "waiting for r1");
        assert_eq!(wait(&[], &["r1", "r2"]), "waiting for r1, r2");
        assert_eq!(
            wait(&[], &["r1", "r2", "r3", "r4", "r5"]),
            "waiting for r1, r2 and 3 more"
        );
        // The ids a call names are what it waits for, also when they ended.
        assert_eq!(
            wait(&[("runs", "r3, r4".into())], &["r1", "r2"]),
            "waiting for r3, r4"
        );
        assert_eq!(
            wait(&[("runs", serde_json::json!(["r1", "r2"]))], &[]),
            "waiting for r1, r2"
        );
        // The result of such a wait says the run is over.
        let outcome = outcome(
            crew::WAIT_FOR_AGENTS,
            "Nothing to wait for: r1 is finished. Reports that came earlier reached you already.",
        );
        assert_eq!(outcome.status, CallStatus::Ok);
        assert!(
            outcome
                .text
                .is_some_and(|text| text.starts_with("Nothing to wait for: r1 is finished."))
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
        assert_eq!(item.detail.as_deref(), Some(task.trim()));
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
        assert_eq!(item.text, "message lead “Done?”");
    }

    /// Only a start that began the worker at once names it as started: one
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

    /// Each call says what it does for the Quiet view, and its result
    /// completes that: a test run's counts, the index a search used, what
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
        let message = act(
            crew::MESSAGE_AGENT,
            &[("to", "lead".into()), ("message", "Done.".into())],
        );
        assert_eq!(
            (message.kind, message.target.as_str()),
            (ActKind::Message, "lead")
        );
        let wait = super::call(
            crew::WAIT_FOR_AGENTS,
            &params(&[]),
            &["r3".into(), "r4".into()],
        )
        .and_then(|item| item.act)
        .unwrap();
        assert_eq!((wait.kind, wait.target.as_str()), (ActKind::Wait, "r3, r4"));
        let start = act(crew::START_AGENT, &[("task", "Fix it.".into())]);
        assert_eq!(
            (start.kind, start.doing.as_str(), start.verb.as_str()),
            (ActKind::Other, "Starting a worker", "start a worker")
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
        assert_eq!((built.news.as_str(), built.alone), ("exit 2", true));
        let quiet = settled(SHELL_TOOL_NAME, "exit code 0, 0.1 s\n(no output)", shell);
        assert_eq!((quiet.news.as_str(), quiet.ran), ("", true));
        // A search's indexes and its count come from the search itself.
        let search = act("index_search", &[("query", "state in session".into())]);
        let found = |indexes: &[&str], hits: usize| {
            let mut found = Settled::default();
            found.found(crate::index::Found {
                indexes: indexes.iter().map(|index| index.to_string()).collect(),
                hits,
            });
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

    /// The line that counts what a command's block leaves out says "line"
    /// for one.
    #[test]
    fn a_block_counts_the_lines_it_leaves_out() {
        let left_out = |lines: usize| {
            let output: Vec<String> = (1..=lines).map(|n| n.to_string()).collect();
            outcome(
                SHELL_TOOL_NAME,
                &format!("exit code 0, 0.1 s\n{}", output.join("\n")),
            )
            .block
            .and_then(|block| block.detail)
            .and_then(|detail| detail.lines().last().map(str::to_string))
        };
        assert_eq!(left_out(5).as_deref(), Some("… 1 more line"));
        assert_eq!(left_out(7).as_deref(), Some("… 3 more lines"));
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

//! Code runs as the feed gets them, one in each state a run's row can be
//! in, with the conversation a session saves of them, for the tests of the
//! feed, the reopened session, the terminal and the page.

use std::collections::HashMap;

use arko_agents::{AgentEvent, NestedCallOutcome};
use arko_ai::{Content, Message, ToolCall, ToolResult};
use arko_workflows::WorkflowEvent;
use serde_json::{Value, json};
use uuid::Uuid;

use super::feed::Feed;
use crate::code_mode::RUN_CODE;

/// How a run ends, or that it still runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// It returned, its code longer than a preview shows.
    Done,
    /// A call of its code failed, and the code threw.
    Failed,
    /// A call of its code was refused, and the code went on.
    Refused,
    /// A stop ended it while a call of its code ran.
    Stopped,
    /// A call of its code runs.
    Running,
}

/// Every stage, in the order a transcript shows them, the one that runs
/// last.
pub(crate) const STAGES: [Stage; 5] = [
    Stage::Done,
    Stage::Failed,
    Stage::Refused,
    Stage::Stopped,
    Stage::Running,
];

/// A call the code makes, how it ends (none while it runs), and its result.
struct Nested {
    tool: &'static str,
    parameters: Value,
    ended: Option<(NestedCallOutcome, String)>,
}

/// A run, as the model wrote it and as it went.
pub(crate) struct Run {
    pub id: Uuid,
    pub purpose: &'static str,
    pub code: String,
    calls: Vec<Nested>,
    /// Its result, `Err` when it failed, none while it runs or once stopped.
    result: Option<Result<String, String>>,
    stage: Stage,
}

fn ok(text: &str) -> Option<(NestedCallOutcome, String)> {
    Some((NestedCallOutcome::Finished, text.to_string()))
}

fn read(path: &str) -> Nested {
    Nested {
        tool: "read_file",
        parameters: json!({ "path": path }),
        ended: ok("1 | // TODO: split the loop\n2 | fn main() {}\n[Lines 1-2 of 2.]"),
    }
}

fn shell(command: &str, ended: Option<(NestedCallOutcome, String)>) -> Nested {
    Nested {
        tool: "shell",
        parameters: json!({ "command": command }),
        ended,
    }
}

impl Run {
    pub(crate) fn of(stage: Stage) -> Self {
        let list = Nested {
            tool: "list_files",
            parameters: json!({ "directory": "crates" }),
            ended: ok("crates/\n  arko_agents/\n  arko_ai/\n  arko_index/"),
        };
        let (purpose, code, calls, result): (_, &str, _, _) = match stage {
            Stage::Done => (
                "count TODO comments per crate",
                "const crates = [\"arko_agents\", \"arko_ai\", \"arko_index\"];\n\
                 const hits = [];\n\
                 for (const name of crates) {\n\
                 \x20 const listed = await tools.list_files({ directory: `crates/${name}/src` });\n\
                 \x20 const paths = listed\n\
                 \x20   .split(\"\\n\")\n\
                 \x20   .map((line) => line.trim())\n\
                 \x20   .filter((line) => line.endsWith(\".rs\"));\n\
                 \x20 let count = 0;\n\
                 \x20 for (const path of paths) {\n\
                 \x20   const text = await tools.read_file({ path: `crates/${name}/src/${path}` });\n\
                 \x20   count += (text.match(/TODO/g) ?? []).length;\n\
                 \x20 }\n\
                 \x20 if (count) {\n\
                 \x20   hits.push(`${count} ${name}`);\n\
                 \x20 }\n\
                 }\n\
                 // Most first.\n\
                 hits.sort((a, b) => parseInt(b) - parseInt(a));\n\
                 return hits.join(\"\\n\");",
                vec![
                    list,
                    read("crates/arko_agents/src/runtime/loop.rs"),
                    read("crates/arko_ai/src/model.rs"),
                    read("crates/arko_index/src/lib.rs"),
                ],
                Some(Ok("31 arko_agents\n12 arko_ai\n4 arko_index".to_string())),
            ),
            Stage::Failed => (
                "rename the setting in every crate",
                "for (const path of [\"crates/arko_ai/src/config.rs\", \"crates/arko_ui/src/config.rs\"]) {\n\
                 \x20 const text = await tools.read_file({ path });\n\
                 \x20 await tools.write_file({ path, content: text.replaceAll(\"keep_warm\", \"warm\") });\n\
                 }",
                vec![
                    read("crates/arko_ai/src/config.rs"),
                    Nested {
                        tool: "write_file",
                        parameters: json!({ "path": "crates/arko_ai/src/config.rs", "content": "warm" }),
                        ended: ok("Wrote crates/arko_ai/src/config.rs"),
                    },
                    Nested {
                        tool: "read_file",
                        parameters: json!({ "path": "crates/arko_ui/src/config.rs" }),
                        ended: Some((
                            NestedCallOutcome::Failed,
                            "No such file or directory (os error 2)".into(),
                        )),
                    },
                ],
                Some(Err(
                    "The code threw on line 2, at a call of read_file that failed: No such file or directory (os error 2)\nBefore it ended, the code wrote 1 file."
                        .to_string(),
                )),
            ),
            Stage::Refused => (
                "clean the build folders",
                "for (const name of [\"target\", \"web/dist\"]) {\n\
                 \x20 try {\n\
                 \x20   await tools.shell({ command: `rm -rf ${name}` });\n\
                 \x20 } catch (error) {\n\
                 \x20   console.log(`${name}: ${error.message}`);\n\
                 \x20 }\n\
                 }\n\
                 return \"Done.\";",
                vec![
                    shell(
                        "rm -rf target",
                        Some((
                            NestedCallOutcome::Refused,
                            "The command guard refused it.".into(),
                        )),
                    ),
                    shell("rm -rf web/dist", ok("exit code 0, 0.1 s")),
                ],
                Some(Ok(
                    "target: refused (The command guard refused it.)\nDone.".to_string()
                )),
            ),
            Stage::Stopped => (
                "build each crate",
                "for (const name of [\"arko_ai\", \"arko_agents\"]) {\n\
                 \x20 await tools.shell({ command: `cargo build -p ${name}` });\n\
                 }",
                vec![
                    shell("cargo build -p arko_ai", ok("exit code 0, 41.2 s")),
                    shell(
                        "cargo build -p arko_agents",
                        Some((NestedCallOutcome::Stopped, String::new())),
                    ),
                ],
                None,
            ),
            Stage::Running => (
                "run each crate's tests",
                "for (const name of [\"arko_index\", \"arko_ai\"]) {\n\
                 \x20 const output = await tools.shell({ command: `cargo test -p ${name}` });\n\
                 \x20 console.log(output.split(\"\\n\").pop());\n\
                 }",
                vec![
                    list,
                    shell("cargo test -p arko_index", ok("exit code 0, 12.0 s\ntest result: ok. 24 passed; 0 failed")),
                    shell("cargo test -p arko_ai", None),
                ],
                None,
            ),
        };
        Self {
            id: Uuid::new_v4(),
            purpose,
            code: code.to_string(),
            calls,
            result,
            stage,
        }
    }

    /// Its call's parameters.
    pub(crate) fn parameters(&self) -> HashMap<String, Value> {
        [
            ("code".to_string(), Value::from(self.code.as_str())),
            ("purpose".to_string(), Value::from(self.purpose)),
        ]
        .into()
    }

    /// Sends `feed` the run's events as the main agent's, its call, its
    /// code's calls and how it ended.
    pub(crate) fn send(&self, feed: &Feed) {
        let send = |event: AgentEvent| feed.event(&workflow_event(event), None);
        send(AgentEvent::ToolCallFormatted {
            tool_call_id: self.id,
            tool_name: RUN_CODE.into(),
            parameters: self.parameters(),
        });
        for nested in &self.calls {
            let id = Uuid::new_v4();
            let parameters: HashMap<String, Value> = nested
                .parameters
                .as_object()
                .map(|object| object.clone().into_iter().collect())
                .unwrap_or_default();
            send(AgentEvent::NestedToolCallStarted {
                outer_tool_call_id: self.id,
                tool_call_id: id,
                tool_name: nested.tool.into(),
                parameters_bytes: nested.parameters.to_string().len(),
                parameters,
            });
            if let Some((outcome, output)) = &nested.ended {
                send(AgentEvent::NestedToolCallFinished {
                    outer_tool_call_id: self.id,
                    tool_call_id: id,
                    tool_name: nested.tool.into(),
                    outcome: *outcome,
                    output: output.clone(),
                    output_bytes: output.len(),
                });
            }
        }
        match (&self.result, self.stage) {
            (Some(Ok(output)), _) => send(AgentEvent::ToolResult {
                tool_call_id: self.id,
                tool_name: RUN_CODE.into(),
                output: output.clone(),
                receipt: None,
            }),
            (Some(Err(why)), _) => send(AgentEvent::ErrorEncountered {
                error_message: why.clone(),
                requires_approval_to_retry: false,
                tool_call_id: Some(self.id),
                error_kind: None,
            }),
            (None, Stage::Stopped) => send(AgentEvent::ToolRejected {
                tool_call_id: self.id,
                tool_name: RUN_CODE.into(),
                reason: Some(STOPPED_BY_CANCEL.into()),
            }),
            (None, _) => {}
        }
    }

    /// What a session saves of the run, the model's call and its result.
    pub(crate) fn messages(&self) -> Vec<Message> {
        let call: ToolCall = serde_json::from_value(json!({
            "id": self.id.to_string(),
            "name": RUN_CODE,
            "arguments": { "code": self.code, "purpose": self.purpose },
        }))
        .unwrap();
        let mut asked = Message::new_assistant("");
        asked.content.clear();
        asked.tool_calls = vec![call];
        let (output, is_error) = match (&self.result, self.stage) {
            (Some(Ok(output)), _) => (output.clone(), false),
            (Some(Err(why)), _) => (why.clone(), true),
            (None, Stage::Stopped) => (STOPPED_BY_CANCEL.to_string(), true),
            (None, _) => return vec![asked],
        };
        let result = ToolResult {
            is_error,
            ..ToolResult::new(Some(self.id.to_string()), RUN_CODE, &output)
        };
        vec![
            asked,
            Message::new_user_with_contents(vec![Content::ToolResult(result)]),
        ]
    }
}

/// What the framework says of a call a stop cancelled, as the feed and a
/// saved conversation get it.
const STOPPED_BY_CANCEL: &str = "The task was cancelled while this call was running, so the call was stopped before it finished.";

fn workflow_event(event: AgentEvent) -> WorkflowEvent {
    WorkflowEvent {
        timestamp: chrono::Utc::now(),
        workflow_run_id: Uuid::nil(),
        agent_name: "main".into(),
        agent_run_id: Uuid::nil(),
        event,
    }
}

/// A feed with the user's message, then a run in each stage, the one that
/// runs last, and the runs.
pub(crate) fn feed() -> (Feed, Vec<Run>) {
    let feed = Feed::new("main");
    feed.user(
        "Count the TODO comments, rename the setting, clean up, build and test",
        super::Source::Terminal,
        super::Delivery::Turn,
    );
    let runs: Vec<Run> = STAGES.into_iter().map(Run::of).collect();
    for run in &runs {
        run.send(&feed);
    }
    (feed, runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::Images;
    use crate::remote::feed::{CallStatus, ContentKind, Item, Kind};
    use crate::remote::quiet::{self, Piece, Step};

    /// The items of the run `id` in `items`, its call and the blocks under it.
    fn of(items: &[Item], id: u64) -> (Item, Vec<Item>) {
        let call = items.iter().find(|item| item.id == id).unwrap().clone();
        let blocks = items
            .iter()
            .filter(|item| item.call == Some(id))
            .cloned()
            .collect();
        (call, blocks)
    }

    /// The Quiet row of the call `call` alone.
    fn words(call: &Item, content: bool) -> quiet::Words {
        let steps = [Step::Call(quiet::Call {
            id: call.id,
            act: call.act.as_ref(),
            status: call.status,
            outcome: call.outcome.as_deref(),
            content,
            ..quiet::Call::default()
        })];
        match quiet::rows(&steps).pop() {
            Some(Piece::Row(row)) => {
                assert!(row.opens && !row.merged);
                row.words
            }
            other => panic!("{other:?}"),
        }
    }

    fn runs(items: &[Item]) -> Vec<u64> {
        items
            .iter()
            .filter(|item| item.tool.as_deref() == Some(RUN_CODE))
            .map(|item| item.id)
            .collect()
    }

    /// Each run's row says what its code is for, in the tense of how it
    /// stands, with ✗ once it failed and "stopped" once a stop ended it, and
    /// while it runs how many calls its code made and the one that runs. It
    /// opens to its code from the start, highlighted as JavaScript, and to
    /// its result once that comes. Open, it says what the calls that went
    /// well did and has a line for each that failed or was refused.
    #[test]
    fn a_runs_row_says_what_its_code_is_for_and_what_its_calls_did() {
        let (feed, _) = feed();
        let items = feed.subscribe().0.items;
        let ids = runs(&items);
        assert_eq!(ids.len(), STAGES.len());
        let mut seen = Vec::new();
        for id in ids {
            let (call, blocks) = of(&items, id);
            let code = &blocks[0];
            assert_eq!(
                (code.content, code.language.as_deref(), code.id),
                (Some(ContentKind::Code), Some("js"), id + 1),
                "the code follows its call"
            );
            let calls = call.act.as_ref().unwrap().calls.clone().unwrap();
            let failed: Vec<&str> = calls
                .lines
                .iter()
                .map(|line| line.failed.as_str())
                .filter(|failed| !failed.is_empty())
                .collect();
            let words = words(&call, true);
            seen.push((
                words.said(),
                words.failed,
                words.muted,
                words.running,
                calls.summary.clone(),
                failed.join(" | "),
                blocks.len(),
            ));
        }
        let row = |said: &str, failed, muted, running, summary: &str, lines: &str, blocks| {
            (
                said.to_string(),
                failed,
                muted,
                running,
                summary.to_string(),
                lines.to_string(),
                blocks,
            )
        };
        assert_eq!(
            seen,
            [
                row(
                    "Ran code to count TODO comments per crate",
                    false,
                    false,
                    false,
                    "Listed 1 folder and read 3 files",
                    "",
                    2
                ),
                row(
                    "Ran code to rename the setting in every crate",
                    true,
                    false,
                    false,
                    "Read 1 file and wrote 1 file",
                    "Couldn’t read config.rs · No such file or directory (os error 2)",
                    2
                ),
                row(
                    "Ran code to clean the build folders",
                    false,
                    false,
                    false,
                    "Ran 1 command",
                    "Couldn’t run rm -rf target · refused",
                    2
                ),
                row(
                    "Ran code to build each crate (stopped)",
                    false,
                    true,
                    false,
                    "Ran 1 command",
                    "",
                    1
                ),
                row(
                    "Running code to run each crate's tests · 3 tool calls · running cargo test -p arko_ai",
                    false,
                    false,
                    true,
                    "Listed 1 folder and ran 1 command",
                    "",
                    1
                ),
            ]
        );
    }

    /// The code shows as it was written, its first lines and the door to
    /// the rest, and a run's Full line names it by what it's for, with how
    /// many lines its result has.
    #[test]
    fn a_runs_code_shows_as_written_and_its_full_line_says_what_it_is_for() {
        let (feed, runs) = feed();
        let items = feed.subscribe().0.items;
        let (call, blocks) = of(&items, self::runs(&items)[0]);
        assert_eq!(call.text, "run_code count TODO comments per crate");
        assert_eq!(
            (call.status, call.outcome.as_deref()),
            (Some(CallStatus::Ok), Some("3 lines"))
        );
        let code = &blocks[0];
        let lines: Vec<&str> = runs[0].code.lines().collect();
        assert_eq!(code.text, lines[..16].join("\n"));
        assert_eq!(code.more.as_deref(), Some("… 4 more lines"));
        assert_eq!(code.whole.as_deref(), Some(runs[0].code.as_str()));
        assert_eq!(
            (blocks[1].content, blocks[1].text.as_str()),
            (
                Some(ContentKind::Muted),
                "31 arko_agents\n12 arko_ai\n4 arko_index"
            )
        );
        // Each call's Full line, with how it went.
        let calls = call.act.as_ref().unwrap().calls.clone().unwrap();
        let full: Vec<(String, CallStatus, String)> = calls
            .lines
            .iter()
            .map(|line| (line.text.clone(), line.status, line.outcome.clone()))
            .collect();
        assert_eq!(
            full[0],
            ("list crates".into(), CallStatus::Ok, "4 entries".into())
        );
        assert_eq!(
            full[1],
            (
                "read crates/arko_agents/src/runtime/loop.rs".into(),
                CallStatus::Ok,
                "2 lines".into()
            )
        );
        // Leading blank lines and indents stay, so the code's line numbers
        // are its own.
        let indented = Run {
            code: "\n  return 1;\n\n".into(),
            ..Run::of(Stage::Done)
        };
        let block = crate::remote::render::code_block(RUN_CODE, &indented.parameters()).unwrap();
        assert_eq!(block.text, "\n  return 1;");
    }

    /// A run of more calls than a run may make keeps a line for each it may
    /// make and counts the rest, so the feed holds a bounded amount of it.
    #[test]
    fn a_runs_lines_stop_at_the_most_a_run_makes() {
        let feed = Feed::new("main");
        let run = Run {
            calls: (0..600).map(|n| read(&format!("src/file{n}.rs"))).collect(),
            result: Some(Ok("done".into())),
            ..Run::of(Stage::Done)
        };
        run.send(&feed);
        let items = feed.subscribe().0.items;
        let (call, _) = of(&items, runs(&items)[0]);
        let calls = call.act.unwrap().calls.unwrap();
        assert_eq!(calls.count, 600);
        assert_eq!(calls.lines.len(), crate::code_mode::MAX_CALLS as usize);
        assert_eq!(calls.summary, "Read 600 files");
        assert!(calls.running.is_empty());
    }

    /// A worker's run counts as one of its calls, and its code's calls
    /// don't count.
    #[test]
    fn a_workers_run_counts_as_one_call() {
        let feed = Feed::new("main");
        feed.worker("r1", "Count TODOs", crate::remote::WorkerChange::Started);
        let run = Run::of(Stage::Done);
        let send = |event: AgentEvent| feed.event(&workflow_event(event), Some("r1"));
        send(AgentEvent::ToolCallFormatted {
            tool_call_id: run.id,
            tool_name: RUN_CODE.into(),
            parameters: run.parameters(),
        });
        for n in 0..3 {
            let id = Uuid::new_v4();
            send(AgentEvent::NestedToolCallStarted {
                outer_tool_call_id: run.id,
                tool_call_id: id,
                tool_name: "read_file".into(),
                parameters: [("path".to_string(), Value::from(format!("a{n}.rs")))].into(),
                parameters_bytes: 16,
            });
        }
        let worker = feed
            .workers()
            .into_iter()
            .find(|worker| worker.id == "r1")
            .unwrap();
        assert_eq!(worker.tool_calls, 1);
    }

    /// Serves the runs on the page, for screenshots, until it's stopped:
    /// `CODE_RUNS_URL=FILE cargo test --lib serve_code_runs -- --ignored`
    /// writes the page's address in FILE.
    #[cfg(feature = "remote")]
    #[tokio::test]
    #[ignore]
    async fn serve_code_runs() {
        struct Still;
        impl crate::remote::Meter for Still {
            fn read(&self) -> crate::remote::Reading {
                crate::remote::Reading::default()
            }
        }
        const KEY: &str = "Y29kZSBydW5zIG9uIHRoZSBwYWdlIDAx";
        let (feed, _) = feed();
        // With `CODE_RUNS_DOORS`, a command whose output has a door too.
        if std::env::var_os("CODE_RUNS_DOORS").is_some() {
            let id = Uuid::new_v4();
            let send = |event: AgentEvent| feed.event(&workflow_event(event), None);
            send(AgentEvent::ToolCallFormatted {
                tool_call_id: id,
                tool_name: "shell".into(),
                parameters: [("command".to_string(), Value::from("cargo build"))].into(),
            });
            let output: Vec<String> = (1..=30)
                .map(|n| format!("   Compiling crate{n} v0.1.0"))
                .collect();
            send(AgentEvent::ToolResult {
                tool_call_id: id,
                tool_name: "shell".into(),
                output: format!("exit code 0, 41.0 s\n{}", output.join("\n")),
                receipt: None,
            });
        }
        feed.turn_started();
        let root = tempfile::tempdir().unwrap();
        let (actions, _received) = tokio::sync::mpsc::unbounded_channel();
        let server = crate::remote::Server::start(
            "127.0.0.1:0".parse().unwrap(),
            KEY.into(),
            crate::remote::Info {
                model: "claude-opus".into(),
                workspace: "/home/ada/proj".into(),
                session: "code runs".into(),
                app: String::new(),
            },
            feed.clone(),
            Images::default(),
            actions,
            std::sync::Arc::new(Still),
            None,
            crate::remote::Project {
                workspace: root.path().join("work"),
                root: root.path().join("root"),
            },
        )
        .unwrap();
        let url = format!("http://{}/#key={KEY}", server.addr());
        if let Ok(file) = std::env::var("CODE_RUNS_URL") {
            std::fs::write(file, &url).unwrap();
        }
        println!("{url}");
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }

    /// A reopened session shows each run's row, its code and its result as
    /// the live one did. What the code's calls did is live only.
    #[test]
    fn a_reopened_session_shows_a_runs_row_code_and_result_as_it_did_live() {
        let (feed, runs) = feed();
        let live = feed.subscribe().0.items;
        let mut messages = vec![Message::new_user(
            "Count the TODO comments, rename the setting, clean up, build and test",
        )];
        for run in runs.iter().filter(|run| run.stage != Stage::Running) {
            messages.extend(run.messages());
        }
        let reopened = crate::remote::history::items(&messages, false, &Images::default());
        // Each run's lines, its act but for its calls, and its blocks.
        let shown = |items: &[Item]| {
            self::runs(items)
                .into_iter()
                .map(|id| {
                    let (call, blocks) = of(items, id);
                    let act = call
                        .act
                        .clone()
                        .map(|act| crate::remote::Act { calls: None, ..act });
                    let blocks: Vec<_> = blocks
                        .iter()
                        .map(|block| (block.content, block.text.clone(), block.more.clone()))
                        .collect();
                    let said = words(&call, true).said();
                    (call.text, said, call.status, call.outcome, act, blocks)
                })
                .collect::<Vec<_>>()
        };
        let live_shown = shown(&live);
        let reopened_shown = shown(&reopened);
        // (The live session's last run still runs.)
        assert_eq!(reopened_shown[..], live_shown[..4]);
        for item in reopened.iter().filter(|item| item.kind == Kind::Call) {
            assert!(item.act.as_ref().is_none_or(|act| act.calls.is_none()));
        }
    }
}

//! A scripted session for trying the remote page by hand, with Arko's own
//! harness and the mock model: no model is called.

use std::sync::Arc;
use std::time::Duration;

use arko_agents::builtins::ASK_USER_TOOL_NAME;
use arko_workflows::prelude::*;

use crate::Managers;
use crate::agents::{MAIN, WORKER};
use crate::crew;
use crate::remote::{self, Level};
use crate::shell::{Shell, WindowsShell};
use crate::ui::Input;

/// Not a test: serves the web page for a scripted session, to try the
/// page by hand. `ARKO_DEMO_HOST` (127.0.0.1), `ARKO_DEMO_PORT` (8799) and
/// `ARKO_DEMO_SECONDS` (900). `ARKO_DEMO_ITEMS` starts the transcript with
/// that many items of earlier turns, and with `ARKO_DEMO_STAMPS=1` each
/// word "demo:stream" sends ends with the time it was sent (`⟦t=…⟧`, Unix
/// milliseconds), for timing how fast a page shows it.
/// run with `cargo test remote_page_demo -- --ignored --nocapture` and
/// open the link it prints. Messages: "Run the demo" (calls, two
/// workers, a failed command, a question, a report), "demo:stream" (a
/// long reply that streams), "demo:slow" (a 30 s command), "demo:ask" (a
/// question), "demo:long" (3,500 calls, to reach the history's limit;
/// "demo:long 500", 500), anything else (a short turn). Lines added to the
/// file `ARKO_DEMO_INPUT` are typed at the terminal.
/// What starts a script: a message with one of these.
const MARKERS: [&str; 5] = [
    "Run the demo",
    "demo:stream",
    "demo:slow",
    "demo:ask",
    "demo:long",
];

#[derive(Default)]
struct Script {
    /// The markers seen, by how many times.
    known: std::collections::HashMap<&'static str, usize>,
    /// The script running and its next step.
    current: Option<(&'static str, usize)>,
}

#[tokio::test]
#[ignore]
async fn remote_page_demo() {
    use crate::mock_model::{ModelServer, complete, tool_call};
    use crate::shell::SHELL_TOOL_NAME;
    use arko_agents::AgentEvent;
    use arko_agents::builtins::READ_FILE_TOOL_NAME;
    use uuid::Uuid;
    fn turn(id: &str, text: &str, call: Option<(&str, serde_json::Value)>) -> String {
        let mut events = Vec::new();
        if !text.is_empty() {
            events.push(serde_json::json!({"type": "response.output_text.delta", "delta": text}));
            events.push(serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": format!("msg_{id}"), "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            }}));
        }
        let mut stream: String = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect();
        stream.push_str(&match call {
            Some((name, arguments)) => tool_call(id, name, arguments),
            None => complete(id, text),
        });
        stream
    }
    let stamps = std::env::var("ARKO_DEMO_STAMPS").is_ok_and(|value| value == "1");
    let feed_cell: Arc<std::sync::OnceLock<remote::Feed>> = Arc::new(std::sync::OnceLock::new());
    let feed_for_model = feed_cell.clone();
    // The script running: started by its marker in a new message, it goes
    // a step on with each request of the main agent, until it completes.
    let script: Arc<std::sync::Mutex<Script>> = Arc::default();
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        let feed = feed_for_model.clone();
        let script = script.clone();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(900)).await;
            let shell = |command: &str| (SHELL_TOOL_NAME, serde_json::json!({"command": command}));
            if body.contains("You are agent run r") && body.contains("Fix the flaky test") {
                let step = body.matches("function_call_output").count();
                return match step {
                    0 => turn("w10", "Looking at the parser's tests first.", Some(shell("sleep 1; echo 'running 40 tests'; echo 'test parser::tests::flaky ... FAILED'; exit 101"))),
                    1 => turn("w11", "", Some((READ_FILE_TOOL_NAME, serde_json::json!({"path": "src/parser.rs"})))),
                    2 => turn("w12", "The setup races the clock, and waiting on the fixture fixes it.", Some(shell("sleep 2; echo 'test result: ok. 40 passed; 0 failed'"))),
                    3 => turn("w13", "", Some(shell("sleep 2; for i in 1 2 3; do echo \"run $i: ok\"; done"))),
                    _ => complete("w14", "Fixed. The parser test's setup raced the clock.\n\n- The fixture now signals when it is ready.\n- 40 passed, ten runs in a row."),
                };
            }
            if body.contains("You are agent run r") {
                let step = body.matches("function_call_output").count();
                return match step {
                    0 => turn("w20", "", Some(shell("sleep 4; echo 'README: 14 flags documented'"))),
                    _ => complete("w21", "The README's flags are current, except the note on --remote's port."),
                };
            }
            let (marker, step) = {
                let mut script = script.lock().unwrap();
                for marker in MARKERS {
                    let count = body.matches(marker).count();
                    let known = script.known.entry(marker).or_default();
                    if count > *known {
                        *known = count;
                        script.current = Some((marker, 0));
                    }
                }
                match &mut script.current {
                    Some((marker, step)) => {
                        *step += 1;
                        (Some(*marker), *step - 1)
                    }
                    None => (None, 0),
                }
            };
            let after = |name: &str| (marker == Some(name)).then_some(step);
            let done = || script.lock().unwrap().current = None;
            if let Some(step) = after("Run the demo") {
                return match step {
                    0 => turn("d0", "I'll look around first.", Some(shell("ls -la; echo 'running 12 tests'; echo 'test result: ok. 12 passed; 0 failed'"))),
                    1 => turn("d1", "Two workers next, one to fix the flaky test and one to check the docs.", Some((crew::START_AGENT, serde_json::json!({"agent": WORKER, "task": "Fix the flaky test in src/parser.rs\n\nIt fails one run in ten. Find why, fix it, and run it ten times."})))),
                    2 => turn("d2", "", Some((crew::START_AGENT, serde_json::json!({"agent": WORKER, "task": "Review the README for stale flags"})))),
                    3 => turn("d3", "Meanwhile, a build.", Some(shell("sleep 2; echo 'error[E0425]: cannot find value `x` in this scope'; exit 3"))),
                    4 => turn("d4", "", Some((ASK_USER_TOOL_NAME, serde_json::json!({"questions": "Should I also update the **CHANGELOG**? It has no entry for 0.4 yet."})))),
                    5 => turn("d5", "Waiting for the workers.", Some((crew::WAIT_FOR_AGENTS, serde_json::json!({"all": true, "timeout_seconds": 60})))),
                    _ => {
                        done();
                        complete("d6", "## Done\n\nThe flaky test is fixed and the docs are checked.\n\n- **r1** found a race in the parser's setup. The fixture now signals when it's ready, and `cargo test` passed **40 of 40**, ten times.\n- **r2** found one stale note, on `--remote`'s port.\n\n```rust\nfixture.ready().await?;\nparse(&input)?;\n```\n\n| Check | Result |\n|---|---|\n| Tests | 40 passed |\n| Docs | 1 note |\n\nSee https://example.com/notes for the details.")
                    }
                };
            }
            if let Some(step) = after("demo:stream") {
                if step == 0 && let Some(feed) = feed.get() {
                    let words = "Here is a long reply that streams in, word by word, so the page has something to follow while you scroll up and down. ".repeat(3);
                    'paragraphs: for paragraph in 0..12 {
                        for word in words.split_inclusive(' ') {
                            if !feed.activity().working {
                                break 'paragraphs;
                            }
                            let mut word = word.to_string();
                            if stamps {
                                word.push_str(&format!("⟦t={}⟧ ", chrono::Utc::now().timestamp_millis()));
                            }
                            feed.event(&WorkflowEvent {
                                timestamp: chrono::Utc::now(),
                                workflow_run_id: Uuid::nil(),
                                agent_name: MAIN.into(),
                                agent_run_id: Uuid::nil(),
                                event: AgentEvent::AssistantResponseChunk(word),
                            }, None);
                            tokio::time::sleep(Duration::from_millis(40)).await;
                        }
                        feed.event(&WorkflowEvent {
                            timestamp: chrono::Utc::now(),
                            workflow_run_id: Uuid::nil(),
                            agent_name: MAIN.into(),
                            agent_run_id: Uuid::nil(),
                            event: AgentEvent::AssistantResponseChunk(format!("(paragraph {})\n\n", paragraph + 1)),
                        }, None);
                    }
                }
                done();
                return complete("s1", "That was the stream.");
            }
            if let Some(step) = after("demo:slow") {
                return match step {
                    0 => turn("z0", "This takes a while.", Some(shell("sleep 30; echo done"))),
                    _ => {
                        done();
                        complete("z1", "Slept.")
                    }
                };
            }
            if let Some(step) = after("demo:ask") {
                return match step {
                    0 => tool_call("q0", ASK_USER_TOOL_NAME, serde_json::json!({"questions": "Tabs or spaces?"})),
                    _ => {
                        done();
                        complete("q1", "Noted.")
                    }
                };
            }
            if let Some(step) = after("demo:long") {
                if step == 0 && let Some(feed) = feed.get() {
                    // "demo:long 500": that many.
                    let count: usize = body
                        .rsplit("demo:long")
                        .next()
                        .and_then(|rest| rest.trim_start().split(|c: char| !c.is_ascii_digit()).next())
                        .and_then(|digits| digits.parse().ok())
                        .unwrap_or(3500);
                    for n in 0..count {
                        let call = Uuid::new_v4();
                        feed.event(&WorkflowEvent {
                            timestamp: chrono::Utc::now(),
                            workflow_run_id: Uuid::nil(),
                            agent_name: MAIN.into(),
                            agent_run_id: Uuid::nil(),
                            event: AgentEvent::ToolCallFormatted { tool_call_id: call, tool_name: "read_file".into(), parameters: [("path".to_string(), format!("src/file{n}.rs").into())].into() },
                        }, None);
                        feed.event(&WorkflowEvent {
                            timestamp: chrono::Utc::now(),
                            workflow_run_id: Uuid::nil(),
                            agent_name: MAIN.into(),
                            agent_run_id: Uuid::nil(),
                            event: AgentEvent::ToolResult { tool_call_id: call, tool_name: "read_file".into(), output: "1 | a\n2 | b".into() },
                        }, None);
                        if n % 50 == 0 {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                }
                done();
                return complete("l1", "Read the files.");
            }
            turn("m0", "On it.", None)
        })
    }))
    .await;
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, mut inputs, sender) =
        crate::tests::harness(temp.path(), server.client(), true);
    // Arko's shell, and the live switches the page's settings change.
    let shell = Shell::detect(WindowsShell::Powershell);
    let managers = Managers::open(&harness.session, &shell).unwrap();
    harness.tools = managers.tools(&shell, harness.handover.runs());
    harness.outside = Arc::new(crate::outside::Outside::new(
        false,
        managers.list(),
        harness.settings.workspace.clone(),
    ));
    harness.gauges = Arc::new(crate::Gauges {
        models: std::sync::Mutex::default(),
        conversations: std::sync::Mutex::default(),
        workflow: std::sync::Mutex::new(None),
        crew: std::sync::Mutex::new(None),
        stats: harness.stats.clone(),
        outside: harness.outside.clone(),
        handover: harness.handover.clone(),
        keys: harness.keys.clone(),
        text_replies: Default::default(),
        cc_compact: Default::default(),
        extra_rules: Default::default(),
        worker_models: Default::default(),
        monitor_wakes: Default::default(),
        worker_depth: Default::default(),
        worker_retry_cap: Default::default(),
        message_copies: Default::default(),
        builtin_skills: Default::default(),
        project: Default::default(),
        background: managers.background.clone(),
        feed: Some(harness.feed.clone()),
        page: std::sync::Mutex::default(),
        changed: Default::default(),
        title: Default::default(),
        published: Default::default(),
    });
    harness.open(false).unwrap();
    if let Some(count) = std::env::var("ARKO_DEMO_ITEMS")
        .ok()
        .and_then(|count| count.parse().ok())
    {
        harness.feed.restore(earlier_turns(count));
    }
    let _ = feed_cell.set(harness.feed.clone());
    let port = std::env::var("ARKO_DEMO_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(8799);
    harness
        .start_remote(remote::Bind {
            ip: std::env::var("ARKO_DEMO_HOST")
                .ok()
                .and_then(|host| host.parse().ok())
                .unwrap_or(std::net::Ipv4Addr::LOCALHOST.into()),
            port: Some(port),
        })
        .await;
    let key = std::fs::read_to_string(harness.session.dir.join(remote::KEY_FILE)).unwrap();
    println!("DEMO http://127.0.0.1:{port}/#key={}", key.trim());
    // Lines added to the file `ARKO_DEMO_INPUT` are typed at the prompt.
    if let Ok(path) = std::env::var("ARKO_DEMO_INPUT") {
        let sender = sender.clone();
        tokio::spawn(async move {
            let mut typed = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .lines()
                .count();
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                for line in text.lines().skip(typed) {
                    let _ = sender.send(Input::Line(line.to_string()));
                }
                typed = text.lines().count();
            }
        });
    }
    let seconds = std::env::var("ARKO_DEMO_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(900);
    let ending = async move {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        sender.send(Input::Eof).unwrap();
    };
    futures_util::future::join(harness.repl(&mut inputs), ending).await;
    harness
        .feed
        .notice(Level::Info, "Arko closed. The session is saved.");
    harness.stop_remote().await;
    managers.shutdown().await;
}

/// `count` items of earlier turns, each a message, some calls, a result and
/// a reply, for a long transcript.
fn earlier_turns(count: usize) -> Vec<remote::Item> {
    use remote::{CallStatus, Delivery, Item, Kind, Source};
    const REPLY: &str = "Here's what I found. The **parser** reads each line once, so \
        the cost is linear in the file's size, and `read_until` stops at the first blank \
        line.\n\n```rust\nfn parse(text: &str) -> Vec<Line> {\n    \
        text.lines().map(Line::from).collect()\n}\n```\n\n- The tests cover empty input \
        and a missing final newline.\n- A [link](https://example.com) and some \
        *emphasis*.\n\nThe change is small, and the benchmarks didn't move.";
    let mut items = Vec::with_capacity(count);
    let mut turn = 0;
    while items.len() < count {
        turn += 1;
        let mut push = |mut item: Item| {
            item.id = items.len() as u64 + 1;
            item.turn = Some(turn);
            items.push(item);
            items.len() as u64
        };
        let mut user = Item::new(
            Kind::User,
            format!("Turn {turn}. Look at the parser and tell me why the last test fails."),
        );
        user.source = Some(Source::Web);
        user.delivery = Some(Delivery::Turn);
        push(user);
        push(Item::new(
            Kind::Text,
            "Reading the parser and its tests first.",
        ));
        for (text, outcome) in [
            ("read_file src/parse.rs", "212 lines"),
            ("$ cargo test -p parse", "1 failed"),
            ("edit_file src/parse.rs", "+4 -1"),
        ] {
            let mut call = Item::new(Kind::Call, text);
            call.status = Some(CallStatus::Ok);
            call.outcome = Some(outcome.into());
            let id = push(call);
            if text.starts_with('$') {
                let mut result = Item::new(
                    Kind::Result,
                    "running 14 tests\ntest empty ... ok\ntest trailing_newline ... FAILED\n\n\
                     test result: FAILED. 13 passed; 1 failed",
                );
                result.call = Some(id);
                push(result);
            }
        }
        push(Item::new(Kind::Reply, REPLY));
        push(Item::new(Kind::Turn, "38s · $0.04 · 48k context"));
    }
    items.truncate(count);
    items
}

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
/// long reply that streams), "demo:worker" (a new worker whose long reply
/// streams while the main agent waits), "demo:think" (reasoning that
/// streams for 20 s),
/// "demo:slow" (a 30 s command), "demo:ask" (a
/// question), "demo:wait" (a question the agent waits for, which the next
/// message answers), "demo:questions" (three numbered questions with choices),
/// "demo:approval" (a worker profile to approve), "demo:image" (a file read
/// that shows a large image), "demo:table" (a reply with a narrow table, a
/// wide one and one with long cells), "demo:long" (3,500 calls, to reach the
/// history's limit; "demo:long 500", 500), anything else (a short turn).
/// With `ARKO_DEMO_ITEMS`, an earlier message sends an image too.
/// Lines added to the file `ARKO_DEMO_INPUT` are typed at the terminal.
/// `ARKO_DEMO_MODEL_MS` (900) is how long the model takes to answer, and a
/// long one keeps "Working…" on the page.
/// What starts a script: a message with one of these.
const MARKERS: [&str; 12] = [
    "Run the demo",
    "demo:stream",
    "demo:worker",
    "demo:think",
    "demo:slow",
    "demo:ask",
    "demo:wait",
    "demo:questions",
    "demo:approval",
    "demo:image",
    "demo:table",
    "demo:long",
];

/// The reply of "demo:table": a table that fits a phone, a wide one with
/// many columns, and one whose cells hold long text.
const TABLES: &str = "Three tables, from the narrowest.\n\n\
| Check | Result |\n|---|---|\n| Tests | 40 passed |\n| Docs | 1 note |\n\n\
A wide one, with a column for each of the page's views and workers.\n\n\
| Worker | Task | State | Model | Effort | Calls | Failed | Cost | Time | Started | Branch | Report |\n\
|---|---|---|---|---|---:|---:|---:|---:|---|---|---|\n\
| r1 | Fix the flaky test | finished | claude-sonnet-5-5 | high | 14 | 0 | $0.41 | 2 min 10 s | 14:02:11 | fix/flaky-parser-setup | Fixed |\n\
| r2 | Review the README | finished | claude-haiku-5-5 | low | 6 | 1 | $0.03 | 38 s | 14:02:15 | docs/readme-flags | One stale note |\n\
| r3 | Write the changelog | running | claude-sonnet-5-5 | medium | 3 | 0 | $0.09 | 1 min 4 s | 14:04:40 | docs/changelog | |\n\n\
And one with long cells, which wrap instead of making the table wider.\n\n\
| Where | What happens |\n|---|---|\n\
| The parser | The setup raced the clock, so the test failed one run in ten. Waiting for the fixture to signal that it is ready fixed it, and the test passed ten runs in a row. |\n\
| The docs | The note on --remote's port still said 8765, though https://example.com/notes/remote/ports/the-default-port-is-now-chosen-for-you says otherwise. |\n\
| The page | Short. |";

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
    let model_ms: u64 = std::env::var("ARKO_DEMO_MODEL_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(900);
    let feed_cell: Arc<std::sync::OnceLock<remote::Feed>> = Arc::new(std::sync::OnceLock::new());
    let feed_for_model = feed_cell.clone();
    // The script running: started by its marker in a new message, it goes
    // a step on with each request of the main agent, until it completes.
    let script: Arc<std::sync::Mutex<Script>> = Arc::default();
    let server = ModelServer::start(Arc::new(move |_, body: String| {
        let feed = feed_for_model.clone();
        let script = script.clone();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(model_ms)).await;
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
                    4 => turn("d4", "", Some((ASK_USER_TOOL_NAME, serde_json::json!({"questions": "Should I also update the **CHANGELOG**? It has no entry for 0.4 yet.", "wait": false})))),
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
            if let Some(step) = after("demo:worker") {
                if step == 0 && let Some(feed) = feed.get() {
                    // A worker of its own each time: r5, r6, and so on.
                    let id = format!("r{}", 4 + body.matches("demo:worker").count());
                    let run = Uuid::new_v4();
                    let words = "The worker reads the parser's tests one case after another, and writes down what it finds so the lead can check it later. ".repeat(2);
                    let send = |text: String| {
                        feed.event(&WorkflowEvent {
                            timestamp: chrono::Utc::now(),
                            workflow_run_id: Uuid::nil(),
                            agent_name: WORKER.into(),
                            agent_run_id: run,
                            event: AgentEvent::AssistantResponseChunk(text),
                        }, Some(&id));
                    };
                    'paragraphs: for paragraph in 0..6 {
                        for word in words.split_inclusive(' ') {
                            if !feed.activity().working {
                                break 'paragraphs;
                            }
                            send(word.to_string());
                            tokio::time::sleep(Duration::from_millis(40)).await;
                        }
                        send(format!("(paragraph {})\n\n", paragraph + 1));
                    }
                }
                done();
                return complete("x1", "The worker is done.");
            }
            if let Some(step) = after("demo:think") {
                if step == 0 && let Some(feed) = feed.get() {
                    let words = "The user wants the parser's last test to pass, so first I should read how it splits each line, then check what the empty input does, and only then change anything. ".repeat(3);
                    for word in words.split_inclusive(' ').cycle().take(130) {
                        if !feed.activity().working {
                            break;
                        }
                        feed.event(&WorkflowEvent {
                            timestamp: chrono::Utc::now(),
                            workflow_run_id: Uuid::nil(),
                            agent_name: MAIN.into(),
                            agent_run_id: Uuid::nil(),
                            event: AgentEvent::ReasoningChunk(word.to_string()),
                        }, None);
                        tokio::time::sleep(Duration::from_millis(150)).await;
                    }
                }
                done();
                return complete("k1", "That was the thinking.");
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
                    0 => tool_call("q0", ASK_USER_TOOL_NAME, serde_json::json!({"questions": "Tabs or spaces?", "wait": false})),
                    _ => {
                        done();
                        complete("q1", "Noted.")
                    }
                };
            }
            if let Some(step) = after("demo:wait") {
                return match step {
                    0 => tool_call(
                        "v0",
                        ASK_USER_TOOL_NAME,
                        serde_json::json!({"questions": "Which branch should the change go to?", "wait": true}),
                    ),
                    _ => {
                        done();
                        complete("v1", "Noted.")
                    }
                };
            }
            if let Some(step) = after("demo:questions") {
                return match step {
                    0 => tool_call(
                        "n0",
                        ASK_USER_TOOL_NAME,
                        serde_json::json!({"questions": [
                            {"question": "Tabs or spaces for the new files?", "choices": ["Tabs", "Spaces"], "default": "Spaces"},
                            {"question": "Which tests should I run before I push?", "choices": ["Unit", "Integration", "Screenshots"], "multiple": true},
                            {"question": "Which branch should the change go to?", "default": "main"},
                        ], "wait": false}),
                    ),
                    _ => {
                        done();
                        complete("n1", "I'll read the tests while you decide.")
                    }
                };
            }
            if let Some(step) = after("demo:approval") {
                return match step {
                    0 => tool_call(
                        "p0",
                        crew::SAVE_WORKER_PROFILE,
                        serde_json::json!({
                            "name": "3d-judge",
                            "description": "Judges 3D results against a reference image",
                            "instructions": "You judge 3D models against a reference image.\n\n1. Render the model from the reference's camera.\n2. Compare silhouettes, proportions and materials.\n3. Score each from 0 to 10, with one line on why.\n4. Name the three changes that would help most.",
                            "scope": "user",
                        }),
                    ),
                    _ => {
                        done();
                        complete("p1", "Noted.")
                    }
                };
            }
            if let Some(step) = after("demo:image") {
                return match step {
                    0 => turn("i0", "Here's the chart.", Some((READ_FILE_TOOL_NAME, serde_json::json!({"path": "chart.png"})))),
                    _ => {
                        done();
                        complete("i1", "The chart is 1600 by 1000 pixels.")
                    }
                };
            }
            if after("demo:table").is_some() {
                done();
                return complete("t0", TABLES);
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
    harness.background = managers.background.clone();
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
        billing: Default::default(),
        monitor_wakes: Default::default(),
        worker_depth: Default::default(),
        worker_retry_cap: Default::default(),
        message_copies: Default::default(),
        builtin_skills: Default::default(),
        extensions: Default::default(),
        project: Default::default(),
        background: managers.background.clone(),
        feed: Some(harness.feed.clone()),
        page: std::sync::Mutex::default(),
        changed: Default::default(),
        title: Default::default(),
        published: Default::default(),
    });
    harness.open(false).unwrap();
    // What new workers start on, as an app's session sets it, so profiles
    // can be saved (demo:approval).
    harness.start_new_workers();
    if let Some(count) = std::env::var("ARKO_DEMO_ITEMS")
        .ok()
        .and_then(|count| count.parse().ok())
    {
        let mut items = earlier_turns(count);
        // The message of the last turn that has its reply.
        let replied = items
            .iter()
            .rposition(|item| item.kind == remote::Kind::Reply)
            .unwrap_or(0);
        if let Some(message) = items[..replied]
            .iter_mut()
            .rev()
            .find(|item| item.kind == remote::Kind::User)
        {
            message.images.push(harness.images.add(
                demo_png(480, 300).into(),
                "image/png",
                "sketch.png",
            ));
        }
        harness.feed.restore(items);
    }
    std::fs::write(
        harness.settings.workspace.join("chart.png"),
        demo_png(1600, 1000),
    )
    .unwrap();
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

/// A PNG of `width` by `height` pixels, a gradient with rings.
fn demo_png(width: u32, height: u32) -> Vec<u8> {
    use std::io::Write;
    let mut pixels = Vec::with_capacity(((width * 3 + 1) * height) as usize);
    for y in 0..height {
        // Each row starts with its filter, none.
        pixels.push(0);
        for x in 0..width {
            let (dx, dy) = (x as f32 - width as f32 / 2., y as f32 - height as f32 / 2.);
            let ring = (((dx * dx + dy * dy).sqrt() / 40.) as u32).is_multiple_of(2);
            let shade = if ring { 40 } else { 0 };
            pixels.extend([
                (x * 200 / width) as u8 + shade,
                (y * 180 / height) as u8 + 30,
                200 - shade,
            ]);
        }
    }
    let mut packed = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    packed.write_all(&pixels).unwrap();
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], body: &[u8]| {
        png.extend((body.len() as u32).to_be_bytes());
        png.extend(kind);
        png.extend(body);
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(body);
        png.extend(crc.sum().to_be_bytes());
    };
    // 8-bit RGB.
    let header = [
        &width.to_be_bytes()[..],
        &height.to_be_bytes(),
        &[8, 2, 0, 0, 0],
    ]
    .concat();
    chunk(b"IHDR", &header);
    chunk(b"IDAT", &packed.finish().unwrap());
    chunk(b"IEND", &[]);
    png
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

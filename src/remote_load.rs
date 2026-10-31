//! A synthetic long, busy session served for the remote page, to measure
//! the page as a session grows, with Arko's feed and server and no model.
//! Not a test. Run it with `ARKO_LOAD_CONTROL=/tmp/load cargo test --lib
//! remote_page_load -- --ignored --nocapture`, open the link it prints, and
//! add commands to the control file, one per line:
//!
//! - `grow W` adds the history of W finished workers (calls with long
//!   details, command output, text and a report), with the main agent's
//!   calls between.
//! - `main N` adds N rounds of the main agent's own calls, output and text.
//! - `busy A` keeps A workers working (0 stops them).
//! - `status on|off` changes the status every second, as the costs of a
//!   session with running workers do.
//! - `stream on|off` streams the main agent's text.
//! - `turn` starts a turn of the main agent, and `end` ends it.
//! - `replay FILE` adds a session's recorded workflow events (its
//!   `run/events.jsonl`), with its workers' starts and ends.
//! - `quit`.
//!
//! `ARKO_LOAD_PORT` is the port (8811).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arko_agents::AgentEvent;
use arko_agents::builtins::READ_FILE_TOOL_NAME;
use arko_workflows::prelude::*;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::images::Images;
use crate::remote::{Delivery, Feed, Info, Meter, Reading, Server, Source, WorkerChange};
use crate::shell::SHELL_TOOL_NAME;

const KEY: &str = "d2VibGFnIG1lYXN1cmVtZW50cyAwMQ";

struct Moving(Mutex<Reading>);

impl Meter for Moving {
    fn read(&self) -> Reading {
        self.0.lock().unwrap().clone()
    }
}

fn event(agent: &str, run: Uuid, event: AgentEvent) -> WorkflowEvent {
    WorkflowEvent {
        timestamp: chrono::Utc::now(),
        workflow_run_id: Uuid::nil(),
        agent_name: agent.into(),
        agent_run_id: run,
        event,
    }
}

const WORDS: &[&str] = &[
    "the",
    "parser",
    "reads",
    "each",
    "line",
    "once",
    "and",
    "stops",
    "at",
    "first",
    "blank",
    "so",
    "cost",
    "is",
    "linear",
    "in",
    "file's",
    "size",
    "tests",
    "cover",
    "empty",
    "input",
    "missing",
    "final",
    "newline",
    "worker",
    "found",
    "race",
    "fixture",
    "signals",
    "ready",
    "change",
    "small",
    "benchmarks",
    "didn't",
    "move",
    "session",
    "remote",
    "page",
    "stream",
];

/// Words, about `bytes` long.
fn words(bytes: usize, seed: usize) -> String {
    let mut out = String::with_capacity(bytes + 16);
    let mut n = seed;
    while out.len() < bytes {
        n = n
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push_str(WORDS[(n >> 33) % WORDS.len()]);
        out.push(' ');
    }
    out
}

/// A reply in Markdown, about `bytes` long: paragraphs, a list, a code block.
fn reply(bytes: usize, seed: usize) -> String {
    let mut out = String::new();
    let mut part = 0;
    while out.len() < bytes {
        match part % 4 {
            0 | 2 => {
                out.push_str(&words(300, seed + part));
                out.push_str("\n\n");
            }
            1 => {
                for line in 0..4 {
                    out.push_str(&format!("- **item {line}** {}\n", words(60, seed + line)));
                }
                out.push('\n');
            }
            _ => {
                out.push_str("```rust\nfn parse(text: &str) -> Vec<Line> {\n    text.lines().map(Line::from).collect()\n}\n```\n\n");
            }
        }
        part += 1;
    }
    out
}

/// A command's output, `lines` lines.
fn output(lines: usize, seed: usize) -> String {
    (0..lines)
        .map(|line| {
            format!(
                "test parser::tests::case_{seed}_{line} ... ok {}",
                words(40, seed + line)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Sim {
    feed: Feed,
    workers: AtomicUsize,
    seed: AtomicUsize,
}

impl Sim {
    fn seed(&self) -> usize {
        self.seed.fetch_add(1, Ordering::Relaxed)
    }

    fn send(&self, agent: &str, run: Uuid, label: Option<&str>, change: AgentEvent) {
        self.feed.event(&event(agent, run, change), label);
    }

    fn call(
        &self,
        agent: &str,
        run: Uuid,
        label: Option<&str>,
        tool: &str,
        parameters: serde_json::Value,
    ) -> Uuid {
        let call = Uuid::new_v4();
        let parameters = parameters
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        self.send(
            agent,
            run,
            label,
            AgentEvent::ToolCallFormatted {
                tool_call_id: call,
                tool_name: tool.into(),
                parameters,
            },
        );
        call
    }

    fn result(
        &self,
        agent: &str,
        run: Uuid,
        label: Option<&str>,
        call: Uuid,
        tool: &str,
        out: String,
    ) {
        self.send(
            agent,
            run,
            label,
            AgentEvent::ToolResult {
                tool_call_id: call,
                tool_name: tool.into(),
                output: out,
            },
        );
    }

    /// A worker of the main agent starts, with a long task.
    fn start_worker(&self) -> (String, Uuid) {
        let n = self.workers.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("r{n}");
        let task = format!("Task {n}. {}", reply(1800, self.seed()));
        let call = self.call(
            "main",
            Uuid::nil(),
            None,
            crate::crew::START_AGENT,
            serde_json::json!({"agent": "worker", "task": task}),
        );
        self.result(
            "main",
            Uuid::nil(),
            None,
            call,
            crate::crew::START_AGENT,
            format!("Started {id} (agent `worker`)."),
        );
        self.feed.worker(&id, &task, WorkerChange::Started);
        (id, Uuid::new_v4())
    }

    /// One step of a worker's work: some text, then a command with a long
    /// script and long output, or a file read.
    fn worker_call(&self, id: &str, run: Uuid, step: usize) -> (Uuid, &'static str, String) {
        let seed = self.seed();
        if step % 3 == 2 {
            let call = self.call(
                "worker",
                run,
                Some(id),
                READ_FILE_TOOL_NAME,
                serde_json::json!({"path": format!("src/module{seed}.rs")}),
            );
            return (call, READ_FILE_TOOL_NAME, output(60, seed));
        }
        let script = format!(
            "cd crates/parser\ncargo test -p parser -- case_{seed} 2>&1 | tail -40\n{}",
            (0..12)
                .map(|line| format!("echo '{}'", words(50, seed + line)))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let call = self.call(
            "worker",
            run,
            Some(id),
            SHELL_TOOL_NAME,
            serde_json::json!({"command": script}),
        );
        (call, SHELL_TOOL_NAME, output(40, seed))
    }

    fn finish_worker(&self, id: &str) {
        let report = reply(2500, self.seed());
        self.feed
            .worker(id, "", WorkerChange::Finished { report: &report });
    }

    /// A worker's whole run, at once.
    fn grow_worker(&self, steps: usize) {
        let (id, run) = self.start_worker();
        for step in 0..steps {
            self.send(
                "worker",
                run,
                Some(&id),
                AgentEvent::AssistantResponseChunk(words(400, self.seed())),
            );
            let (call, tool, out) = self.worker_call(&id, run, step);
            self.result("worker", run, Some(&id), call, tool, out);
        }
        self.finish_worker(&id);
    }

    /// A round of the main agent's own work.
    fn main_round(&self) {
        let seed = self.seed();
        self.send(
            "main",
            Uuid::nil(),
            None,
            AgentEvent::AssistantResponseChunk(reply(900, seed)),
        );
        let call = self.call("main", Uuid::nil(), None, SHELL_TOOL_NAME, serde_json::json!({"command": format!("git log --oneline -20\ngit status --short\ncargo test -p parser case_{seed}")}));
        self.result(
            "main",
            Uuid::nil(),
            None,
            call,
            SHELL_TOOL_NAME,
            output(30, seed),
        );
        let to = format!(
            "r{}",
            1 + seed % self.workers.load(Ordering::Relaxed).max(1)
        );
        let call = self.call(
            "main",
            Uuid::nil(),
            None,
            crate::crew::MESSAGE_AGENT,
            serde_json::json!({"to": to, "message": words(700, seed)}),
        );
        self.result(
            "main",
            Uuid::nil(),
            None,
            call,
            crate::crew::MESSAGE_AGENT,
            format!("Sent to {to}."),
        );
        let call = self.call(
            "main",
            Uuid::nil(),
            None,
            READ_FILE_TOOL_NAME,
            serde_json::json!({"path": format!("src/main{seed}.rs")}),
        );
        self.result(
            "main",
            Uuid::nil(),
            None,
            call,
            READ_FILE_TOOL_NAME,
            output(20, seed),
        );
    }

    /// The meter's reading as a busy session's: a line for each worker, with
    /// how long it ran, which moves every second.
    fn reading(&self, tick: u64) -> Reading {
        let count = self.workers.load(Ordering::Relaxed);
        let lines: Vec<String> = (1..=count)
            .map(|n| {
                format!(
                    "r{n} · Task {n} with a title of some length · {} tool calls · {} s · running",
                    n * 3,
                    tick + n as u64
                )
            })
            .collect();
        Reading {
            cost: 12.0 + tick as f64 * 0.013,
            context_tokens: Some(120_000 + tick as usize * 37),
            workers: Some(lines.join("\n")),
            ..Reading::default()
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn remote_page_load() {
    let port: u16 = std::env::var("ARKO_LOAD_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(8811);
    let control =
        std::env::var("ARKO_LOAD_CONTROL").expect("ARKO_LOAD_CONTROL names the control file");
    let feed = Feed::new("main");
    let meter = Arc::new(Moving(Mutex::new(Reading::default())));
    let (actions, mut received) = mpsc::unbounded_channel();
    let server = Server::start(
        format!("127.0.0.1:{port}").parse().unwrap(),
        KEY.into(),
        Info {
            model: "claude-opus".into(),
            workspace: "/work".into(),
            session: "load".into(),
            app: String::new(),
        },
        feed.clone(),
        Images::default(),
        actions,
        meter.clone(),
        None,
    )
    .unwrap();
    // The page's messages become the user's items, as a session's would.
    {
        let feed = feed.clone();
        tokio::spawn(async move {
            while let Some(action) = received.recv().await {
                if let crate::remote::Action::Message(text) = action {
                    feed.user(&text, Source::Web, Delivery::Note);
                }
            }
        });
    }
    println!("LOAD http://127.0.0.1:{}/#key={KEY}", server.addr().port());
    let sim = Arc::new(Sim {
        feed: feed.clone(),
        workers: AtomicUsize::new(0),
        seed: AtomicUsize::new(1),
    });
    let status = Arc::new(AtomicBool::new(false));
    let stream = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicUsize::new(0));
    // The meter, every second.
    {
        let (sim, status, meter) = (sim.clone(), status.clone(), meter.clone());
        tokio::spawn(async move {
            let mut tick = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if status.load(Ordering::Relaxed) {
                    tick += 1;
                    *meter.0.lock().unwrap() = sim.reading(tick);
                }
            }
        });
    }
    // The main agent's text, streaming.
    {
        let (sim, stream) = (sim.clone(), stream.clone());
        tokio::spawn(async move {
            let mut n = 0;
            loop {
                tokio::time::sleep(Duration::from_millis(40)).await;
                if !stream.load(Ordering::Relaxed) {
                    continue;
                }
                n += 1;
                let mut word = WORDS[n % WORDS.len()].to_string();
                word.push(' ');
                if n % 40 == 0 {
                    word.push_str("\n\n");
                }
                sim.send(
                    "main",
                    Uuid::nil(),
                    None,
                    AgentEvent::AssistantResponseChunk(word),
                );
                // A call now and then ends the text item.
                if n % 600 == 0 {
                    let call = sim.call(
                        "main",
                        Uuid::nil(),
                        None,
                        READ_FILE_TOOL_NAME,
                        serde_json::json!({"path": "src/lib.rs"}),
                    );
                    sim.result(
                        "main",
                        Uuid::nil(),
                        None,
                        call,
                        READ_FILE_TOOL_NAME,
                        output(10, n),
                    );
                }
            }
        });
    }
    // The live workers: each streams text, runs a command for a few
    // seconds, and after some steps reports, and another takes its place.
    for slot in 0..64usize {
        let (sim, busy) = (sim.clone(), busy.clone());
        tokio::spawn(async move {
            loop {
                if slot >= busy.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue;
                }
                let (id, run) = sim.start_worker();
                for step in 0..12 {
                    for _ in 0..30 {
                        sim.send(
                            "worker",
                            run,
                            Some(&id),
                            AgentEvent::AssistantResponseChunk(words(12, sim.seed())),
                        );
                        tokio::time::sleep(Duration::from_millis(60)).await;
                    }
                    let (call, tool, out) = sim.worker_call(&id, run, step);
                    tokio::time::sleep(Duration::from_millis(1500 + (slot as u64 * 337) % 2000))
                        .await;
                    sim.result("worker", run, Some(&id), call, tool, out);
                    if slot >= busy.load(Ordering::Relaxed) {
                        break;
                    }
                }
                sim.finish_worker(&id);
            }
        });
    }
    // The control file's new lines.
    let mut done = std::fs::read_to_string(&control)
        .unwrap_or_default()
        .lines()
        .count();
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let text = std::fs::read_to_string(&control).unwrap_or_default();
        let lines: Vec<String> = text.lines().skip(done).map(str::to_string).collect();
        done += lines.len();
        for line in lines {
            let mut parts = line.split_whitespace();
            let command = parts.next().unwrap_or_default();
            let number: usize = parts
                .clone()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            let on = parts.next() == Some("on");
            match command {
                "grow" => {
                    for n in 0..number {
                        sim.grow_worker(25);
                        sim.main_round();
                        if n % 4 == 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                }
                "main" => {
                    for _ in 0..number {
                        sim.main_round();
                    }
                }
                // A session's recorded workflow events (run/events.jsonl),
                // with its workers' starts and ends, as they came.
                "replay" => {
                    let path = line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    let mut labels = std::collections::HashMap::new();
                    let (mut sent, mut bad) = (0, 0);
                    for (n, record) in text.lines().enumerate() {
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(record) else {
                            bad += 1;
                            continue;
                        };
                        if let (Some(handle), Some(run)) =
                            (value["handle"].as_str(), value["run_id"].as_str())
                        {
                            labels.insert(run.to_string(), handle.to_string());
                            let kind = &value["kind"];
                            let words =
                                |name: &str| kind[name].as_str().unwrap_or_default().to_string();
                            match kind["kind"].as_str() {
                                Some("started" | "resumed") => {
                                    feed.worker(handle, "", WorkerChange::Started)
                                }
                                Some("finished") => feed.worker(
                                    handle,
                                    "",
                                    WorkerChange::Finished {
                                        report: &words("summary"),
                                    },
                                ),
                                Some("failed") => feed.worker(
                                    handle,
                                    "",
                                    WorkerChange::Failed {
                                        error: &words("error"),
                                    },
                                ),
                                Some("stopped") => feed.worker(
                                    handle,
                                    "",
                                    WorkerChange::Stopped {
                                        reason: &words("reason"),
                                    },
                                ),
                                _ => {}
                            }
                            continue;
                        }
                        if value.get("event").is_none() {
                            continue;
                        }
                        let Ok(event) = serde_json::from_value::<WorkflowEvent>(value) else {
                            bad += 1;
                            continue;
                        };
                        let label = (event.agent_name != "main")
                            .then(|| labels.get(&event.agent_run_id.to_string()).cloned())
                            .flatten();
                        feed.event(&event, label.as_deref());
                        sent += 1;
                        if n % 500 == 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                    let count = labels.len();
                    sim.workers.store(count, Ordering::Relaxed);
                    println!("LOAD replayed {sent} events, {bad} unread, {count} workers");
                }
                "busy" => busy.store(number, Ordering::Relaxed),
                "status" => status.store(on, Ordering::Relaxed),
                "stream" => stream.store(on, Ordering::Relaxed),
                "turn" => feed.turn_started(),
                "end" => feed.turn_ended("38s · $0.04 · 48k context", None),
                "quit" => {
                    println!("LOAD quit");
                    return;
                }
                _ => {}
            }
            println!("LOAD did {line}");
        }
    }
}

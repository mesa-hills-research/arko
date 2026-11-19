//! The web page's shift meter (`web/shift/meter.mjs`), run against Arko's
//! own server and feed. The meter plays a scripted session in headless
//! Chromium and measures every line of text on every frame. This side is
//! the agent. It serves a new session for each run of the meter and makes
//! the feed's changes the meter asks for, one line of JSON at a time on a
//! local socket.
//!
//! Run it with `cargo test --lib remote_page_shift -- --ignored`. It finds
//! Chromium's headless shell in `ARKO_CHROMIUM` or Playwright's folder and
//! Node in `ARKO_NODE` or the path, and says it skipped without them.
//! `SHIFT_METER_OUT` names a file for the meter's tables, and
//! `SHIFT_METER_FULL=1` plays every size and view instead of the test's
//! set. The meter fails on any line that moves down where the rules don't
//! let it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arko_agents::AgentEvent;
use arko_workflows::prelude::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::approvals::{Answer, ApprovalAnswers, ApprovalChoice};
use crate::images::Images;
use crate::remote::{
    Action, Delivery, Feed, Info, Level, Meter, Reading, Server, Source, Verdict, WorkerChange,
};

const KEY: &str = "c2hpZnQgbWV0ZXIgc2Vzc2lvbnMgMDE";

/// The session's status, which says nothing changes but the calls a client
/// may move to the background.
#[derive(Default)]
struct Still {
    movable: std::sync::Mutex<Vec<u64>>,
}

impl Meter for Still {
    fn read(&self) -> Reading {
        Reading {
            movable: self.movable.lock().unwrap().clone(),
            ..Reading::default()
        }
    }
}

/// A session the meter plays, its feed and server, the status it gives,
/// and what waits on the user.
struct Session {
    feed: Feed,
    server: Server,
    still: Arc<Still>,
    approvals: HashMap<String, oneshot::Receiver<Answer>>,
}

fn event(agent: &str, label: Option<&str>, event: AgentEvent) -> (WorkflowEvent, Option<String>) {
    (
        WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: Uuid::nil(),
            agent_name: agent.into(),
            agent_run_id: Uuid::nil(),
            event,
        },
        label.map(str::to_string),
    )
}

/// The text of `name` in `request`.
fn text(request: &Value, name: &str) -> String {
    request[name].as_str().unwrap_or_default().to_string()
}

/// The page's messages and answers, as a session takes them.
fn answer_actions(feed: Feed, mut actions: mpsc::UnboundedReceiver<Action>) {
    tokio::spawn(async move {
        while let Some(action) = actions.recv().await {
            let Action::Send { action, reply } = action else {
                continue;
            };
            let verdict = match *action {
                Action::Message { text, .. } => {
                    let item = feed.user(&text, Source::Web, Delivery::Turn);
                    Verdict::accepted(Delivery::Turn, Some(item))
                }
                Action::AnswerApprovals(answers) => {
                    feed.answer_approvals(&answers);
                    Verdict::accepted(Delivery::Turn, None)
                }
                _ => Verdict::accepted(Delivery::Turn, None),
            };
            reply.answer(verdict);
        }
    });
}

/// Starts a session, served on a port of its own.
fn start(root: &Path) -> Session {
    let feed = Feed::new("main");
    let still = Arc::new(Still::default());
    let (actions, received) = mpsc::unbounded_channel();
    let server = Server::start(
        "127.0.0.1:0".parse().unwrap(),
        KEY.into(),
        Info {
            model: "claude-opus".into(),
            workspace: "/home/ada/proj".into(),
            session: "shift".into(),
            app: String::new(),
        },
        feed.clone(),
        Images::default(),
        actions,
        still.clone(),
        None,
        crate::remote::Project {
            workspace: root.join("work"),
            root: root.join("root"),
        },
    )
    .unwrap();
    answer_actions(feed.clone(), received);
    Session {
        feed,
        server,
        still,
        approvals: HashMap::new(),
    }
}

/// Does what `request` asks of the session, and says what came of it.
fn act(session: &mut Session, request: &Value) -> Value {
    let feed = &session.feed;
    let label = request["worker"].as_str();
    let agent = if label.is_some() { "worker" } else { "main" };
    let send = |change: AgentEvent| {
        let (event, label) = event(agent, label, change);
        feed.event(&event, label.as_deref());
    };
    match request["do"].as_str().unwrap_or_default() {
        "user" => {
            let id = feed.user(&text(request, "text"), Source::Terminal, Delivery::Turn);
            feed.turn_started();
            json!({ "item": id })
        }
        "started" => {
            feed.turn_started();
            json!({})
        }
        "end" => {
            feed.turn_ended("12 s · $0.02", None);
            json!({})
        }
        "think" => {
            send(AgentEvent::ReasoningChunk(text(request, "text")));
            json!({})
        }
        "text" => {
            send(AgentEvent::AssistantResponseChunk(text(request, "text")));
            json!({})
        }
        "call" => {
            let call = Uuid::new_v4();
            let parameters = request["parameters"]
                .as_object()
                .map(|parameters| {
                    parameters
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect()
                })
                .unwrap_or_default();
            send(AgentEvent::ToolCallFormatted {
                tool_call_id: call,
                tool_name: text(request, "tool"),
                parameters,
            });
            json!({ "call": call.to_string() })
        }
        // A call a code run's code makes, which starts and then ends.
        "nested" => {
            let call = Uuid::new_v4();
            let parameters: HashMap<String, Value> = request["parameters"]
                .as_object()
                .map(|parameters| {
                    parameters
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect()
                })
                .unwrap_or_default();
            send(AgentEvent::NestedToolCallStarted {
                outer_tool_call_id: request["run"]
                    .as_str()
                    .and_then(|run| run.parse().ok())
                    .unwrap_or_default(),
                tool_call_id: call,
                tool_name: text(request, "tool"),
                parameters_bytes: request["parameters"].to_string().len(),
                parameters,
            });
            json!({ "call": call.to_string() })
        }
        "nested_end" => {
            let output = text(request, "output");
            let outcome = match request["outcome"].as_str() {
                Some("refused") => arko_agents::NestedCallOutcome::Refused,
                _ => arko_agents::NestedCallOutcome::Finished,
            };
            let id = |name: &str| {
                request[name]
                    .as_str()
                    .and_then(|id| id.parse().ok())
                    .unwrap_or_default()
            };
            send(AgentEvent::NestedToolCallFinished {
                outer_tool_call_id: id("run"),
                tool_call_id: id("call"),
                tool_name: text(request, "tool"),
                outcome,
                output_bytes: output.len(),
                output,
            });
            json!({})
        }
        "result" => {
            send(AgentEvent::ToolResult {
                tool_call_id: request["call"]
                    .as_str()
                    .and_then(|call| call.parse().ok())
                    .unwrap_or_default(),
                tool_name: text(request, "tool"),
                output: text(request, "output"),
                receipt: None,
            });
            json!({})
        }
        "notice" => {
            let level = if request["warn"].as_bool() == Some(true) {
                Level::Warn
            } else {
                Level::Info
            };
            feed.notice(level, &text(request, "text"));
            json!({})
        }
        "note" => {
            let seq = request["seq"].as_u64().unwrap_or_default();
            let id = feed.note(&text(request, "text"), Source::Web, seq, Vec::new());
            json!({ "item": id })
        }
        "read" => {
            let seq = request["seq"].as_u64().unwrap_or_default();
            send(AgentEvent::ContextInjected {
                ids: vec![format!("note:{seq}")],
                bytes: 10,
            });
            json!({})
        }
        "approval" => {
            let approval = crate::approvals::Approval::Profile(crate::profiles::approval(
                &crate::profiles::Profile::new(text(request, "name"), "Reviews code"),
                crate::profiles::Place::User,
            ));
            let (id, answer) = feed.request_approval(Uuid::nil(), None, approval);
            session.approvals.insert(id.clone(), answer);
            json!({ "id": id })
        }
        "approved" => {
            let id = text(request, "id");
            session.approvals.remove(&id);
            feed.answer_approvals(&ApprovalAnswers {
                choices: vec![ApprovalChoice {
                    id,
                    approve: true,
                    option: false,
                }],
                words: None,
            });
            json!({})
        }
        // (With `also`, more questions of the same ask, each an `id` and a
        // `text`.)
        "ask" => {
            let asked = |request: &Value| crate::questions::AskedQuestion {
                id: text(request, "id"),
                question: arko_agents::questions::Question {
                    question: text(request, "text"),
                    ..Default::default()
                },
            };
            let mut all = vec![asked(request)];
            all.extend(request["also"].as_array().into_iter().flatten().map(asked));
            let item = feed.ask_questions(&all, request["waits"].as_bool() == Some(true));
            json!({ "item": item })
        }
        // What a worker runs with, and what it has cost (`model`, `effort`,
        // `profile`, `cost`).
        "worker_info" => {
            let words = |name: &str| request[name].as_str().map(str::to_string);
            let mut info = crate::remote::WorkerInfo::new();
            info.model = words("model");
            info.effort = words("effort");
            info.profile = words("profile");
            info.costs = request["cost"].as_f64().map(|cost| (cost, cost));
            feed.worker_info(&text(request, "id"), info);
            json!({})
        }
        "dismissed" => {
            feed.set_question_state(
                &text(request, "id"),
                crate::questions::QuestionState::Dismissed,
            );
            json!({})
        }
        "answered" => {
            feed.set_question_state(
                &text(request, "id"),
                crate::questions::QuestionState::Answered {
                    answer: text(request, "text"),
                },
            );
            json!({})
        }
        "worker" => {
            let id = text(request, "id");
            match request["change"].as_str() {
                Some("finished") => feed.worker(
                    &id,
                    "",
                    WorkerChange::Finished {
                        report: &text(request, "text"),
                    },
                ),
                _ => feed.worker(&id, &text(request, "text"), WorkerChange::Started),
            }
            json!({})
        }
        "job" => {
            let job = Uuid::new_v4();
            let mut start = crate::remote::JobStart::new(
                job,
                crate::remote::MAIN_AGENT.to_string(),
                crate::remote::JobKind::Command,
                text(request, "text"),
            );
            if let Some(call) = request["call"].as_str().and_then(|call| call.parse().ok()) {
                start = start.made_by(call);
            }
            feed.job_started(start);
            json!({ "job": job.to_string() })
        }
        "job_ended" => {
            let job = request["job"]
                .as_str()
                .and_then(|job| job.parse().ok())
                .unwrap_or_default();
            feed.job_ended(
                job,
                crate::remote::JobState::Finished,
                Some("exit 0".into()),
            );
            json!({})
        }
        // The latest call that runs may move to the background.
        "movable" => {
            let items = feed.subscribe().0.items;
            let running = items.iter().rev().find(|item| {
                item.kind == crate::remote::Kind::Call
                    && item.status == Some(crate::remote::CallStatus::Running)
            });
            *session.still.movable.lock().unwrap() =
                running.map(|item| item.id).into_iter().collect();
            json!({})
        }
        other => json!({ "error": format!("no such step: {other}") }),
    }
}

/// Serves the meter on `listener`, each connection with sessions of its own.
async fn serve(listener: TcpListener, root: PathBuf) {
    let mut each = 0;
    while let Ok((socket, _)) = listener.accept().await {
        each += 1;
        tokio::spawn(connection(socket, root.join(each.to_string())));
    }
}

/// Serves one of the meter's connections until it closes.
async fn connection(socket: tokio::net::TcpStream, root: PathBuf) {
    let (read, mut write) = socket.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut session: Option<Session> = None;
    while let Ok(Some(line)) = lines.next_line().await {
        let request: Value = serde_json::from_str(&line).unwrap_or_default();
        let mut answer = if request["do"] == "session" {
            if let Some(old) = session.take() {
                old.server.stop().await;
            }
            let new = start(&root);
            let url = format!("http://{}/#key={KEY}", new.server.addr());
            session = Some(new);
            json!({ "url": url })
        } else if let Some(session) = session.as_mut() {
            act(session, &request)
        } else {
            json!({ "error": "no session" })
        };
        answer["id"] = request["id"].clone();
        let mut out = answer.to_string();
        out.push('\n');
        if write.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }
    if let Some(old) = session.take() {
        old.server.stop().await;
    }
}

/// Chromium's headless shell, which can draw a frame when asked.
fn chromium() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ARKO_CHROMIUM") {
        return Some(path.into());
    }
    let cache = PathBuf::from(std::env::var_os("HOME")?).join(".cache/ms-playwright");
    let mut found: Vec<PathBuf> = std::fs::read_dir(cache)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("chromium_headless_shell-"))
        })
        .map(|path| path.join("chrome-headless-shell-linux64/chrome-headless-shell"))
        .filter(|path| path.exists())
        .collect();
    found.sort();
    found.pop()
}

fn node() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ARKO_NODE") {
        return Some(path.into());
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join("node"))
        .find(|path| path.exists())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn remote_page_shift() {
    let (Some(chromium), Some(node)) = (chromium(), node()) else {
        println!("skipped: the shift meter needs Node and Chromium's headless shell");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control = listener.local_addr().unwrap();
    let served = tokio::spawn(serve(listener, root.path().to_path_buf()));
    // (To run the meter by hand, as often as wanted.)
    if std::env::var_os("SHIFT_METER_SERVE").is_some() {
        println!("CONTROL {control}");
        let _ = served.await;
        return;
    }
    let tree = Path::new(env!("CARGO_MANIFEST_DIR"));
    let status = tokio::process::Command::new(node)
        .arg(tree.join("web/shift/meter.mjs"))
        .arg("--page")
        .arg(tree.join("src/remote/page"))
        .arg("--control")
        .arg(control.to_string())
        .arg("--chromium")
        .arg(chromium)
        .status()
        .await
        .unwrap();
    served.abort();
    assert!(status.success(), "the shift meter failed ({status})");
}

//! A session played into a feed from a script of the calls an agent makes
//! and what their tools return, for renderings of the transcript's views
//! from the code that draws them (the ignored `renderings` tests, with
//! `ARKO_SCENARIO` naming the script and `ARKO_RENDERINGS` the folder they
//! go in).

use std::collections::HashMap;

use arko_agents::AgentEvent;
use arko_workflows::WorkflowEvent;
use serde::Deserialize;
use uuid::Uuid;

use super::feed::{Delivery, Feed, Source, WorkerChange};

#[derive(Deserialize)]
struct Script {
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    kind: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    job: Option<Uuid>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    tool: String,
    #[serde(default)]
    parameters: HashMap<String, serde_json::Value>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    refused: Option<String>,
    #[serde(default)]
    running: bool,
    /// The worker whose step it is (`worker_start`, `worker_text`,
    /// `worker_thinking`, `worker_call`, `worker_finish`).
    #[serde(default)]
    worker: String,
    /// The files a `user` step's message carried from the web page.
    #[serde(default)]
    files: Vec<ScriptFile>,
}

/// A file of a `user` step, at `path`, of `bytes` and the type `mime`.
#[derive(Deserialize)]
struct ScriptFile {
    path: std::path::PathBuf,
    mime: String,
    bytes: u64,
}

fn event(event: AgentEvent) -> WorkflowEvent {
    agent_event("main", event)
}

fn agent_event(agent: &str, event: AgentEvent) -> WorkflowEvent {
    WorkflowEvent {
        timestamp: chrono::Utc::now(),
        workflow_run_id: Uuid::nil(),
        agent_name: agent.into(),
        agent_run_id: Uuid::nil(),
        event,
    }
}

/// A feed with the session of the script at `path` played into it.
pub(crate) fn play(path: &str) -> Feed {
    play_each(path, |_, _, _| {})
}

/// Plays each step before drawing it, so a rendering can show streams in progress.
pub(crate) fn play_each(path: &str, mut draw: impl FnMut(usize, &str, &Feed)) -> Feed {
    let script: Script = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let feed = Feed::new("main");
    for (index, step) in script.steps.into_iter().enumerate() {
        let label = step.label.clone();
        match step.kind.as_str() {
            "user" => {
                let files: Vec<crate::attachments::SentFile> = step
                    .files
                    .iter()
                    .map(|file| crate::attachments::SentFile {
                        file: crate::attachments::FileRef {
                            name: file
                                .path
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            mime: file.mime.clone(),
                            bytes: file.bytes,
                            path: file.path.clone(),
                            lines: None,
                        },
                        shown: false,
                    })
                    .collect();
                let source = match files.is_empty() {
                    true => Source::Terminal,
                    false => Source::Web,
                };
                feed.user_with_files(&step.text, source, Delivery::Turn, Vec::new(), files);
                feed.turn_started();
            }
            "text" => {
                let chunk = AgentEvent::AssistantResponseChunk(step.text.clone());
                feed.event(&event(chunk), None);
            }
            "thinking" => {
                let chunk = AgentEvent::ReasoningChunk(step.text.clone());
                feed.event(&event(chunk), None);
            }
            // A message the user sends while the agent works, which waits
            // for the agent to read it.
            "note" => {
                feed.note(&step.text, Source::Terminal, 1, Vec::new());
            }
            "reply" => {
                feed.reply(&step.text);
                feed.turn_ended("", None);
            }
            "worker_thinking" => {
                let chunk = AgentEvent::ReasoningChunk(step.text.clone());
                feed.event(&agent_event("worker", chunk), Some(&step.worker));
            }
            "job_end" => feed.job_ended(step.job.unwrap(), super::JobState::Finished, None),
            "worker_start" => feed.worker(&step.worker, &step.text, WorkerChange::Started),
            "worker_text" => {
                let chunk = AgentEvent::AssistantResponseChunk(step.text.clone());
                feed.event(&agent_event("worker", chunk), Some(&step.worker));
            }
            "worker_finish" => feed.worker(
                &step.worker,
                "",
                WorkerChange::Finished { report: &step.text },
            ),
            _ => {
                let command = step
                    .parameters
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let id = Uuid::new_v4();
                let call = AgentEvent::ToolCallFormatted {
                    tool_call_id: id,
                    tool_name: step.tool.clone(),
                    parameters: step.parameters,
                };
                let worker = (!step.worker.is_empty()).then_some(step.worker.as_str());
                let agent = if worker.is_some() { "worker" } else { "main" };
                feed.event(&agent_event(agent, call), worker);
                if let Some(job) = step.job {
                    feed.job_started(
                        super::JobStart::new(
                            job,
                            worker.unwrap_or("main"),
                            super::JobKind::Command,
                            command,
                        )
                        .made_by(id),
                    );
                }
                if !step.running {
                    let ended = match step.refused {
                        Some(reason) => AgentEvent::ToolRejected {
                            tool_call_id: id,
                            tool_name: step.tool,
                            reason: Some(reason),
                        },
                        None => AgentEvent::ToolResult {
                            tool_call_id: id,
                            tool_name: step.tool,
                            output: step.output.unwrap_or_default(),
                            receipt: None,
                        },
                    };
                    feed.event(&agent_event(agent, ended), worker);
                }
            }
        }
        draw(index + 1, &label, &feed);
    }
    feed
}

/// Renders either the final session or each step in a numbered folder.
pub(crate) fn render(path: &str, out: &str, mut draw: impl FnMut(&Feed, &str)) {
    std::fs::create_dir_all(out).unwrap();
    if std::env::var_os("ARKO_RENDER_STEPS").is_some() {
        let mut steps = Vec::new();
        play_each(path, |index, label, feed| {
            let name = format!("{index:02}");
            let folder = format!("{out}/{name}");
            std::fs::create_dir_all(&folder).unwrap();
            draw(feed, &folder);
            let workers: Vec<String> = feed.workers().into_iter().map(|worker| worker.id).collect();
            steps.push(serde_json::json!({"step": name, "label": label, "workers": workers}));
        });
        std::fs::write(
            format!("{out}/steps.json"),
            serde_json::to_vec_pretty(&steps).unwrap(),
        )
        .unwrap();
    } else {
        draw(&play(path), out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{CallStatus, Kind, WorkerState};

    #[test]
    fn thinking_steps_keep_streams_workers_failures_and_the_final_reply() {
        let path = format!(
            "{}/src/remote/scenarios/thinking.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut count = 0;
        let mut before_second_chunk = None;
        let feed = play_each(&path, |_, label, feed| {
            count += 1;
            assert!(!label.is_empty());
            let snapshot = feed.subscribe().0;
            if label == "Live thought after the failed call" {
                before_second_chunk = snapshot.items.last().map(|item| item.id);
            }
            if label == "A second consecutive thought chunk" {
                let thought = snapshot.items.last().unwrap();
                assert_eq!(Some(thought.id), before_second_chunk);
                assert!(thought.text.contains("full test path"));
            }
            if label == "Worker thinking inside its read row" {
                assert!(
                    snapshot
                        .items
                        .iter()
                        .any(|item| item.kind == Kind::Thinking && item.agent == "r3")
                );
                assert_eq!(snapshot.workers[0].state, WorkerState::Running);
            }
        });
        assert_eq!(count, 33);
        let snapshot = feed.subscribe().0;
        assert!(!snapshot.activity.working);
        assert_eq!(snapshot.workers[0].state, WorkerState::Finished);
        assert!(
            snapshot
                .items
                .iter()
                .any(|item| item.status == Some(CallStatus::Failed))
        );
        assert_eq!(
            snapshot
                .items
                .iter()
                .filter(|item| item.kind == Kind::Reply && item.agent == "main")
                .count(),
            1
        );
        assert_eq!(snapshot.jobs[0].state, super::super::JobState::Finished);
    }
}

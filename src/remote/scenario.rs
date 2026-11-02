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

use super::feed::{Delivery, Feed, Source};

#[derive(Deserialize)]
struct Script {
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    kind: String,
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
}

fn event(event: AgentEvent) -> WorkflowEvent {
    WorkflowEvent {
        timestamp: chrono::Utc::now(),
        workflow_run_id: Uuid::nil(),
        agent_name: "main".into(),
        agent_run_id: Uuid::nil(),
        event,
    }
}

/// A feed with the session of the script at `path` played into it, a turn
/// that is still under way.
pub(crate) fn play(path: &str) -> Feed {
    let script: Script = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let feed = Feed::new("main");
    for step in script.steps {
        match step.kind.as_str() {
            "user" => {
                feed.user(&step.text, Source::Terminal, Delivery::Turn);
                feed.turn_started();
            }
            "text" => {
                let chunk = AgentEvent::AssistantResponseChunk(step.text.clone());
                feed.event(&event(chunk), None);
            }
            _ => {
                let id = Uuid::new_v4();
                let call = AgentEvent::ToolCallFormatted {
                    tool_call_id: id,
                    tool_name: step.tool.clone(),
                    parameters: step.parameters,
                };
                feed.event(&event(call), None);
                if step.running {
                    continue;
                }
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
                    },
                };
                feed.event(&event(ended), None);
            }
        }
    }
    feed
}

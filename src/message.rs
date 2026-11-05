//! `message_user` lets an agent answer the user while it works, without ending
//! its turn or its task. The event printer shows the message as the call is
//! made (see `ui`), in order with the agent's other output, and names the
//! worker that sent it. The workers have it too, to reply to what the user
//! writes to them directly, and read a description of their own.

use std::collections::HashMap;

use arko_agents::{
    TaskContext, Tool, ToolDefinition, ToolError, ToolParameterDefinition, ToolParameterType,
    ToolTypeDetails,
};
use async_trait::async_trait;
use serde_json::Value;

pub const MESSAGE_USER: &str = "message_user";

/// The text parameter.
pub const MESSAGE_PARAM: &str = "message";

/// What the main agent's `message_user` says of itself.
const MAIN_DESCRIPTION: &str = "Shows the user a message now, without stopping your work.";

/// What a worker's says. A worker's instructions say when to use it and when
/// to write to its lead instead.
const WORKER_DESCRIPTION: &str = "Replies to the user who wrote to you directly. They read it in your own view, and your lead never sees it.";

#[derive(Debug)]
pub struct MessageUser {
    /// Whether a worker has it (and reads the worker's description).
    pub worker: bool,
}

#[async_trait]
impl Tool for MessageUser {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: MESSAGE_USER.to_string(),
            description: if self.worker {
                WORKER_DESCRIPTION
            } else {
                MAIN_DESCRIPTION
            }
            .to_string(),
            output_based_example: None,
            parameters: vec![ToolParameterDefinition {
                name: MESSAGE_PARAM.to_string(),
                description: "The message, in Markdown (shown as plain text).".to_string(),
                param_type: ToolParameterType::String,
                required: true,
                is_path: false,
                flag: None,
                default_value: None,
            }],
            parallel_safe: true,
            read_only: false,
            tool_type_details: ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: None,
        }
    }

    async fn execute(
        &self,
        _context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let text = params
            .get(MESSAGE_PARAM)
            .and_then(Value::as_str)
            .unwrap_or_default();
        if text.trim().is_empty() {
            return Err(ToolError::ExecutionFailed("the message is empty".into()));
        }
        Ok("The user has your message. Carry on with your work.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_takes_one_message() {
        for worker in [false, true] {
            let definition = MessageUser { worker }.definition();
            assert_eq!(definition.name, MESSAGE_USER);
            assert_eq!(definition.parameters.len(), 1);
            assert!(definition.parameters[0].required);
            assert!(definition.parallel_safe);
        }
    }

    /// A worker's description says it replies to a user who wrote to the
    /// worker, whose lead never sees the reply. A worker once sent the user a
    /// progress note every few minutes. The main agent's keeps saying that it
    /// shows a message.
    #[test]
    fn a_worker_reads_a_description_of_its_own() {
        let main = MessageUser { worker: false }.definition().description;
        let worker = MessageUser { worker: true }.definition().description;
        assert_eq!(
            main,
            "Shows the user a message now, without stopping your work."
        );
        assert_ne!(worker, main);
        assert!(worker.contains("who wrote to you directly"), "{worker}");
        assert!(worker.contains("your lead never sees it"), "{worker}");
        assert!(!main.contains("your lead"), "{main}");
    }
}

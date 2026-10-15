//! `message_user`: the main agent answers the user while it works, without
//! ending its turn. The event printer shows the message as the call is
//! made (see `ui`), in order with the agent's other output.

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

#[derive(Debug)]
pub struct MessageUser;

#[async_trait]
impl Tool for MessageUser {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: MESSAGE_USER.to_string(),
            description: "Shows the user a message now, without ending your turn.".to_string(),
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
        let definition = MessageUser.definition();
        assert_eq!(definition.name, MESSAGE_USER);
        assert_eq!(definition.parameters.len(), 1);
        assert!(definition.parameters[0].required);
        assert!(definition.parallel_safe);
    }
}

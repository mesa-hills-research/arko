//! A chat session's agent: Arko as a conversation partner, with no tools
//! (no shell, files, workers, MCP servers or skills). Each message is a
//! turn its reply answers. At the handover size it writes a summary of the
//! conversation in text and goes on from it, answering the latest message.

use arko_agents::{AgentConfig, AgentToolConfig};
use arko_agents::{BuiltInEnvironmentDetail, EnvironmentDetailPlacement, EnvironmentDetailsConfig};

use crate::agents::{self, AgentSettings, MAIN};

/// The chat's main agent, as the session's `settings` say: its model, its
/// handover size, and the user's instructions for it.
pub fn config(settings: &AgentSettings) -> Result<AgentConfig, String> {
    let mut builder = AgentConfig::builder(
        MAIN.into(),
        "Arko".into(),
        settings.main.llm_config(settings.thinking_text)?,
    )
    .role_description("You are Arko, a helpful assistant in a chat with the user.".into())
    .instructions(instructions(settings.chat_instructions))
    .tool_config(AgentToolConfig::chat())
    .environment_details_config(environment())
    // A summary in text, also in Claude Code (whose own compaction is for
    // agents that work).
    .handover(agents::handover(
        settings.main,
        settings.main.handover_at.unwrap_or(settings.handover_at),
        false,
    ));
    if settings.main.in_claude_code() {
        builder = builder.claude_code(settings.providers.claude_code.clone());
    }
    builder.build().map_err(|error| error.to_string())
}

/// What the chat's agent is told: how the chat works, then the user's
/// instructions, when there are any.
fn instructions(user: Option<&str>) -> String {
    let mut text = String::from(
        "# How this conversation works\n\
         The user chats with you in Arko. Each of their messages is a turn: answer it in your reply, written in Markdown. When what they ask isn't clear, ask them in your reply.\n\n\
         You have no tools here: you can't run commands, read or change files, or look things up, so don't say you will. When the answer needs something you don't have, say what you would need.\n",
    );
    if let Some(user) = user.map(str::trim).filter(|user| !user.is_empty()) {
        text.push_str("\n# The user's instructions\n");
        text.push_str(user);
        text.push('\n');
    }
    text
}

/// What a chat's agent knows of its surroundings: the date and the
/// operating system, no folder.
fn environment() -> EnvironmentDetailsConfig {
    let detail = BuiltInEnvironmentDetail {
        include: true,
        placement: EnvironmentDetailPlacement::SystemPrompt,
        depth: None,
    };
    EnvironmentDetailsConfig {
        current_datetime: detail.clone(),
        operating_system: detail,
        ..EnvironmentDetailsConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{Effort, ModelSettings};

    /// A chat in Claude Code writes its summary in text at the handover
    /// size, as every chat does: it isn't compacted.
    #[test]
    fn a_chat_in_claude_code_hands_over_with_a_summary() {
        let opus = ModelSettings {
            model: crate::providers::model_name(&agents::resolve_model("cc:opus").unwrap()),
            effort: Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let settings = AgentSettings {
            cc_compact: true,
            mode: crate::session::Mode::Chat,
            ..AgentSettings::for_test(&opus, &opus)
        };
        let config = config(&settings).unwrap();
        assert!(config.runs_in_claude_code());
        assert!(!agents::compacts(&config));
    }
}

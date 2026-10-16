//! A resumed session's conversation as the feed's items, so that its
//! clients start with what was said before (the full-screen view starts on
//! an empty screen): the user's messages, the agent's text, calls (with
//! their outcomes), questions and replies.

use std::collections::HashMap;

use arko_agents::builtins::ASK_USER_TOOL_NAME;
use arko_ai::{Content, Message, Role};

use super::feed::{Delivery, Item, Kind, Source};
use super::render;

/// The messages read back at most (the feed keeps fewer items anyway).
const MESSAGES: usize = 600;

/// The items of `messages` (a conversation of the main agent), numbered
/// from 1 (a result's `call` names its call by that number). With
/// `text_replies` (the agent ends its turns with a reply in plain text), a
/// response of text alone is a reply, unless the user's answer to a
/// question follows it (Claude Code ends its turn after `ask_user`).
pub fn items(messages: &[Message], text_replies: bool) -> Vec<Item> {
    let messages = &messages[messages.len().saturating_sub(MESSAGES)..];
    let mut items: Vec<Item> = Vec::new();
    // The calls waiting for their results: their items, by call ID.
    let mut calls: HashMap<String, (usize, String)> = HashMap::new();
    let push = |items: &mut Vec<Item>, mut item: Item| -> usize {
        item.id = items.len() as u64 + 1;
        items.push(item);
        items.len() - 1
    };
    for (index, message) in messages.iter().enumerate() {
        match message.role {
            Role::System => {}
            Role::User => {
                for content in &message.content {
                    match content {
                        Content::ToolResult(result) => {
                            let Some((index, tool)) =
                                result.call_id.as_ref().and_then(|id| calls.remove(id))
                            else {
                                continue;
                            };
                            // (The answer comes as a message of the user's.)
                            if tool == ASK_USER_TOOL_NAME {
                                continue;
                            }
                            let outcome = if result.is_error {
                                render::Outcome::failed(&result.output)
                            } else {
                                render::outcome(&tool, &result.output)
                            };
                            let call = &mut items[index];
                            call.status = Some(outcome.status);
                            call.outcome = outcome.text;
                            call.output = outcome.output;
                            let call_id = call.id;
                            if let Some(mut block) = outcome.block {
                                block.call = Some(call_id);
                                push(&mut items, block);
                            }
                        }
                        Content::Text(text) => {
                            if let Some(item) = user_item(text) {
                                push(&mut items, item);
                            }
                        }
                        Content::Image(_) => {}
                    }
                }
            }
            Role::Assistant => {
                let reply = text_replies
                    && message.tool_calls.is_empty()
                    && !messages.get(index + 1).is_some_and(is_answer);
                if reply {
                    let text: String = message
                        .content
                        .iter()
                        .filter_map(Content::as_text)
                        .collect();
                    if !text.trim().is_empty() {
                        push(&mut items, Item::new(Kind::Reply, text.trim()));
                    }
                }
                for content in message.content.iter().filter(|_| !reply) {
                    if let Content::Text(text) = content
                        && !text.trim().is_empty()
                    {
                        push(&mut items, Item::new(Kind::Text, text.trim()));
                    }
                }
                for call in &message.tool_calls {
                    let parameters: HashMap<String, serde_json::Value> = call
                        .arguments
                        .as_object()
                        .map(|object| object.clone().into_iter().collect())
                        .unwrap_or_default();
                    let text = |name: &str| {
                        parameters
                            .get(name)
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .trim()
                            .to_string()
                    };
                    let item = match call.name.as_str() {
                        "complete_task" => Some(Item::new(Kind::Reply, text("summary"))),
                        ASK_USER_TOOL_NAME => Some(Item::new(Kind::Question, text("questions"))),
                        name => render::call(name, &parameters),
                    };
                    let Some(mut item) = item else {
                        continue;
                    };
                    let call_item = item.kind == Kind::Call;
                    if call_item {
                        // Without a result (the session stopped first).
                        item.status = Some(super::CallStatus::Failed);
                        item.outcome = Some("interrupted".into());
                    }
                    let index = push(&mut items, item);
                    if (call_item || call.name == ASK_USER_TOOL_NAME)
                        && let Some(id) = &call.id
                    {
                        calls.insert(id.clone(), (index, call.name.clone()));
                    }
                }
            }
        }
    }
    items
}

/// Whether `message` is the user's answer to the agent's questions.
fn is_answer(message: &Message) -> bool {
    message.role == Role::User
        && message.content.iter().any(|content| {
            content.as_text().is_some_and(|text| {
                let text = text.trim_start();
                text.starts_with("You asked the following questions:")
                    || text.starts_with("Response to the agent's questions:")
            })
        })
}

/// What the user wrote, from a message of theirs: Arko's composed message
/// keeps it under "# My message"; context updates and the workers' news
/// aren't the user's words.
fn user_item(text: &str) -> Option<Item> {
    let text = text.trim();
    if text.is_empty() || text.starts_with("# Workflow context update") {
        return None;
    }
    // The answer to the agent's question.
    if let Some(asked) = text.strip_prefix("You asked the following questions:") {
        let answer = asked
            .split_once("The user responded:")
            .map_or(asked, |(_, answer)| answer);
        let mut item = Item::new(Kind::User, answer.trim());
        item.source = Some(Source::Terminal);
        item.delivery = Some(Delivery::Answer);
        return Some(item);
    }
    if text.starts_with("Your workers sent you news") {
        return Some(
            Item::new(Kind::Notice, "the workers' news went to the agent")
                .level(super::Level::Info),
        );
    }
    let mine = match text.split_once("# My message\n\n") {
        Some((_, mine)) => mine,
        // Only Arko's own sections: no words of the user's.
        None if text.starts_with("# ") => return None,
        None => text,
    };
    let mut item = Item::new(Kind::User, mine.trim());
    item.source = Some(Source::Terminal);
    item.delivery = Some(Delivery::Turn);
    Some(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arko_ai::{ToolCall, ToolResult};

    fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
        serde_json::from_value(serde_json::json!({"id": id, "name": name, "arguments": arguments}))
            .unwrap()
    }

    /// The user's words (not Arko's sections or context updates), the
    /// agent's text, a call with its outcome and output, and the reply.
    #[test]
    fn a_conversation_reads_as_it_did() {
        let mut ask = Message::new_assistant("Let me check.");
        ask.tool_calls = vec![call(
            "call_1",
            crate::shell::SHELL_TOOL_NAME,
            serde_json::json!({"command": "cargo test"}),
        )];
        let mut done = Message::new_assistant("");
        done.content.clear();
        done.tool_calls = vec![call(
            "call_2",
            "complete_task",
            serde_json::json!({"summary": "All 12 pass."}),
        )];
        let result = Message::new_user_with_contents(vec![Content::ToolResult(ToolResult::new(
            Some("call_1".into()),
            crate::shell::SHELL_TOOL_NAME,
            "exit code 0, 1.0 s\ntest result: ok. 12 passed; 0 failed",
        ))]);
        let messages = vec![
            Message::new_user(
                "# Project instructions (AGENTS.md in the working directory)\n\nBe brief.\n\n# My message\n\nRun the tests",
            ),
            ask,
            result,
            Message::new_user("# Workflow context update\n\nfiles changed"),
            Message::new_user(
                "You asked the following questions:\nKeep the old API?\n\nThe user responded:\nyes",
            ),
            done,
        ];
        let items = items(&messages, false);
        let shown: Vec<(Kind, &str)> = items.iter().map(|i| (i.kind, i.text.as_str())).collect();
        assert_eq!(
            shown,
            [
                (Kind::User, "Run the tests"),
                (Kind::Text, "Let me check."),
                (Kind::Call, "$ cargo test"),
                (Kind::Result, "exit code 0, 1.0 s"),
                (Kind::User, "yes"),
                (Kind::Reply, "All 12 pass."),
            ]
        );
        assert_eq!(items[2].outcome.as_deref(), Some("12 passed"));
        assert_eq!(items[3].call, Some(items[2].id));
    }

    /// With text replies, a response of text alone is the agent's reply,
    /// unless the answer to its question follows it. Without, it is text.
    #[test]
    fn text_replies_read_as_replies() {
        let mut look = Message::new_assistant("Looking.");
        look.tool_calls = vec![call(
            "call_1",
            crate::shell::SHELL_TOOL_NAME,
            serde_json::json!({"command": "ls"}),
        )];
        let result = Message::new_user_with_contents(vec![Content::ToolResult(ToolResult::new(
            Some("call_1".into()),
            crate::shell::SHELL_TOOL_NAME,
            "exit code 0, 0.1 s",
        ))]);
        let messages = vec![
            Message::new_user("How many files?"),
            look,
            result,
            Message::new_assistant("Asked which folder."),
            Message::new_user(
                "You asked the following questions:\nWhich folder?\n\nThe user responded:\nsrc",
            ),
            Message::new_assistant("There are 3 files."),
            Message::new_user("Thanks."),
            Message::new_assistant("You're welcome."),
        ];
        let shown = |text_replies| {
            items(&messages, text_replies)
                .into_iter()
                .filter(|item| matches!(item.kind, Kind::Text | Kind::Reply))
                .map(|item| (item.kind, item.text))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shown(true),
            [
                (Kind::Text, "Looking.".to_string()),
                (Kind::Text, "Asked which folder.".to_string()),
                (Kind::Reply, "There are 3 files.".to_string()),
                (Kind::Reply, "You're welcome.".to_string()),
            ]
        );
        assert!(shown(false).iter().all(|(kind, _)| *kind == Kind::Text));
    }
}

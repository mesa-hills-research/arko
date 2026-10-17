//! A resumed session's conversation as the feed's items, so that its
//! clients start with what was said before (the full-screen view starts on
//! an empty screen): the user's messages, the agent's text, calls (with
//! their outcomes), questions and replies.

use std::collections::HashMap;

use arko_agents::builtins::ASK_USER_TOOL_NAME;
use arko_ai::{Content, Message, Role};

use super::feed::{Delivery, Item, Kind, Source};
use super::render;
use crate::images::Images;

/// The messages read back at most (the feed keeps fewer items anyway).
const MESSAGES: usize = 600;

/// The message that kept the prompt cache warm, as Arko worded it before.
const EARLIER_REFRESH_MESSAGE: &str = "(Arko: this message only keeps the prompt cache warm. Reply with \"Acknowledged\" and nothing else.)";

/// What the restored transcript says where the workers' news started a
/// turn.
const NEWS_NOTICE: &str = "the workers' news went to the agent";

/// The items of `messages` (a conversation of the main agent), numbered
/// from 1 (a result's `call` names its call by that number), each with
/// the turn it came in, from 1. A turn begins at each new prompt, including
/// the context update that reopens a stopped run and messages that show no
/// item (what the user typed during a turn, sent in a turn of its own).
/// In-turn context updates and answers to questions keep the same turn.
/// Recovery after a crash adds no saved message, so it keeps the interrupted
/// turn's number here. With
/// `text_replies` (the agent ends its turns with a reply in plain text), a
/// response of text alone is a reply, unless the user's answer to a
/// question follows it (Claude Code ends its turn after `ask_user`).
///
/// The images of the user's messages and of tool results go to `images`,
/// and their items show them.
pub fn items(messages: &[Message], text_replies: bool, images: &Images) -> Vec<Item> {
    items_from(
        messages,
        messages.len().saturating_sub(MESSAGES),
        text_replies,
        images,
    )
}

/// The [`items`] of every message of `messages`, however many.
pub(crate) fn all_items(messages: &[Message], text_replies: bool, images: &Images) -> Vec<Item> {
    items_from(messages, 0, text_replies, images)
}

/// The [`items`] of `messages` from the `first`th on.
fn items_from(
    messages: &[Message],
    first: usize,
    text_replies: bool,
    images: &Images,
) -> Vec<Item> {
    let messages = &messages[first..];
    let mut items: Vec<Item> = Vec::new();
    // The calls waiting for their results: their items, by call ID.
    let mut calls: HashMap<String, (usize, String)> = HashMap::new();
    let push = |items: &mut Vec<Item>, mut item: Item| -> usize {
        item.id = items.len() as u64 + 1;
        items.push(item);
        items.len() - 1
    };
    let mut turn: Option<u64> = None;
    let mut ended = true;
    for (index, message) in messages.iter().enumerate() {
        let start = items.len();
        if starts_turn(message)
            && (ended
                || message
                    .content
                    .iter()
                    .filter_map(Content::as_text)
                    .any(|text| {
                        !text.trim_start().starts_with("# Workflow context update")
                            || reopened(text)
                    }))
        {
            turn = Some(turn.map_or(1, |turn| turn + 1));
            ended = false;
        }
        match message.role {
            Role::System => {}
            Role::User => {
                // The user's item of this message, which its images join.
                let mut mine: Option<usize> = None;
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
                            call.images.extend(
                                result.images.iter().filter_map(|image| {
                                    images.add_image(&image.image, &image.label)
                                }),
                            );
                            let call_id = call.id;
                            if let Some(mut block) = outcome.block {
                                block.call = Some(call_id);
                                push(&mut items, block);
                            }
                        }
                        Content::Text(text) => {
                            if let Some(mut item) = user_item(text) {
                                // A message that started a turn can be
                                // edited at its index.
                                if item.delivery == Some(Delivery::Turn) {
                                    item.message_index = Some(first + index);
                                }
                                mine = Some(push(&mut items, item));
                            }
                        }
                        Content::Image(image) => {
                            if let Some(at) = mine
                                && let Some(shown) = images.add_image(image, "image")
                            {
                                items[at].images.push(shown);
                            }
                        }
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
        if message.role == Role::Assistant {
            ended = message
                .tool_calls
                .iter()
                .any(|call| call.name == "complete_task")
                || (text_replies
                    && message.tool_calls.is_empty()
                    && !messages.get(index + 1).is_some_and(is_answer));
        }
        for item in &mut items[start..] {
            item.turn = turn;
        }
    }
    items
}

/// A stopped run gets this notice inside a context update when it restarts.
/// Other context updates arrive during a turn and keep that turn's number.
fn reopened(text: &str) -> bool {
    text.trim_start().starts_with("# Workflow context update")
        && text.lines().any(|line| {
            line.starts_with("Your run was stopped before it finished and has now been reopened:")
        })
}

/// Whether `message` can begin a turn: a message from the user's side with
/// text, not tool results alone, nor the user's answer to a question.
fn starts_turn(message: &Message) -> bool {
    message.role == Role::User
        && message
            .content
            .iter()
            .any(|content| content.as_text().is_some())
        && !is_answer(message)
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

/// The user's own words in a message Arko sent: before a handover's report
/// (the first message of a conversation that was handed over holds the
/// message it answers, then the report), and under "# My message" when Arko
/// composed sections around them.
pub(crate) fn own_words(text: &str) -> &str {
    let heading = arko_agents::handover::REPORT_HEADING;
    let text = text
        .find(&format!("\n\n{heading}"))
        .map_or(text, |at| &text[..at]);
    text.split_once("# My message\n\n")
        .map_or(text, |(_, mine)| mine)
        .trim()
}

/// What the user wrote, from a message of theirs: Arko's composed message
/// keeps it under "# My message"; context updates and the workers' news
/// aren't the user's words, nor is a handover's report.
fn user_item(text: &str) -> Option<Item> {
    let heading = arko_agents::handover::REPORT_HEADING;
    let text = text
        .find(&format!("\n\n{heading}"))
        .map_or(text, |at| &text[..at])
        .trim();
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
        return Some(Item::new(Kind::Notice, NEWS_NOTICE).level(super::Level::Info));
    }
    let mine = match text.split_once("# My message\n\n") {
        Some((_, mine)) => mine,
        // Only Arko's own sections: no words of the user's.
        None if text.starts_with("# ") => return None,
        None => text,
    };
    let mut item = Item::new(Kind::User, mine.trim());
    item.source = Some(Source::Terminal);
    // (Sessions saved before it was reworded have the earlier text.)
    let refresh = [super::feed::REFRESH_MESSAGE, EARLIER_REFRESH_MESSAGE];
    item.delivery = Some(if refresh.contains(&item.text.as_str()) {
        Delivery::Refresh
    } else {
        Delivery::Turn
    });
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

    #[test]
    fn prompts_after_an_interrupted_call_start_their_own_turns() {
        let mut shell = Message::new_assistant("");
        shell.tool_calls = vec![call(
            "shell",
            "shell",
            serde_json::json!({"command": "sleep 20"}),
        )];
        for prompt in [
            "Go on.",
            "# My message\n\nGo on.",
            "# What I typed during your last turn, which you didn't get to read\n\n- Go on.",
            "Your workers sent you news as your last turn ended.",
            "A background job of yours finished after your last turn ended.",
            super::super::feed::REFRESH_MESSAGE,
            "# Workflow context update\n\nYour run was stopped before it finished and has now been reopened: continue your task where you were.\n\nGo on.",
        ] {
            for text_replies in [false, true] {
                let messages = vec![
                    Message::new_user("Start."),
                    shell.clone(),
                    Message::new_user("# Workflow context update\n\nA worker is running."),
                    Message::new_user(prompt),
                    Message::new_assistant("Done."),
                ];
                let items = all_items(&messages, text_replies, &Images::default());
                assert_eq!(items.last().unwrap().turn, Some(2), "{prompt}");
                assert_eq!(items[1].turn, Some(1));
            }
        }
    }

    #[test]
    fn crash_recovery_without_a_prompt_keeps_the_saved_turn_number() {
        let mut shell = Message::new_assistant("");
        shell.tool_calls = vec![call(
            "shell",
            "shell",
            serde_json::json!({"command": "sleep 20"}),
        )];
        let messages = vec![
            Message::new_user("Start."),
            shell,
            Message::new_user("# Workflow context update\n\nA worker is running."),
            Message::new_assistant("Recovered."),
        ];
        let items = all_items(&messages, true, &Images::default());
        assert!(items.iter().all(|item| item.turn == Some(1)));
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
        let items = items(&messages, false, &Images::default());
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

    /// Every message reads as items when asked, however many.
    #[test]
    fn a_long_conversation_reads_whole_when_asked() {
        let messages: Vec<Message> = (0..400)
            .flat_map(|n| {
                [
                    Message::new_user(&format!("Question {n}.")),
                    Message::new_assistant(&format!("Answer {n}.")),
                ]
            })
            .collect();
        let images = Images::default();
        assert_eq!(items(&messages, true, &images).len(), MESSAGES);
        let all = all_items(&messages, true, &images);
        assert_eq!(all.len(), 800);
        assert_eq!(all[0].text, "Question 0.");
        assert_eq!(all[799].turn, Some(400));
    }

    /// What the user typed during a turn, which Arko sends in a turn of its
    /// own after the reply, begins a turn, though it shows no item.
    #[test]
    fn a_turn_for_what_was_typed_meanwhile_is_a_turn_of_its_own() {
        let messages = vec![
            Message::new_user("Slow."),
            Message::new_assistant("Slow 4."),
            Message::new_user("# What I typed during your last turn\n\n- Also the docs."),
            Message::new_assistant("Main 5."),
        ];
        let turns: Vec<(Kind, Option<u64>)> = items(&messages, true, &Images::default())
            .iter()
            .map(|item| (item.kind, item.turn))
            .collect();
        assert_eq!(
            turns,
            [
                (Kind::User, Some(1)),
                (Kind::Reply, Some(1)),
                (Kind::Reply, Some(2))
            ]
        );
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
            items(&messages, text_replies, &Images::default())
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
        // Each with its turn: the answer to the question is in the first.
        let turns: Vec<(Kind, Option<u64>)> = items(&messages, true, &Images::default())
            .into_iter()
            .map(|item| (item.kind, item.turn))
            .collect();
        assert_eq!(
            turns,
            [
                (Kind::User, Some(1)),
                (Kind::Text, Some(1)),
                (Kind::Call, Some(1)),
                (Kind::Text, Some(1)),
                (Kind::User, Some(1)),
                (Kind::Reply, Some(1)),
                (Kind::User, Some(2)),
                (Kind::Reply, Some(2)),
            ]
        );
    }
}

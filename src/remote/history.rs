//! A resumed session's conversation as the feed's items, so that its
//! clients start with what was said before (the full-screen view starts on
//! an empty screen). They are the user's messages and the notes they typed
//! during turns, the agent's text, calls (with their outcomes), questions
//! and replies.

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
/// the context update that reopens a stopped run and what the user typed
/// during a turn, sent in a turn of its own.
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
                            for (mut item, editable) in user_items(text) {
                                // A message that started a turn can be
                                // edited at its index.
                                if editable && item.delivery == Some(Delivery::Turn) {
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
                let reply = text_replies && is_reply(messages, index);
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
                || (text_replies && is_reply(messages, index));
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

/// Whether the agent's message at `index` ends its turn when it replies in
/// text. That's text alone, with neither the answer to its question after
/// it nor more of its response. Claude Code's agents save the text of a
/// response apart from its calls, as a message of its own before them.
fn is_reply(messages: &[Message], index: usize) -> bool {
    messages[index].tool_calls.is_empty()
        && messages
            .get(index + 1)
            .is_none_or(|next| next.role != Role::Assistant && !is_answer(next))
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

/// The heading of the section of a message that holds what the user typed
/// during the turn before, which the agent didn't get to read. (Sessions
/// saved before it was reworded have the heading without its end.)
const UNREAD_HEADING: &str = "# What I typed during your last turn";

/// How a context update names a note of the user's, between its number and
/// when it was written.
const OPERATOR_NOTE: &str = " from the operator (the person running this workflow), ";

/// The items of a text from the user's side, each with whether it can be
/// edited at its message's index. They are the user's words (see
/// `user_item`), and the notes they typed during a turn, as the live
/// transcript shows them. A note the agent read as it worked came in a
/// context update. One it didn't read went with the next message, in a
/// section of its own. A stopped run reopened with the user's message has
/// that message after the reopening notice, in a context update, where the
/// live transcript doesn't offer to edit it either.
fn user_items(text: &str) -> Vec<(Item, bool)> {
    let Some(update) = context_update(text) else {
        return composed(text)
            .into_iter()
            .map(|item| (item, true))
            .collect();
    };
    let items = match reopened_with(update) {
        Some(prompt) => composed(prompt),
        None => operator_notes(update).into_iter().map(note).collect(),
    };
    items.into_iter().map(|item| (item, false)).collect()
}

/// The content of a context update, without what the runtime adds after it
/// (the environment and reminders). `None` for any other text.
fn context_update(text: &str) -> Option<&str> {
    let update = text
        .trim_start()
        .strip_prefix("# Workflow context update")?
        .trim_start();
    let update = update
        .split("\n\n# Current Environment Information (updated this turn)\n")
        .next()?;
    update.split("\n\n# Reminder\n").next()
}

/// What follows the notice in the context update that reopens a stopped
/// run: the message Arko composed for it (empty without one).
fn reopened_with(update: &str) -> Option<&str> {
    if !update.starts_with("Your run was stopped before it finished and has now been reopened:") {
        return None;
    }
    Some(update.split_once("\n\n").map_or("", |(_, prompt)| prompt))
}

/// The items of a message Arko composed: the notes in its section of what
/// the user typed during the last turn, then the user's words.
fn composed(text: &str) -> Vec<Item> {
    let mut items: Vec<Item> = unread_notes(text).into_iter().map(note).collect();
    items.extend(user_item(text));
    items
}

/// The notes in the section of `text` that holds what the user typed during
/// the last turn, which the agent didn't read then.
fn unread_notes(text: &str) -> Vec<String> {
    let start = if text.starts_with(UNREAD_HEADING) {
        Some(0)
    } else {
        text.find(&format!("\n\n{UNREAD_HEADING}")).map(|at| at + 2)
    };
    let Some((_, list)) = start.and_then(|start| text[start..].split_once('\n')) else {
        return Vec::new();
    };
    let list = list.split("\n\n# ").next().unwrap_or(list);
    let mut notes: Vec<String> = Vec::new();
    for line in list.lines() {
        if let Some(first) = line.strip_prefix("- ") {
            notes.push(first.to_string());
        } else if let (Some(rest), Some(note)) = (line.strip_prefix("  "), notes.last_mut()) {
            note.push('\n');
            note.push_str(rest);
        }
    }
    notes
        .into_iter()
        .map(|note| note.trim().to_string())
        .filter(|note| !note.is_empty())
        .collect()
}

/// The user's notes in a context update: those from the operator, each a
/// list item of indented lines, without the line saying images follow.
/// The workflow's own notes and the agents' aren't the user's.
fn operator_notes(update: &str) -> Vec<String> {
    let mut notes = Vec::new();
    let mut note: Option<Vec<&str>> = None;
    for line in update.lines() {
        if let (Some(rest), Some(note)) = (line.strip_prefix("  "), note.as_mut()) {
            note.push(rest);
            continue;
        }
        notes.extend(note.take());
        if line.starts_with("- Note ") && line.contains(OPERATOR_NOTE) && line.ends_with(':') {
            note = Some(Vec::new());
        }
    }
    notes.extend(note);
    notes
        .into_iter()
        .map(|mut lines| {
            if lines.last().is_some_and(|last| {
                last.starts_with("(With ") && last.ends_with("shown after this update.)")
            }) {
                lines.pop();
            }
            lines.join("\n").trim().to_string()
        })
        .filter(|note| !note.is_empty())
        .collect()
}

/// A note the user typed during a turn, as the live transcript shows it.
fn note(text: String) -> Item {
    let mut item = Item::new(Kind::User, text);
    item.source = Some(Source::Terminal);
    item.delivery = Some(Delivery::Note);
    item
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
    /// own after the reply, begins a turn, and shows as the user's note.
    #[test]
    fn a_turn_for_what_was_typed_meanwhile_is_a_turn_of_its_own() {
        let messages = vec![
            Message::new_user("Slow."),
            Message::new_assistant("Slow 4."),
            Message::new_user("# What I typed during your last turn\n\n- Also the docs."),
            Message::new_assistant("Main 5."),
        ];
        let turns: Vec<(Kind, Option<Delivery>, Option<u64>)> =
            items(&messages, true, &Images::default())
                .iter()
                .map(|item| (item.kind, item.delivery, item.turn))
                .collect();
        assert_eq!(
            turns,
            [
                (Kind::User, Some(Delivery::Turn), Some(1)),
                (Kind::Reply, None, Some(1)),
                (Kind::User, Some(Delivery::Note), Some(2)),
                (Kind::Reply, None, Some(2))
            ]
        );
    }

    /// The notes the user typed during a turn read as notes, in their own
    /// words, where the agent got them. That's in a context update as it
    /// worked, or with the next message when it didn't read them in time.
    /// A stopped run reopened with the user's message shows that message,
    /// which can't be edited at its index, as in the live transcript. The
    /// workflow's own notes aren't the user's.
    #[test]
    fn notes_typed_during_a_turn_read_as_notes() {
        let mut look = Message::new_assistant("Looking.");
        look.tool_calls = vec![call(
            "call_1",
            crate::shell::SHELL_TOOL_NAME,
            serde_json::json!({"command": "ls"}),
        )];
        let result = Message::new_user_with_contents(vec![Content::ToolResult(ToolResult::new(
            Some("call_1".into()),
            crate::shell::SHELL_TOOL_NAME,
            "exit code 0, 0.0 s",
        ))]);
        let reopened = "Your run was stopped before it finished and has now been reopened: continue your task where you were. Tool calls reported as cancelled or stopped did not finish; check their effects before repeating them.";
        let messages = vec![
            Message::new_user("Count the fruit."),
            look,
            result,
            Message::new_user(
                "# Workflow context update\n\nNew notes on your task, oldest first. A note refines your task.\n- Note 1 from the workflow (the program running the agents), 2026-10-17 21:30 UTC:\n  Outside access is on.\n- Note 2 from the operator (the person running this workflow), 2026-10-17 21:30 UTC:\n  Also count the plums.\n  \n  And the pears.\n- Note 3 from the operator (the person running this workflow), 2026-10-17 21:31 UTC:\n  This one.\n  (With an image, shown after this update.)\n- (1 more note follows at your next turn.)\n\n# Reminder\nKeep going.",
            ),
            Message::new_assistant("Counted."),
            Message::new_user(
                "# What I typed during your last turn, which you didn't get to read\n\n- Also please count cherries.\n- Two\n  lines.\n\n# My message\n\nGo on.",
            ),
            Message::new_assistant("Going on."),
            Message::new_user(&format!(
                "# Workflow context update\n\n{reopened}\n\n# What I typed during your last turn, which you didn't get to read\n\n- And the figs.\n\n# My message\n\nThird."
            )),
            Message::new_assistant("Third done."),
            Message::new_user(&format!("# Workflow context update\n\n{reopened}")),
            Message::new_assistant("Recovered."),
        ];
        let items = items(&messages, true, &Images::default());
        // Text, delivery, message index and turn.
        type Shown<'a> = (&'a str, Option<Delivery>, Option<usize>, Option<u64>);
        let users: Vec<Shown> = items
            .iter()
            .filter(|item| item.kind == Kind::User)
            .map(|item| {
                (
                    item.text.as_str(),
                    item.delivery,
                    item.message_index,
                    item.turn,
                )
            })
            .collect();
        assert_eq!(
            users,
            [
                ("Count the fruit.", Some(Delivery::Turn), Some(0), Some(1)),
                (
                    "Also count the plums.\n\nAnd the pears.",
                    Some(Delivery::Note),
                    None,
                    Some(1)
                ),
                ("This one.", Some(Delivery::Note), None, Some(1)),
                (
                    "Also please count cherries.",
                    Some(Delivery::Note),
                    None,
                    Some(2)
                ),
                ("Two\nlines.", Some(Delivery::Note), None, Some(2)),
                ("Go on.", Some(Delivery::Turn), Some(5), Some(2)),
                ("And the figs.", Some(Delivery::Note), None, Some(3)),
                ("Third.", Some(Delivery::Turn), None, Some(3)),
            ]
        );
        assert_eq!(items.last().unwrap().turn, Some(4));
    }

    /// An agent in Claude Code has its response's text saved as a message
    /// of its own before the calls. That text is what it wrote beside them,
    /// not its reply, and the turn goes on, also through a note it got with
    /// a call's result.
    #[test]
    fn text_saved_apart_from_its_calls_reads_as_text() {
        let calls = |id: &str, command: &str| {
            let mut message = Message::new_assistant("");
            message.tool_calls = vec![call(
                id,
                crate::shell::SHELL_TOOL_NAME,
                serde_json::json!({ "command": command }),
            )];
            message
        };
        let result = |id: &str| {
            Message::new_user_with_contents(vec![Content::ToolResult(ToolResult::new(
                Some(id.into()),
                crate::shell::SHELL_TOOL_NAME,
                "exit code 0, 0.1 s",
            ))])
        };
        let messages = vec![
            Message::new_user("Count the words."),
            Message::new_assistant("First I'll write count.py."),
            calls("call_1", "touch count.py"),
            result("call_1"),
            Message::new_user(
                "# Workflow context update\n\nNew notes on your task, oldest first. A note refines your task.\n- Note 1 from the operator (the person running this workflow), 2026-10-21 13:20 UTC:\n  Skip empty files.",
            ),
            Message::new_assistant("It's written. Next I'll run it."),
            calls("call_2", "python3 count.py"),
            result("call_2"),
            Message::new_assistant("There are 3 files."),
            Message::new_user("Thanks."),
            Message::new_assistant("You're welcome."),
        ];
        let shown: Vec<(Kind, String, Option<u64>)> = items(&messages, true, &Images::default())
            .into_iter()
            .filter(|item| matches!(item.kind, Kind::Text | Kind::Reply | Kind::User))
            .map(|item| (item.kind, item.text, item.turn))
            .collect();
        let text = |text: &str| text.to_string();
        assert_eq!(
            shown,
            [
                (Kind::User, text("Count the words."), Some(1)),
                (Kind::Text, text("First I'll write count.py."), Some(1)),
                (Kind::User, text("Skip empty files."), Some(1)),
                (Kind::Text, text("It's written. Next I'll run it."), Some(1)),
                (Kind::Reply, text("There are 3 files."), Some(1)),
                (Kind::User, text("Thanks."), Some(2)),
                (Kind::Reply, text("You're welcome."), Some(2)),
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

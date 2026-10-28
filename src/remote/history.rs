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
use crate::crew;
use crate::images::Images;
use crate::openings::{Opening, Subjects};

/// The messages read back at most (the feed keeps fewer items anyway).
const MESSAGES: usize = 600;

/// The message that kept the prompt cache warm, as Arko worded it before.
const EARLIER_REFRESH_MESSAGE: &str = "(Arko: this message only keeps the prompt cache warm. Reply with \"Acknowledged\" and nothing else.)";

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
    // The runs the agent started and hasn't been told are over, which a
    // wait without ids waits for. (A run that reported in a note, between
    // two waits, stays in until a wait's result lists what still runs.)
    let mut running: Vec<String> = Vec::new();
    // The user's answers to questions the agent waited for.
    let answers = waited_answers(messages);
    for (index, message) in messages.iter().enumerate() {
        let start = items.len();
        if starts_turn(message)
            && !answers.contains(&index)
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
                            // The questions' ids are in the result. (The
                            // answers come as the user's messages and notes.)
                            if tool == ASK_USER_TOOL_NAME {
                                card_ids(&mut items[index], &result.output);
                                continue;
                            }
                            let outcome = if result.is_error {
                                render::Outcome::failed(&result.output)
                            } else {
                                render::outcome(&tool, &result.output)
                            };
                            if !result.is_error {
                                track_runs(&mut running, &tool, &result.output);
                            }
                            let call = &mut items[index];
                            call.status = Some(outcome.status);
                            call.outcome = outcome.text;
                            call.output = outcome.output;
                            // The job a moved call went on in, which may
                            // have ended since.
                            call.job = outcome.job.map(|job| job.to_string());
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
                        Content::Text(text) if answers.contains(&index) => {
                            for item in answer_items(text) {
                                mine = Some(push(&mut items, item));
                            }
                        }
                        Content::Text(text) => {
                            let about = |opening| subjects_of(opening, messages, index);
                            for (mut item, editable) in user_items(text, &about) {
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
                let reply = text_replies && is_reply(messages, index, &answers);
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
                        ASK_USER_TOOL_NAME => Some(question_card(parameters.get("questions"))),
                        name => render::call(name, &parameters, &running),
                    };
                    if call.name == crew::STOP_AGENT {
                        let stopped = text("run");
                        running.retain(|run| !run.eq_ignore_ascii_case(&stopped));
                    }
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
                    // The user may have been asked to approve the profile:
                    // the feed gives this card what the session kept of it,
                    // or leaves it out.
                    if call.name == crew::SAVE_WORKER_PROFILE
                        && let Some(id) = &call.id
                    {
                        push(&mut items, approval_card(id));
                    }
                }
            }
        }
        if message.role == Role::Assistant {
            ended = message
                .tool_calls
                .iter()
                .any(|call| call.name == "complete_task")
                || (text_replies && is_reply(messages, index, &answers));
        }
        for item in &mut items[start..] {
            item.turn = turn;
        }
    }
    items
}

/// The card of an `ask_user` call that asked `questions` (a list, or one
/// question as text). Its text is the questions as the user reads them,
/// which it shows when the session no longer keeps what became of them.
fn question_card(questions: Option<&serde_json::Value>) -> Item {
    let asked = questions
        .and_then(|questions| arko_agents::questions::parse_questions(questions).ok())
        .unwrap_or_default();
    let mut placeholders: Vec<crate::questions::AgentQuestion> = asked
        .iter()
        .map(|question| crate::questions::AgentQuestion::placeholder(&question.question))
        .collect();
    if placeholders.len() > 1 {
        for (number, question) in (1..).zip(&mut placeholders) {
            question.number = Some(number);
        }
    }
    let text = placeholders
        .iter()
        .map(crate::questions::AgentQuestion::numbered)
        .collect::<Vec<_>>()
        .join("\n");
    let mut item = Item::new(Kind::Question, text);
    // The questions, for their ids once the result names them.
    item.questions = (!placeholders.is_empty()).then_some(placeholders);
    item
}

/// The card of the approval the save `call` (the model's id of the call)
/// may have asked for, which the feed fills from what the session kept. Its
/// placeholder question carries the call's id.
fn approval_card(call: &str) -> Item {
    let mut question = crate::questions::AgentQuestion::placeholder("");
    question.id = call.to_string();
    question.approval = Some(crate::questions::Approval::default());
    let mut item = Item::new(Kind::Question, String::new());
    item.questions = Some(vec![question]);
    item
}

/// Gives the questions of `card` the ids its `ask_user` result names.
/// Without ids (a question asked before questions had them), the card has
/// none.
fn card_ids(card: &mut Item, result: &str) {
    let ids = crate::questions::ids_in_result(result);
    let Some(questions) = card.questions.as_mut() else {
        return;
    };
    if ids.len() != questions.len() {
        card.questions = None;
        return;
    }
    for (question, id) in questions.iter_mut().zip(ids) {
        question.id = id;
    }
}

/// Follows the runs the agent has going through the results of its calls.
/// `start_agent` names the run it started, and `wait_for_agents` ends with
/// the runs that still go.
fn track_runs(running: &mut Vec<String>, tool: &str, output: &str) {
    if tool == crew::START_AGENT {
        // "Started r2 (agent `worker`)." or "Queued r2 ...".
        let mut words = output.split_whitespace();
        if let (Some("Started" | "Queued"), Some(run)) = (words.next(), words.next())
            && !running.iter().any(|known| known == run)
        {
            running.push(run.to_string());
        }
    } else if tool == crew::WAIT_FOR_AGENTS {
        for line in output.lines() {
            if let Some(rest) = line.trim().strip_prefix("Still running: ") {
                *running = rest
                    .trim_end_matches('.')
                    .split(", ")
                    .map(str::to_string)
                    .collect();
            } else if line
                .trim()
                .starts_with("None of your agent runs is running")
            {
                running.clear();
            }
        }
    }
}

/// A stopped run gets this notice inside a context update when it restarts.
/// Other context updates arrive during a turn and keep that turn's number.
fn reopened(text: &str) -> bool {
    text.trim_start().starts_with("# Workflow context update")
        && text.lines().any(|line| line.starts_with(REOPENED))
}

/// Whether the agent's message at `index` ends its turn when it replies in
/// text. That's text alone, with neither the answer to its question after
/// it nor more of its response. Claude Code's agents save the text of a
/// response apart from its calls, as a message of its own before them.
fn is_reply(messages: &[Message], index: usize, answers: &[usize]) -> bool {
    messages[index].tool_calls.is_empty()
        && messages.get(index + 1).is_none_or(|next| {
            next.role != Role::Assistant && !is_answer(next) && !answers.contains(&(index + 1))
        })
}

/// The messages that answer questions the agent waited for: the first
/// message of the user's own text after an `ask_user` result that didn't
/// return at once (context updates come apart from it).
fn waited_answers(messages: &[Message]) -> Vec<usize> {
    let mut waiting = false;
    let mut answers = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role != Role::User {
            continue;
        }
        for content in &message.content {
            match content {
                Content::ToolResult(result) if result.name == ASK_USER_TOOL_NAME => {
                    waiting = !crate::questions::returned_at_once(&result.output);
                }
                Content::Text(text)
                    if waiting && !text.trim_start().starts_with("# Workflow context update") =>
                {
                    answers.push(index);
                    waiting = false;
                }
                _ => {}
            }
        }
    }
    answers
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
/// live transcript doesn't offer to edit it either. The news that went
/// with it there (a monitor's alert, a job's update) isn't theirs.
fn user_items(text: &str, about: &dyn Fn(Opening) -> Subjects) -> Vec<(Item, bool)> {
    let Some(update) = context_update(text) else {
        return composed(text, about)
            .into_iter()
            .map(|item| (item, true))
            .collect();
    };
    let items = match reopened_with(update) {
        Some((prompt, rest)) => {
            let mut items = composed(prompt, about);
            items.extend(operator_notes(rest).into_iter().filter_map(note));
            items
        }
        None => operator_notes(update)
            .into_iter()
            .filter_map(note)
            .collect(),
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

/// How the notice that reopens a stopped run begins.
const REOPENED: &str = "Your run was stopped before it finished and has now been reopened:";

/// The message Arko composed for a stopped run it reopened (empty without
/// one), which follows the notice in the context update, then the rest of
/// the update. Other context can come before the notice (news that waited
/// when the turn stopped) and after the message (news that came since).
fn reopened_with(update: &str) -> Option<(&str, &str)> {
    let start = if update.starts_with(REOPENED) {
        0
    } else {
        update.find(&format!("\n\n{REOPENED}"))? + 2
    };
    let Some((_, after)) = update[start..].split_once("\n\n") else {
        return Some(("", ""));
    };
    let end = std::iter::once(0)
        .chain(after.match_indices("\n\n").map(|(at, _)| at + 2))
        .find(|&at| is_news(&after[at..]));
    Some(match end {
        Some(end) => (after[..end].trim_end(), &after[end..]),
        None => (after, ""),
    })
}

/// Whether `text` begins with a note of the terminal's jobs or monitors,
/// as their news reaches the agent: a monitor's alert or other news, a
/// command's terminal update, a moved call's update.
fn is_news(text: &str) -> bool {
    let named = |start: &str| {
        text.strip_prefix(start)
            .and_then(|rest| rest.get(..36))
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
    };
    text.starts_with("Monitor alert. ")
        || text.starts_with("Terminal update:\n")
        || named("Monitor ")
        || named("Job ")
}

/// The items of a message Arko composed: the notes in its section of what
/// the user typed during the last turn, then the user's words.
fn composed(text: &str, about: &dyn Fn(Opening) -> Subjects) -> Vec<Item> {
    let mut items: Vec<Item> = unread_notes(text).into_iter().filter_map(note).collect();
    items.extend(user_item(text, about));
    items
}

/// Who and what the turn `opening` opens (in the message at `index`) is for,
/// from the news that went to the agent with it, in that message and the
/// context updates right after it.
fn subjects_of(opening: Opening, messages: &[Message], index: usize) -> Subjects {
    let texts = |message: &Message| -> Vec<String> {
        message
            .content
            .iter()
            .filter_map(Content::as_text)
            .map(str::to_string)
            .collect()
    };
    let mut news = texts(&messages[index]);
    for message in &messages[index + 1..] {
        let updates: Vec<String> = texts(message)
            .into_iter()
            .filter(|text| text.trim_start().starts_with("# Workflow context update"))
            .collect();
        if message.role != Role::User || updates.is_empty() {
            break;
        }
        news.extend(updates);
    }
    Subjects::in_news(opening, &news.join("\n\n"), |job| {
        job_ran(&messages[..index], job)
    })
}

/// What job `id` ran, from the call in `messages` that started it or moved
/// to it, whose result names the job: its command, or a call as it is shown.
pub(crate) fn job_ran(messages: &[Message], id: uuid::Uuid) -> Option<String> {
    let id = id.to_string();
    let call_id = messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult(result) if result.output.contains(&id) => result.call_id.clone(),
            _ => None,
        })?;
    let call = messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .find(|call| call.id.as_deref() == Some(call_id.as_str()))?;
    let parameters: HashMap<String, serde_json::Value> = call
        .arguments
        .as_object()
        .map(|object| object.clone().into_iter().collect())
        .unwrap_or_default();
    Some(
        match parameters
            .get("command")
            .and_then(|command| command.as_str())
        {
            Some(command) => crate::jobs::label(command),
            None => crate::jobs::label(&crate::ui::brief(&call.name, &parameters)),
        },
    )
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

/// A note the user typed during a turn, as the live transcript shows it:
/// the user's words. A note Arko wrote about the agent's questions has
/// none (their card shows the answers).
fn note(text: String) -> Option<Item> {
    if is_answer_note(&text) {
        return None;
    }
    let mut item = Item::new(Kind::User, unlabelled(&text));
    item.source = Some(Source::Terminal);
    item.delivery = Some(Delivery::Note);
    Some(item)
}

/// The items of the user's answer to questions the agent waited for: their
/// message. (A note of the answers, which their card shows, and the wait's
/// own replies, which point to that note, have none.)
fn answer_items(text: &str) -> Vec<Item> {
    let trimmed = text.trim();
    if trimmed.ends_with(crate::questions::WAIT_ANSWERED)
        || trimmed.ends_with(crate::questions::READ_ALREADY)
        || is_answer_note(text)
    {
        return Vec::new();
    }
    let words = unlabelled(text);
    if words.is_empty() {
        Vec::new()
    } else {
        vec![answer_item(words)]
    }
}

fn answer_item(text: &str) -> Item {
    let mut item = Item::new(Kind::User, text.trim());
    item.source = Some(Source::Terminal);
    item.delivery = Some(Delivery::Answer);
    item
}

/// How the notes Arko writes about the agent's questions begin.
const ANSWER_NOTES: [&str; 2] = ["The user answered q", "The user dismissed q"];

/// Whether `text` is a note Arko wrote about the agent's questions.
fn is_answer_note(text: &str) -> bool {
    let text = text.trim();
    ANSWER_NOTES.iter().any(|start| text.starts_with(start))
}

/// The user's words of a message Arko labelled for the agent, without
/// "(This message answers q3.)" before them and "(Still open: q5.)" after
/// them.
fn unlabelled(text: &str) -> &str {
    let mut text = text.trim();
    if text.starts_with("(This message answers q")
        && let Some((_, words)) = text.split_once(".)\n")
    {
        text = words;
    }
    if text.ends_with(".)")
        && let Some(at) = text.rfind("\n\n(Still open: q")
    {
        text = &text[..at];
    }
    text.trim()
}

/// What the user wrote, from a message of theirs. Arko's composed message
/// keeps it under "# My message". Context updates aren't the user's words,
/// nor is a handover's report. A turn Arko started for news (the workers',
/// a job's, a monitor's) shows the line the live transcript showed.
fn user_item(text: &str, about: &dyn Fn(Opening) -> Subjects) -> Option<Item> {
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
    let mine = match text.split_once("# My message\n\n") {
        Some((_, mine)) => mine,
        None => {
            if let Some(opening) = Opening::of(text) {
                return opening.notice(&about(opening)).map(|notice| {
                    Item::new(Kind::Notice, notice)
                        .level(super::Level::Info)
                        .opens_turn()
                });
            }
            // Only Arko's own sections, or its notes about the agent's
            // questions: no words of the user's.
            if text.starts_with("# ") || ANSWER_NOTES.iter().any(|start| text.starts_with(start)) {
                return None;
            }
            text
        }
    };
    let mut item = Item::new(Kind::User, unlabelled(mine));
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

    /// A wait without ids waited for a message before the agent started
    /// any run, then for the runs it started that no result told to be
    /// over, and for a message again once a wait's result says none go.
    #[test]
    fn a_reopened_wait_names_the_runs_the_agent_had_going() {
        let steps = [
            (
                crew::WAIT_FOR_AGENTS,
                serde_json::json!({}),
                "Nothing to wait for: none of your agent runs is running. Reports that came earlier reached you already.",
                "waiting for a message",
            ),
            (
                crew::START_AGENT,
                serde_json::json!({"task": "A"}),
                "Started r1 (agent `worker`).",
                "start worker “A”",
            ),
            (
                crew::START_AGENT,
                serde_json::json!({"task": "B"}),
                "Queued r2 (agent `worker`).",
                "start worker “B”",
            ),
            (
                crew::WAIT_FOR_AGENTS,
                serde_json::json!({}),
                "[mail 1] r1 (agent `worker`) finished. Its report:\n\nA.\n\nStill running: r2.",
                "waiting for r1, r2",
            ),
            (
                crew::WAIT_FOR_AGENTS,
                serde_json::json!({"timeout_seconds": 5}),
                "Still running: r2.",
                "waiting for r2",
            ),
            (
                crew::STOP_AGENT,
                serde_json::json!({"run": "r2"}),
                "Stopped r2.",
                "stop r2",
            ),
            (
                crew::WAIT_FOR_AGENTS,
                serde_json::json!({}),
                "None of your agent runs is running now.",
                "waiting for a message",
            ),
            (
                crew::WAIT_FOR_AGENTS,
                serde_json::json!({"runs": "r1"}),
                "Nothing to wait for: r1 is finished.",
                "waiting for r1",
            ),
        ];
        let mut messages = vec![Message::new_user("Go.")];
        for (index, (tool, arguments, output, _)) in steps.iter().enumerate() {
            let id = format!("call_{index}");
            let mut ask = Message::new_assistant("");
            ask.tool_calls = vec![call(&id, tool, arguments.clone())];
            messages.push(ask);
            messages.push(Message::new_user_with_contents(vec![Content::ToolResult(
                ToolResult::new(Some(id), tool, output),
            )]));
        }
        let items = all_items(&messages, false, &Images::default());
        let calls: Vec<&str> = items
            .iter()
            .filter(|item| item.kind == Kind::Call)
            .map(|item| item.text.as_str())
            .collect();
        let expected: Vec<&str> = steps.iter().map(|step| step.3).collect();
        assert_eq!(calls, expected);
    }

    /// A call that moved to the background names its job, which may have
    /// ended since.
    #[test]
    fn a_moved_call_names_its_job() {
        let job = uuid::Uuid::new_v4();
        let mut ask = Message::new_assistant("");
        ask.tool_calls = vec![call("call_1", "mcp__search__lookup", serde_json::json!({}))];
        let result = Message::new_user_with_contents(vec![Content::ToolResult(ToolResult::new(
            Some("call_1".into()),
            "mcp__search__lookup",
            &format!(
                "in the background as job {job}, 95.3 s\nThe user moved this call to the background."
            ),
        ))]);
        let messages = [Message::new_user("Look it up."), ask, result];
        let items = all_items(&messages, false, &Images::default());
        let call = items.iter().find(|item| item.kind == Kind::Call).unwrap();
        assert_eq!(call.job, Some(job.to_string()));
        assert!(
            call.outcome
                .as_deref()
                .is_some_and(|outcome| outcome.starts_with("in the background"))
        );
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

    /// The turns Arko started for news show the lines the live transcript
    /// showed, also after other sections and as worded before. The news
    /// that went with a reopened run's message isn't the user's.
    #[test]
    fn arkos_turns_for_news_read_as_its_notices() {
        let id = "b57d52ff-2c3e-4c4b-9a55-0d1f6a1e2b3c";
        let alert = format!(
            "Monitor alert. Job {id} (`while true; do echo ERROR tick; sleep 5; done`, lines that match `ERROR`) printed 2 matching lines since 22:47:55 UTC.\n  ERROR tick\n  ERROR tick\nIt ends in 22 min."
        );
        let reopened = "Your run was stopped before it finished and has now been reopened: continue your task where you were. Tool calls reported as cancelled or stopped did not finish; check their effects before repeating them.";
        let messages = vec![
            Message::new_user("Watch it."),
            Message::new_assistant("Watching."),
            Message::new_user(&Opening::Alert.text()),
            Message::new_user(&format!("# Workflow context update\n\n{alert}")),
            Message::new_assistant("It logs errors."),
            Message::new_user(&format!(
                "# Session capabilities\n\nNo images.\n\n{}",
                Opening::EndsSoon(Some(25)).text()
            )),
            Message::new_assistant("Kept it."),
            Message::new_user(&Opening::Job.text()),
            Message::new_assistant("Built."),
            Message::new_user(
                "Your workers sent you news as your last turn ended: it follows. (Arko started this turn for it; the user hasn't written.)",
            ),
            Message::new_assistant("Noted."),
            Message::new_user(&format!(
                "# Workflow context update\n\n{reopened}\n\nhello\n\nthere\n\n{alert}\n\nTerminal update:\n{{}}"
            )),
            Message::new_assistant("Hi."),
            Message::new_user(&format!(
                "# Workflow context update\n\nJob {id} ended. It ran your search call, which moved to the background 1 min ago. Its result follows.\n\nfound\n\n{reopened}\n\n# My message\n\nAgain."
            )),
            Message::new_assistant("Again."),
        ];
        let shown: Vec<(Kind, String, Option<u64>)> = items(&messages, true, &Images::default())
            .into_iter()
            .filter(|item| matches!(item.kind, Kind::User | Kind::Notice))
            .map(|item| (item.kind, item.text, item.turn))
            .collect();
        let notice = |text: &str, turn| (Kind::Notice, text.to_string(), Some(turn));
        assert_eq!(
            shown,
            [
                (Kind::User, "Watch it.".to_string(), Some(1)),
                notice(
                    "Monitor alerted · while true; do echo ERROR tick; sleep 5; done",
                    2
                ),
                notice("Monitor ends in 25 min", 3),
                notice("Job finished", 4),
                notice("News from workers", 5),
                (Kind::User, "hello\n\nthere".to_string(), Some(6)),
                (Kind::User, "Again.".to_string(), Some(7)),
            ]
        );
        // The lines are marked as opening a turn, as the live ones are, and
        // the user's words aren't.
        let marks: Vec<(Kind, bool)> = items(&messages, true, &Images::default())
            .into_iter()
            .filter(|item| matches!(item.kind, Kind::User | Kind::Notice))
            .map(|item| (item.kind, item.opens_turn))
            .collect();
        assert_eq!(
            marks,
            [
                (Kind::User, false),
                (Kind::Notice, true),
                (Kind::Notice, true),
                (Kind::Notice, true),
                (Kind::Notice, true),
                (Kind::User, false),
                (Kind::User, false),
            ]
        );
    }

    /// The line of a turn Arko started names the workers, jobs and monitors
    /// its news is from, as the live transcript did. A job is named by the
    /// call that started it or moved to it.
    #[test]
    fn arkos_turns_for_news_name_who_and_what() {
        let (job, started, monitor) = (
            "4d2c1a9b-0f3e-4c7a-9b1e-5a6d7c8e9f00",
            "b57d52ff-2c3e-4c4b-9a55-0d1f6a1e2b3c",
            "1f0c2d3e-0000-4000-8000-000000000000",
        );
        let moved_call = |id: &str, command: &str| {
            let mut message = Message::new_assistant("");
            message.tool_calls = vec![call(id, "shell", serde_json::json!({"command": command}))];
            message
        };
        let result = |id: &str, tool: &str, output: String| {
            Message::new_user_with_contents(vec![Content::ToolResult(ToolResult::new(
                Some(id.into()),
                tool,
                &output,
            ))])
        };
        let mut start = Message::new_assistant("");
        start.tool_calls = vec![call(
            "bg",
            "terminal_start",
            serde_json::json!({"command": "npm run dev"}),
        )];
        let mail = "# Workflow context update\n\nNew mail from the agent runs you work with, oldest first. Reply with message_agent; wait_for_agents waits for more.\n- [mail 3] r1 (agent `worker`) finished. Its report:\n  Done.\n  [end of mail 3]\n- [mail 4] r2 (agent `worker`) sent you a message (it goes on working):\n  Which branch?\n  [end of mail 4]";
        let messages = vec![
            Message::new_user("Build."),
            moved_call("sh", "cargo build --release"),
            result(
                "sh",
                "shell",
                format!("in the background as job {job}, 95.3 s\nIt ran longer than 10 minutes."),
            ),
            start,
            result(
                "bg",
                "terminal_start",
                format!("{{\"job_id\":\"{started}\",\"state\":\"running\"}}"),
            ),
            Message::new_assistant("Going on."),
            Message::new_user(&Opening::Job.text()),
            Message::new_user(&format!(
                "# Workflow context update\n\nTerminal update:\n{{\"job_id\":\"{job}\",\"revision\":2,\"state\":\"succeeded\"}}\n\nTerminal update:\n{{\"job_id\":\"{started}\",\"revision\":2,\"state\":\"failed\"}}"
            )),
            Message::new_assistant("Both ended."),
            Message::new_user(&Opening::News.text()),
            Message::new_user(mail),
            Message::new_assistant("Noted."),
            Message::new_user(&Opening::EndsSoon(Some(2)).text()),
            Message::new_user(&format!(
                "# Workflow context update\n\nMonitor {monitor} (`npm run dev`) ends in 2 min, at 11:00 UTC. monitor_control rearm keeps it going."
            )),
            Message::new_assistant("Kept it."),
        ];
        let shown: Vec<String> = items(&messages, true, &Images::default())
            .into_iter()
            .filter(|item| item.kind == Kind::Notice)
            .map(|item| item.text)
            .collect();
        assert_eq!(
            shown,
            [
                "2 jobs finished · cargo build --release and npm run dev",
                "r1 reported · r2 sent a message",
                "Monitor ends in 2 min · npm run dev",
            ]
        );
    }

    /// The turn Arko started for the user's answers shows no line of its
    /// own, as the live transcript shows none, also in a session saved when
    /// it showed one.
    #[test]
    fn a_turn_for_the_users_answers_shows_no_notice() {
        let messages = vec![
            Message::new_user("Indent the file."),
            Message::new_assistant("Asked, and going on."),
            Message::new_user(&Opening::Answers.text()),
            Message::new_user(
                "# Workflow context update\n\n- Note 1 from the operator (the person running this workflow), at 10:00:\n  The user answered q1.\n  q1 Tabs or spaces? Spaces",
            ),
            Message::new_assistant("Used spaces."),
        ];
        let shown: Vec<(Kind, String, Option<u64>)> = items(&messages, true, &Images::default())
            .into_iter()
            .map(|item| (item.kind, item.text, item.turn))
            .collect();
        assert_eq!(
            shown,
            [
                (Kind::User, "Indent the file.".to_string(), Some(1)),
                (Kind::Reply, "Asked, and going on.".to_string(), Some(1)),
                (Kind::Reply, "Used spaces.".to_string(), Some(2)),
            ]
        );
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

    /// An ask's card holds its questions with the ids its result names, in
    /// both forms of `ask_user`, and a question asked before ids has none.
    #[test]
    fn question_cards_take_their_ids_from_the_result() {
        for (arguments, output, ids) in [
            (
                serde_json::json!({"questions": [{"question": "Tabs or spaces?"}, "Push now?"], "wait": false}),
                "Asked q3 and q4. Their answers reach you as a note. Until then, do only work that no answer could change.",
                Some(vec!["q3", "q4"]),
            ),
            (
                serde_json::json!({"questions": [{"question": "Tabs or spaces?"}, "Push now?"]}),
                "Asked q3 and q4. Their answers reach you as a note, so go on with your work.",
                Some(vec!["q3", "q4"]),
            ),
            (
                serde_json::json!({"questions": "Which file?", "wait": true}),
                "q5 Which file?",
                Some(vec!["q5"]),
            ),
            (
                serde_json::json!({"questions": "Which file?"}),
                "Which file?",
                None,
            ),
        ] {
            let mut ask = Message::new_assistant("");
            ask.tool_calls = vec![call("ask", ASK_USER_TOOL_NAME, arguments)];
            let result = Message::new_user_with_contents(vec![Content::ToolResult(
                ToolResult::new(Some("ask".into()), ASK_USER_TOOL_NAME, output),
            )]);
            let messages = vec![Message::new_user("Start."), ask, result];
            let items = all_items(&messages, false, &Images::default());
            let card = items
                .iter()
                .find(|item| item.kind == Kind::Question)
                .unwrap();
            assert_eq!(
                card.questions
                    .as_ref()
                    .map(|questions| questions.iter().map(|q| q.id.as_str()).collect::<Vec<_>>()),
                ids,
                "{output}"
            );
        }
    }

    fn texts(items: &[Item]) -> Vec<(Kind, &str, Option<Delivery>, Option<u64>)> {
        items
            .iter()
            .map(|item| (item.kind, item.text.as_str(), item.delivery, item.turn))
            .collect()
    }

    fn completed(summary: &str) -> Message {
        let mut done = Message::new_assistant("");
        done.content.clear();
        done.tool_calls = vec![call(
            &format!("done-{summary}"),
            "complete_task",
            serde_json::json!({"summary": summary}),
        )];
        done
    }

    /// What Arko added for the agent around the user's words (the
    /// questions a message answers, those still open) is left out, and its
    /// notes about the agent's questions show nothing (their cards show the
    /// answers), as in the live transcript.
    #[test]
    fn a_resumed_history_shows_the_users_words_without_arkos_labels() {
        let messages = vec![
            Message::new_user("(This message answers q1.)\nSpaces, please."),
            completed("One."),
            Message::new_user("Hello.\n\n(Still open: q2.)"),
            completed("Two."),
            Message::new_user(
                "# Workflow context update\n\n- Note 1 from the operator (the person running this workflow), at 10:00:\n  The user answered q2.\n  q2 Push now? Not yet\n- Note 2 from the operator (the person running this workflow), at 10:01:\n  (This message answers q3.)\n  main\n\n  (Still open: q4.)",
            ),
            completed("Three."),
            Message::new_user("The user dismissed q4."),
            completed("Four."),
            Message::new_user(
                "The user answered q5.\nq5 Why? Because\n\n# My message\n\n(This message answers q6.)\nAnd this.\n\n(Still open: q7.)",
            ),
            completed("Five."),
        ];
        let items = all_items(&messages, false, &Images::default());
        let users: Vec<_> = texts(&items)
            .into_iter()
            .filter(|(kind, ..)| *kind == Kind::User)
            .map(|(_, text, delivery, _)| (text, delivery))
            .collect();
        assert_eq!(
            users,
            [
                ("Spaces, please.", Some(Delivery::Turn)),
                ("Hello.", Some(Delivery::Turn)),
                ("main", Some(Delivery::Note)),
                ("And this.", Some(Delivery::Turn)),
            ]
        );
    }

    /// The answer to questions the agent waited for stays in their turn, in
    /// both forms Arko sends it: a note of the answers (which their card
    /// shows), or the user's message. A reply to the wait that points to a
    /// note shows nothing, also one saying the agent read the note already.
    #[test]
    fn a_waited_answer_stays_in_its_turn() {
        let read = crate::questions::wait_answered_earlier(&["q1".to_string()]);
        for (answer, shown) in [
            ("The user answered q1.\nq1 Which file? README.md", None),
            ("(This message answers q1.)\nREADME.md", Some("README.md")),
            (crate::questions::WAIT_ANSWERED, None),
            (read.as_str(), None),
        ] {
            let mut ask = Message::new_assistant("");
            ask.tool_calls = vec![call(
                "ask",
                ASK_USER_TOOL_NAME,
                serde_json::json!({"questions": ["Which file?"], "wait": true}),
            )];
            let messages = vec![
                Message::new_user("Start."),
                ask,
                Message::new_user_with_contents(vec![Content::ToolResult(ToolResult::new(
                    Some("ask".into()),
                    ASK_USER_TOOL_NAME,
                    "q1 Which file?",
                ))]),
                Message::new_user(answer),
                completed("Done."),
            ];
            let items = all_items(&messages, false, &Images::default());
            let mut expected = vec![
                (Kind::User, "Start.", Some(Delivery::Turn), Some(1)),
                (Kind::Question, "Which file?", None, Some(1)),
                (Kind::Reply, "Done.", None, Some(1)),
            ];
            if let Some(shown) = shown {
                expected.insert(2, (Kind::User, shown, Some(Delivery::Answer), Some(1)));
            }
            assert_eq!(texts(&items), expected, "{answer}");
            assert_eq!(items[1].questions.as_ref().unwrap()[0].id, "q1");
        }
    }

    /// In Claude Code the agent ends its turn after asking: its text before
    /// the answer isn't its reply, the answer keeps the turn, and the card
    /// takes its id from the call's result.
    #[test]
    fn a_waited_answer_in_claude_code_stays_in_its_turn() {
        let mut ask = Message::new_assistant("");
        ask.tool_calls = vec![call(
            "ask",
            ASK_USER_TOOL_NAME,
            serde_json::json!({"questions": ["Which file?"], "wait": true}),
        )];
        let messages = vec![
            Message::new_user("Start."),
            Message::new_assistant("Let me ask."),
            ask,
            Message::new_user_with_contents(vec![Content::ToolResult(ToolResult::new(
                Some("ask".into()),
                ASK_USER_TOOL_NAME,
                "Asked q1. End your turn now without calling tools, and the answer arrives as the next message.",
            ))]),
            Message::new_assistant("Waiting for your answer."),
            Message::new_user("The user answered q1.\nq1 Which file? README.md"),
            Message::new_assistant("Used README.md."),
        ];
        let items = all_items(&messages, true, &Images::default());
        assert_eq!(
            texts(&items),
            [
                (Kind::User, "Start.", Some(Delivery::Turn), Some(1)),
                (Kind::Text, "Let me ask.", None, Some(1)),
                (Kind::Question, "Which file?", None, Some(1)),
                (Kind::Text, "Waiting for your answer.", None, Some(1)),
                (Kind::Reply, "Used README.md.", None, Some(1)),
            ]
        );
        assert_eq!(items[2].questions.as_ref().unwrap()[0].id, "q1");
    }
}

//! The brief Arko writes of an earlier conversation, and the
//! `earlier_conversation` tool that reads it, for a conversation that goes
//! on from another one (when the main agent's model is switched
//! mid-session, or the user types `/new continue`).
//!
//! Arko writes the brief from what it saved, without a model, so it works
//! whatever state the old model is in. It is a map of the old conversation
//! sized to the new model (see [`budget`]), and the tool reads the rest as
//! the work needs it.
//!
//! Both read a conversation the same way. Its turns are numbered from 1
//! across its runs, by the transcript's rule ([`remote::message_turns`]), as
//! a saved conversation's export numbers them. A turn's parts are numbered
//! from 1 in order. They are the reports of the handovers that replaced what
//! came before it, the user's messages, the agent's text and reply, its
//! calls with their results, and context updates. `12.3` is turn 12's third
//! part.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arko_agents::{
    TaskContext, Tool, ToolDefinition, ToolError, ToolParameterDefinition, ToolParameterType,
    ToolRecovery, ToolTypeDetails,
};
use arko_ai::{Content, Role};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::remote::{self, Delivery, Kind};

pub const TOOL_NAME: &str = "earlier_conversation";

/// The tokens a tool output holds by default, and at most and at least
/// with `limit`.
const OUTPUT_TOKENS: usize = 8_000;
const MAX_OUTPUT_TOKENS: usize = 20_000;
const MIN_OUTPUT_TOKENS: usize = 1_000;
/// The turns `outline` lists at once.
const OUTLINE_PAGE: usize = 50;
/// The characters `read` shows of a call's parameters, and of any other
/// part (a call's result, a message).
const PARAMETERS_SHOWN: usize = 300;
const PART_SHOWN: usize = 2_000;
/// The characters `result` shows at once.
const RESULT_PAGE: usize = 20_000;
/// The turns `search` finds at most.
const HITS: usize = 20;
/// The user's messages the brief lists at most, the newest.
const USER_LINES: usize = 60;
/// The most tokens the brief gives the goal.
const GOAL_TOKENS: usize = 1_500;
/// The characters of the first lines `outline` shows.
const LINE: usize = 120;

/// About how many tokens `text` takes, a token for each 4 bytes.
fn tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// A worker started in the earlier conversation, as the brief lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worker {
    /// Its handle, such as `r2`.
    pub handle: String,
    /// What it does now, such as running, waiting or done.
    pub state: String,
    /// Its task's first line.
    pub task: String,
}

/// What was open when the earlier conversation ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Open {
    /// The agent's questions still waiting for an answer.
    pub questions: Vec<String>,
    /// What the user typed during its last turn that the agent never read.
    pub unread: Vec<String>,
    /// How its last run ended, when that was unusual, such as "stopped by
    /// the user" or "paused at a plan limit".
    pub ended: Option<String>,
}

/// What [`brief`] is written from.
pub struct BriefParts<'a> {
    /// The earlier conversation's number, as `/resume` takes it.
    pub number: usize,
    /// Its model, as `/model` names it ("cc:opus", "gpt-6.1-sol").
    pub model: &'a str,
    /// Its runs' tasks, oldest first.
    pub runs: &'a [&'a arko_agents::Task],
    /// The report its model wrote for this switch, if it wrote one.
    pub report: Option<&'a str>,
    pub workers: &'a [Worker],
    pub open: &'a Open,
    /// The tokens for everything but `report` (see [`budget`]).
    pub budget: usize,
}

/// The brief's budget for a model whose input limit is `limit` tokens, 4%
/// of it, at least 6k and at most 24k tokens.
pub fn budget(limit: Option<usize>) -> usize {
    // A model whose limit isn't known gets the share of one of 300k tokens,
    // which is about what the models Arko runs without a known limit take.
    let limit = limit.unwrap_or(300_000);
    (limit / 25).clamp(6_000, 24_000)
}

/// The brief, the new conversation's first section, under the heading
/// "# Earlier conversation" (which it begins with). Its parts come in this
/// order. A header line names the conversation and says how to read it with
/// the tool. Then come the goal (the user's first message), the report
/// (`report`, else the latest handover's), the user's messages, the files
/// written or edited, the workers, what is open, and the last exchanges in
/// full text, as many as fit, which fill what the others leave of the
/// budget. Everything but the report fits in `budget` tokens.
pub fn brief(parts: &BriefParts) -> String {
    let transcript = Transcript::of(parts.runs);
    let budget = parts.budget;
    let mut sections = vec![header(parts, &transcript)];
    sections.extend(goal(parts, budget));
    // (Each section also takes the blank line before it.)
    let mut used: usize = sections.iter().map(|section| tokens(section) + 1).sum();
    // Outside the budget.
    sections.extend(report(parts));
    let rest = [
        user_lines(&transcript, budget * 3 / 10),
        files(&transcript, budget / 10),
        workers(parts.workers, budget / 10),
        open(parts.open, budget / 10),
    ];
    for section in rest.into_iter().flatten() {
        used += tokens(&section) + 1;
        sections.push(section);
    }
    sections.extend(exchanges(&transcript, budget.saturating_sub(used + 1)));
    sections.join("\n\n")
}

/// The brief's heading and header: the conversation's number, model and
/// size, and how the tool reads it.
fn header(parts: &BriefParts, transcript: &Transcript) -> String {
    let size = parts
        .runs
        .last()
        .and_then(|task| task.current_conversation_context_size())
        .unwrap_or_else(|| transcript.tokens());
    format!(
        "# Earlier conversation\n\nConversation {} on {}, {}, about {} tokens.\n\n\
         This conversation goes on from it. Below is a map of it, and `{TOOL_NAME}` reads the rest, so look things up there before you redo work or ask the user. \
         For example, `{TOOL_NAME} search` with words such as a path, a name or an error finds the turns that mention them, and `read` shows those turns. \
         The turn numbers here are the ones it takes.",
        parts.number,
        parts.model,
        counted(transcript.turns.len(), "turn", "turns"),
        short(size),
    )
}

/// The user's first message, cut to its share of the budget.
fn goal(parts: &BriefParts, budget: usize) -> Option<String> {
    let words = remote::own_words(&parts.runs.first()?.initial_task_prompt);
    if words.is_empty() {
        return None;
    }
    let room = (GOAL_TOKENS.min(budget / 5) * 4).saturating_sub(80);
    let (shown, cut) = cut_bytes(words, room);
    let mut section = format!("## Goal\n\nThe user's first message in it.\n\n{shown}");
    if cut {
        section.push_str("\n\n(The rest of it is cut.)");
    }
    Some(section)
}

/// The report the old model wrote for this switch, else the latest one it
/// wrote at a handover. (Not under the handover's own heading, whose
/// introduction tells the agent it wrote the report itself.)
fn report(parts: &BriefParts) -> Option<String> {
    let (report, about) = match parts.report.map(str::trim) {
        Some(report) if !report.is_empty() => (
            report.to_string(),
            format!(
                "{} wrote this report as conversation {} ended.",
                parts.model, parts.number
            ),
        ),
        _ => {
            let handover = parts
                .runs
                .iter()
                .rev()
                .find_map(|task| task.handovers.last())?;
            (
                handover.report.trim().to_string(),
                format!(
                    "The agent wrote this report when its context was replaced on {}, so later turns aren't in it.",
                    time(handover.at)
                ),
            )
        }
    };
    Some(format!("## Report\n\n{about}\n\n{report}"))
}

/// The user's messages, one line each with its turn, the newest
/// [`USER_LINES`] within `room` tokens.
fn user_lines(transcript: &Transcript, room: usize) -> Option<String> {
    let all: Vec<(usize, &str)> = transcript
        .turns
        .iter()
        .flat_map(|turn| turn.words.iter().map(|words| (turn.number, words.as_str())))
        .collect();
    if all.is_empty() {
        return None;
    }
    // Wide enough that the newest 60 fit (a line also holds its turn).
    let width = ((room * 4).saturating_sub(240) / USER_LINES)
        .saturating_sub(12)
        .clamp(80, 300);
    // Newest first.
    let mut lines: Vec<String> = all
        .iter()
        .rev()
        .take(USER_LINES)
        .map(|(turn, words)| format!("- {turn} {}", crate::ui::cut(words, width)))
        .collect();
    loop {
        let lead = if lines.len() < all.len() {
            format!(
                "Each line starts with its turn. The newest {} of {} are here, and `{TOOL_NAME} outline` lists every turn.",
                lines.len(),
                all.len()
            )
        } else {
            "Each line starts with its turn.".to_string()
        };
        let listed: Vec<&str> = lines.iter().rev().map(String::as_str).collect();
        let section = format!("## The user's messages\n\n{lead}\n\n{}", listed.join("\n"));
        if tokens(&section) <= room || lines.len() <= 1 {
            return Some(section);
        }
        lines.pop();
    }
}

/// The files the agent wrote or edited, each with the turns that did,
/// the latest touched first, within `room` tokens.
fn files(transcript: &Transcript, room: usize) -> Option<String> {
    let mut files: Vec<(&str, Vec<usize>)> = Vec::new();
    for turn in &transcript.turns {
        for part in &turn.parts {
            let Part::Call(call) = part else { continue };
            let (Some(file), false) = (&call.file, call.failed()) else {
                continue;
            };
            match files.iter_mut().find(|(path, _)| *path == file.as_str()) {
                Some((_, turns)) if turns.last() == Some(&turn.number) => {}
                Some((_, turns)) => turns.push(turn.number),
                None => files.push((file, vec![turn.number])),
            }
        }
    }
    if files.is_empty() {
        return None;
    }
    files.sort_by_key(|(_, turns)| std::cmp::Reverse(turns.last().copied()));
    let lines: Vec<String> = files
        .iter()
        .map(|(path, turns)| format!("- {path} ({})", turn_list(turns)))
        .collect();
    Some(fitted("## Files written or edited", &lines, room, |left| {
        format!("{left} more, touched earlier, are left out.")
    }))
}

/// "turn 3", "turns 3, 5 and 12", or "18 turns, the last 38, 40 and 41".
fn turn_list(turns: &[usize]) -> String {
    let and = |numbers: &[usize]| {
        let texts: Vec<String> = numbers.iter().map(usize::to_string).collect();
        match texts.split_last() {
            Some((last, [])) => last.clone(),
            Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
            None => String::new(),
        }
    };
    match turns.len() {
        1 => format!("turn {}", turns[0]),
        2..=5 => format!("turns {}", and(turns)),
        count => format!("{count} turns, the last {}", and(&turns[count - 3..])),
    }
}

/// The workers, one line each, within `room` tokens.
fn workers(workers: &[Worker], room: usize) -> Option<String> {
    if workers.is_empty() {
        return None;
    }
    let lines: Vec<String> = workers
        .iter()
        .map(|worker| {
            let task = crate::ui::cut(worker.task.lines().next().unwrap_or_default(), 160);
            format!("- {}, {}. {task}", worker.handle, worker.state)
        })
        .collect();
    Some(fitted("## Workers", &lines, room, |left| {
        format!("{left} more are left out.")
    }))
}

/// What is open, within `room` tokens, or nothing when nothing is.
fn open(open: &Open, room: usize) -> Option<String> {
    let mut groups = Vec::new();
    let list = |lead: &str, texts: &[String]| {
        let lines: Vec<String> = texts
            .iter()
            .map(|text| format!("- {}", crate::ui::cut(text, 300)))
            .collect();
        format!("{lead}\n{}", lines.join("\n"))
    };
    if !open.questions.is_empty() {
        groups.push(list(
            "Questions the agent asked that wait for the user's answer",
            &open.questions,
        ));
    }
    if !open.unread.is_empty() {
        groups.push(list(
            "What the user typed during its last turn, which it never read",
            &open.unread,
        ));
    }
    if let Some(ended) = open.ended.as_deref().filter(|ended| !ended.is_empty()) {
        groups.push(format!("The last turn was {ended}."));
    }
    if groups.is_empty() {
        return None;
    }
    let mut section = format!("## Open\n\n{}", groups.join("\n\n"));
    if tokens(&section) > room {
        let (shown, _) = cut_bytes(&section, (room * 4).saturating_sub(40));
        section = format!("{shown}\n(The rest is cut.)");
    }
    Some(section)
}

/// A section of `lines` under `heading` within `room` tokens, leaving out
/// the last lines that don't fit and saying how many with `left_out`.
fn fitted(
    heading: &str,
    lines: &[String],
    room: usize,
    left_out: impl Fn(usize) -> String,
) -> String {
    let mut kept = lines.len();
    loop {
        let mut section = format!("{heading}\n\n{}", lines[..kept].join("\n"));
        if kept < lines.len() {
            section.push('\n');
            section.push_str(&left_out(lines.len() - kept));
        }
        if tokens(&section) <= room || kept <= 1 {
            return section;
        }
        kept -= 1;
    }
}

/// The last exchanges in full text, the user's words and the agent's
/// reply, the newest that fit in `room` tokens, oldest first. The oldest of
/// them may be cut.
fn exchanges(transcript: &Transcript, room: usize) -> Option<String> {
    const HEADING: &str = "## Last exchanges";
    let mut room = room.saturating_sub(tokens(HEADING) + 1);
    let mut shown = Vec::new();
    for turn in transcript.turns.iter().rev() {
        let Some((user, agent)) = turn.exchange() else {
            continue;
        };
        let block = exchange_block(turn.number, &user, &agent);
        if tokens(&block) < room {
            room -= tokens(&block) + 1;
            shown.push(block);
            continue;
        }
        // Too long for what is left: cut, if enough is left to be of use.
        if room >= 150 {
            let chars = room * 4 - 160;
            let (user, user_cut) =
                cut_bytes(&user, (chars / 3).max(chars.saturating_sub(agent.len())));
            let (agent, agent_cut) = cut_bytes(&agent, chars - user.len());
            let mark = |cut: bool| if cut { " (cut)" } else { "" };
            shown.push(exchange_block(
                turn.number,
                &format!("{user}{}", mark(user_cut)),
                &format!("{agent}{}", mark(agent_cut)),
            ));
        }
        break;
    }
    if shown.is_empty() {
        return None;
    }
    shown.reverse();
    Some(format!("{HEADING}\n\n{}", shown.join("\n\n")))
}

fn exchange_block(turn: usize, user: &str, agent: &str) -> String {
    format!("### Turn {turn}\n\n{user}\n\n{agent}")
}

// ----- the conversation as both read it -----

/// A conversation's turns, across its runs, in order.
struct Transcript {
    turns: Vec<Turn>,
}

struct Turn {
    /// From 1, across the conversation's runs.
    number: usize,
    /// When it began, if that's known (its run's start).
    at: Option<DateTime<Utc>>,
    /// The user's words in it (their message, what they typed during it,
    /// their answers), as the transcript shows them.
    words: Vec<String>,
    /// For a turn Arko started for news, the transcript's line for it.
    notice: Option<String>,
    /// The agent's reply, which ended it.
    reply: Option<String>,
    /// The agent's last text, for a turn without a reply.
    last_text: Option<String>,
    parts: Vec<Part>,
}

enum Part {
    /// What a handover replaced before this turn, by its report.
    Report {
        at: DateTime<Utc>,
        messages: usize,
        tokens: usize,
        text: String,
    },
    /// A message from the user's side.
    User(String),
    /// A context update (the workers' news, notes, the runtime's notes).
    Update(String),
    /// The agent's text.
    Agent(String),
    /// The agent's `complete_task` call, its reply.
    Reply(String),
    Call(Call),
}

struct Call {
    name: String,
    /// As JSON.
    arguments: String,
    result: Option<CallResult>,
    /// The file it writes or edits, for one of the file tools.
    file: Option<String>,
}

struct CallResult {
    text: String,
    error: bool,
}

impl Call {
    fn failed(&self) -> bool {
        self.result.as_ref().is_some_and(|result| result.error)
    }

    /// Its whole text for `result`, with its parameters first when `read`
    /// cuts them.
    fn body(&self) -> String {
        let result = match &self.result {
            Some(result) => result.text.clone(),
            None => NO_RESULT.to_string(),
        };
        if self.arguments.chars().count() > PARAMETERS_SHOWN {
            format!("Parameters\n{}\n\nResult\n{result}", self.arguments)
        } else {
            result
        }
    }
}

/// What a call without a result shows.
const NO_RESULT: &str = "No result. The turn ended before it came back.";

impl Part {
    fn label(&self) -> String {
        match self {
            Self::Report {
                at,
                messages,
                tokens,
                ..
            } => format!(
                "Report of a handover on {}, which replaced {} (about {} tokens)",
                time(*at),
                counted(*messages, "message", "messages"),
                short(*tokens)
            ),
            Self::User(_) => "User".into(),
            Self::Update(_) => "Update".into(),
            Self::Agent(_) => "Agent".into(),
            Self::Reply(_) => "Reply".into(),
            Self::Call(call) => format!("Call {}", call.name),
        }
    }

    /// Its whole text, as `result` shows it.
    fn body(&self) -> String {
        match self {
            Self::Report { text, .. }
            | Self::User(text)
            | Self::Update(text)
            | Self::Agent(text)
            | Self::Reply(text) => text.clone(),
            Self::Call(call) => call.body(),
        }
    }

    /// The text `search` looks in.
    fn searched(&self) -> String {
        match self {
            Self::Call(call) => format!(
                "{} {}\n{}",
                call.name,
                call.arguments,
                call.result
                    .as_ref()
                    .map_or("", |result| result.text.as_str())
            ),
            other => other.body(),
        }
    }

    fn size(&self) -> usize {
        match self {
            Self::Call(call) => {
                call.name.len()
                    + call.arguments.len()
                    + call.result.as_ref().map_or(0, |result| result.text.len())
            }
            other => other.body().len(),
        }
    }

    /// How `read` shows it, under its handle, with long text cut.
    fn block(&self, handle: &str) -> String {
        let mut block = format!("{handle} {}\n", self.label());
        let cut = match self {
            Self::Call(call) => {
                let (arguments, arguments_cut) = cut_chars(&call.arguments, PARAMETERS_SHOWN);
                if !arguments.is_empty() {
                    block.push_str(arguments);
                    if arguments_cut {
                        block.push('…');
                    }
                    block.push('\n');
                }
                match &call.result {
                    Some(result) => {
                        block.push_str(if result.error { "Error\n" } else { "Result\n" });
                        let (shown, cut) = cut_chars(&result.text, PART_SHOWN);
                        block.push_str(shown);
                        arguments_cut || cut
                    }
                    None => {
                        block.push_str(NO_RESULT);
                        arguments_cut
                    }
                }
            }
            other => {
                let body = other.body();
                let (shown, cut) = cut_chars(&body, PART_SHOWN);
                block.push_str(shown);
                cut
            }
        };
        if cut {
            block.push_str(&format!(
                "\nCut. `result` {handle} shows all {} characters.",
                thousands(self.body().chars().count())
            ));
        }
        block
    }
}

impl Transcript {
    fn of(tasks: &[&arko_agents::Task]) -> Self {
        let mut turns = Vec::new();
        for task in tasks {
            let before = turns.len();
            turns.extend(run_turns(task, before));
        }
        Self { turns }
    }

    /// About how many tokens it holds.
    fn tokens(&self) -> usize {
        self.turns.iter().map(Turn::tokens).sum()
    }

    fn turn(&self, number: usize, conversation: usize) -> Result<&Turn, String> {
        number
            .checked_sub(1)
            .and_then(|index| self.turns.get(index))
            .ok_or_else(|| {
                format!(
                    "There is no turn {number}. Conversation {conversation} has {}.",
                    counted(self.turns.len(), "turn", "turns")
                )
            })
    }
}

impl Turn {
    fn new(number: usize) -> Self {
        Self {
            number,
            at: None,
            words: Vec::new(),
            notice: None,
            reply: None,
            last_text: None,
            parts: Vec::new(),
        }
    }

    fn tokens(&self) -> usize {
        self.parts.iter().map(Part::size).sum::<usize>().div_ceil(4)
    }

    fn calls(&self) -> usize {
        self.parts
            .iter()
            .filter(|part| matches!(part, Part::Call(_)))
            .count()
    }

    fn reports(&self) -> usize {
        self.parts
            .iter()
            .filter(|part| matches!(part, Part::Report { .. }))
            .count()
    }

    /// "Turn 12, 2026-10-29 14:02 UTC, about 3.4k tokens, 7 calls".
    fn summary(&self) -> String {
        let mut summary = format!("Turn {}", self.number);
        if let Some(at) = self.at {
            summary.push_str(&format!(", {}", time(at)));
        }
        summary.push_str(&format!(
            ", about {} tokens, {}",
            short(self.tokens()),
            counted(self.calls(), "call", "calls")
        ));
        summary
    }

    /// The user's side and the agent's of its exchange in full text, for a
    /// turn that has one.
    fn exchange(&self) -> Option<(String, String)> {
        let user = if !self.words.is_empty() {
            format!("User\n{}", self.words.join("\n\n"))
        } else {
            format!(
                "Arko started this turn. {}",
                sentence(self.notice.as_deref()?)
            )
        };
        let agent = match (&self.reply, &self.last_text) {
            (Some(reply), _) => format!("Reply\n{reply}"),
            (None, Some(text)) => format!("Agent, without a reply\n{text}"),
            (None, None) => "No reply.".to_string(),
        };
        Some((user, agent))
    }
}

/// The turns of `task`'s conversation, numbered after the `before` turns
/// of the runs before it. The handovers' reports go first in its first turn.
/// A conversation that was handed over begins again with the run's first
/// prompt and the latest report, which the reports stand for.
fn run_turns(task: &arko_agents::Task, before: usize) -> Vec<Turn> {
    let messages = &task.conversation.messages;
    let text_replies = task.agent_config.tool_config.text_reply_ends_task;
    let numbers = remote::message_turns(messages, text_replies);
    let mut count = numbers.iter().flatten().max().copied().unwrap_or(0) as usize;
    if count == 0 && !task.handovers.is_empty() {
        count = 1;
    }
    let mut turns: Vec<Turn> = (1..=count)
        .map(|number| Turn::new(before + number))
        .collect();
    let Some(first) = turns.first_mut() else {
        return turns;
    };
    first.at = Some(task.start_time);
    first
        .parts
        .extend(task.handovers.iter().map(|handover| Part::Report {
            at: handover.at,
            messages: handover.replaced_messages,
            tokens: handover.input_tokens,
            text: handover.report.trim().to_string(),
        }));
    let handed_over = !task.handovers.is_empty();
    let mut restated =
        handed_over.then(|| remote::own_words(&task.initial_task_prompt).to_string());

    // The user's words, the replies and the lines for news, as the
    // transcript shows them.
    let items = remote::all_items(messages, text_replies, &crate::images::Images::default());
    for item in items {
        let Some(turn) = item
            .turn
            .and_then(|turn| turns.get_mut((turn as usize).saturating_sub(1)))
        else {
            continue;
        };
        match item.kind {
            Kind::User if item.delivery != Some(Delivery::Refresh) => {
                if turn.number == before + 1
                    && let Some(prompt) = restated.take()
                    && prompt == item.text
                {
                    continue;
                }
                turn.words.push(item.text);
            }
            Kind::Notice if turn.notice.is_none() => turn.notice = Some(item.text),
            Kind::Reply => turn.reply = Some(item.text),
            Kind::Text => turn.last_text = Some(item.text),
            _ => {}
        }
    }

    // The parts, from the messages.
    let cwd = &task.current_working_directory;
    // Where each call waiting for its result is, by its ID.
    let mut waiting: HashMap<&str, (usize, usize)> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        let at = numbers[index]
            .map_or(0, |turn| turn as usize - 1)
            .min(count - 1);
        match message.role {
            Role::System => {}
            Role::Assistant => {
                let parts = &mut turns[at].parts;
                for text in message.content.iter().filter_map(Content::as_text) {
                    if !text.trim().is_empty() {
                        parts.push(Part::Agent(text.trim().to_string()));
                    }
                }
                for call in &message.tool_calls {
                    if call.name == "complete_task" {
                        let summary = call.arguments.get("summary").and_then(Value::as_str);
                        parts.push(Part::Reply(summary.unwrap_or_default().trim().to_string()));
                        continue;
                    }
                    if let Some(id) = &call.id {
                        waiting.insert(id, (at, parts.len()));
                    }
                    let arguments = match (&call.arguments, &call.invalid_arguments) {
                        (Value::Null, Some(text)) => text.clone(),
                        (arguments, _) => arguments.to_string(),
                    };
                    parts.push(Part::Call(Call {
                        name: call.name.clone(),
                        arguments,
                        result: None,
                        file: edited(&call.name, &call.arguments, cwd),
                    }));
                }
            }
            Role::User => {
                for content in &message.content {
                    match content {
                        Content::ToolResult(result) => {
                            let found = result.call_id.as_deref().and_then(|id| waiting.remove(id));
                            let outcome = CallResult {
                                text: result.output.clone(),
                                error: result.is_error,
                            };
                            match found {
                                Some((turn, part)) => {
                                    if let Part::Call(call) = &mut turns[turn].parts[part] {
                                        call.result = Some(outcome);
                                    }
                                }
                                None => turns[at].parts.push(Part::Call(Call {
                                    name: result.name.clone(),
                                    arguments: String::new(),
                                    result: Some(outcome),
                                    file: None,
                                })),
                            }
                        }
                        Content::Text(text) => {
                            if index == 0 && handed_over {
                                continue;
                            }
                            if let Some(part) =
                                user_part(text, task.runtime_messages.contains(&index))
                            {
                                turns[at].parts.push(part);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    turns
}

/// The part of a text from the user's side: a context update, a note the
/// runtime or Arko added, or the user's message. `None` when nothing is
/// left once the runtime's sections are gone.
fn user_part(text: &str, runtime: bool) -> Option<Part> {
    let text = without_runtime_sections(text);
    if text.is_empty() {
        return None;
    }
    if let Some(update) = text.strip_prefix("# Workflow context update") {
        return Some(Part::Update(update.trim().to_string()));
    }
    if runtime || crate::openings::Opening::of(text).is_some() {
        return Some(Part::Update(text.to_string()));
    }
    let words = words_of(text);
    Some(if words.is_empty() {
        Part::Update(text.to_string())
    } else {
        Part::User(words)
    })
}

/// `text` without the environment details and reminder the runtime adds
/// after a message.
fn without_runtime_sections(text: &str) -> &str {
    let end = [
        "\n\n# Current Environment Information",
        "\n\n# Initial Environment Information",
        "\n\n# Reminder\n",
    ]
    .iter()
    .filter_map(|heading| text.find(heading))
    .min()
    .unwrap_or(text.len());
    text[..end].trim()
}

/// The user's words in a message Arko composed, after what they typed
/// during the turn before when it holds that.
fn words_of(text: &str) -> String {
    let words = remote::own_words(text);
    let unread = text
        .find("# What I typed during your last turn")
        .map(|start| {
            let rest = &text[start..];
            rest[..rest.find("\n\n# ").unwrap_or(rest.len())].trim()
        });
    match unread {
        Some(unread) if !words.contains(unread) => format!("{unread}\n\n{words}"),
        _ => words.to_string(),
    }
}

/// The file a call of a file tool writes or edits (Arko's `edit_file` and
/// `write_file`, and Claude Code's), relative to `cwd` when it's inside.
fn edited(tool: &str, arguments: &Value, cwd: &Path) -> Option<String> {
    let key = match tool {
        "edit_file" | "write_file" => "path",
        "Edit" | "Write" | "MultiEdit" => "file_path",
        "NotebookEdit" => "notebook_path",
        _ => return None,
    };
    let path = arguments.get(key)?.as_str()?.trim();
    if path.is_empty() {
        return None;
    }
    Some(
        Path::new(path)
            .strip_prefix(cwd)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map_or_else(
                || path.to_string(),
                |relative| relative.display().to_string(),
            ),
    )
}

// ----- the tool -----

/// Where the tool finds the session's conversations.
pub trait Saved: Send + Sync + std::fmt::Debug {
    /// How many conversations the session has.
    fn count(&self) -> usize;
    /// The runs of conversation `number` (from 1), oldest first, or why
    /// they can't be read.
    fn runs(&self, number: usize) -> Result<Vec<arko_workflows::AgentResult>, String>;
    /// The conversation the main agent's run `run` goes on from (the one it
    /// was switched from), which the tool reads when a call names none.
    fn continued(&self, run: uuid::Uuid) -> Option<usize>;
}

/// The `earlier_conversation` tool, which reads the session's saved
/// conversations.
#[derive(Debug)]
pub struct EarlierConversation(pub Arc<dyn Saved>);

const DESCRIPTION: &str = "Reads an earlier conversation of this session, saved with its tool calls and results, which isn't in your context. Look things up in it before you redo work or ask the user something it may already answer. Its turns are numbered from 1, and each part of a turn (a message, a call with its result, an update) has a handle such as `12.3`, the third part of turn 12. `outline` lists turns with their first lines, 50 at a time. `read` shows turns in full, with long parts cut. `search` finds the turns that mention words such as a path, a name or an error. `result` shows one part in full by its handle. An output holds 8000 tokens unless `limit` raises it, and a cut one says how to go on.";

/// The tool's definition.
pub fn definition() -> ToolDefinition {
    let param = |name: &str, description: &str, param_type, required| ToolParameterDefinition {
        name: name.into(),
        description: description.into(),
        param_type,
        required,
        is_path: false,
        flag: None,
        default_value: None,
    };
    ToolDefinition {
        name: TOOL_NAME.into(),
        description: DESCRIPTION.into(),
        output_based_example: None,
        parameters: vec![
            param(
                "operation",
                "outline, read, search or result.",
                ToolParameterType::String,
                true,
            ),
            param(
                "conversation",
                "The conversation's number, as /resume lists it. By default the one this conversation goes on from.",
                ToolParameterType::Integer,
                false,
            ),
            param(
                "turn",
                "For outline, the first turn listed (default 1). For read, the first turn shown, or a handle such as 12.4 to start at that part. For result, the part's handle, such as 12.3.",
                ToolParameterType::String,
                false,
            ),
            param(
                "through",
                "For outline and read, the last turn shown. By default outline lists 50 turns and read shows one.",
                ToolParameterType::Integer,
                false,
            ),
            param(
                "words",
                "For search, the words to find, such as a path, a name or an error.",
                ToolParameterType::String,
                false,
            ),
            param(
                "offset",
                "For result, the character to start at, which a cut result names (default 0).",
                ToolParameterType::Integer,
                false,
            ),
            param(
                "limit",
                "The output's size in tokens, up to 20000 (default 8000).",
                ToolParameterType::Integer,
                false,
            ),
        ],
        parallel_safe: true,
        read_only: true,
        tool_type_details: ToolTypeDetails::DeveloperDefined,
        raw_json_schema_override: None,
    }
}

#[async_trait]
impl Tool for EarlierConversation {
    fn definition(&self) -> ToolDefinition {
        definition()
    }

    fn recovery_mode(&self) -> ToolRecovery {
        ToolRecovery::Rerun
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let request = Request::parse(&params)?;
        let saved = Arc::clone(&self.0);
        let run = context.task_id;
        // Reading a task can mean reading its file, and a long conversation
        // takes a while to go through.
        tokio::task::spawn_blocking(move || request.answer(saved.as_ref(), run))
            .await
            .map_err(|error| {
                ToolError::ExecutionFailed(format!("The conversation couldn't be read ({error})."))
            })?
            .map_err(ToolError::ExecutionFailed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Outline,
    Read,
    Search,
    Result,
}

/// A turn, or a part of one by its handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct At {
    turn: usize,
    part: Option<usize>,
}

impl At {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_matches('`');
        let (turn, part) = match text.split_once('.') {
            Some((turn, part)) => (turn, Some(part.parse().ok()?)),
            None => (text, None),
        };
        Some(Self {
            turn: turn.parse().ok()?,
            part,
        })
    }
}

#[derive(Debug, Clone)]
struct Request {
    operation: Operation,
    conversation: Option<usize>,
    turn: Option<At>,
    through: Option<usize>,
    words: Option<String>,
    offset: usize,
    /// In tokens.
    limit: usize,
}

fn invalid(name: &str, message: impl Into<String>) -> ToolError {
    ToolError::ParameterValidation {
        param_name: name.into(),
        message: message.into(),
    }
}

/// A whole number given as a number or as text.
fn number(params: &HashMap<String, Value>, name: &str) -> Result<Option<usize>, ToolError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(|number| Some(number as usize))
            .ok_or_else(|| invalid(name, "takes a whole number")),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => text
            .trim()
            .replace([',', '_'], "")
            .parse()
            .map(Some)
            .map_err(|_| invalid(name, format!("takes a whole number, not {text:?}"))),
        Some(_) => Err(invalid(name, "takes a whole number")),
    }
}

fn text(params: &HashMap<String, Value>, name: &str) -> Option<String> {
    match params.get(name)? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

impl Request {
    fn parse(params: &HashMap<String, Value>) -> Result<Self, ToolError> {
        let operation = text(params, "operation").ok_or_else(|| ToolError::MissingParameter {
            param_name: "operation".into(),
        })?;
        let operation = match operation.to_ascii_lowercase().as_str() {
            "outline" => Operation::Outline,
            "read" => Operation::Read,
            "search" => Operation::Search,
            "result" => Operation::Result,
            other => {
                return Err(invalid(
                    "operation",
                    format!("takes outline, read, search or result, not {other:?}"),
                ));
            }
        };
        let turn = match text(params, "turn") {
            None => None,
            Some(turn) => Some(At::parse(&turn).ok_or_else(|| {
                invalid(
                    "turn",
                    format!("takes a turn's number such as 12, or a part's handle such as 12.3, not {turn:?}"),
                )
            })?),
        };
        let limit = number(params, "limit")?
            .unwrap_or(OUTPUT_TOKENS)
            .clamp(MIN_OUTPUT_TOKENS, MAX_OUTPUT_TOKENS);
        Ok(Self {
            operation,
            conversation: number(params, "conversation")?,
            turn,
            through: number(params, "through")?,
            words: text(params, "words"),
            offset: number(params, "offset")?.unwrap_or(0),
            limit,
        })
    }

    /// The output, or why there is none.
    fn answer(&self, saved: &dyn Saved, run: uuid::Uuid) -> Result<String, String> {
        let count = saved.count();
        if count == 0 {
            return Err("This session has no conversation to read yet.".into());
        }
        let number = match self.conversation {
            Some(number) => number,
            None => saved.continued(run).ok_or_else(|| {
                format!(
                    "Give `conversation`, a number from 1 to {count}, since this conversation doesn't go on from an earlier one."
                )
            })?,
        };
        if number == 0 || number > count {
            return Err(format!(
                "There is no conversation {number}. This session has {count}."
            ));
        }
        let results = saved.runs(number)?;
        let tasks = results
            .iter()
            .map(|result| {
                result
                    .task()
                    .map_err(|error| format!("Conversation {number} can't be read ({error})."))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let transcript = Transcript::of(&tasks);
        let mut output = Output::new(self.limit);
        match self.operation {
            Operation::Outline => self.outline(&transcript, number, &mut output)?,
            Operation::Read => self.read(&transcript, number, &mut output)?,
            Operation::Search => self.search(&transcript, number, &mut output)?,
            Operation::Result => self.result(&transcript, number, &mut output)?,
        }
        Ok(output.text)
    }

    fn outline(
        &self,
        transcript: &Transcript,
        number: usize,
        output: &mut Output,
    ) -> Result<(), String> {
        let total = transcript.turns.len();
        if total == 0 {
            output.push(&format!("Conversation {number} has no turns."));
            return Ok(());
        }
        let first = self.turn.map_or(1, |at| at.turn);
        transcript.turn(first, number)?;
        let last = self
            .through
            .unwrap_or(total)
            .min(first + OUTLINE_PAGE - 1)
            .min(total);
        if last < first {
            return Err(format!("`through` ({last}) comes before `turn` ({first})."));
        }
        output.push(&format!(
            "Conversation {number}, turns {first} to {last} of {total}."
        ));
        for turn in &transcript.turns[first - 1..last] {
            let block = outline_block(turn);
            if !output.fits(&block) {
                output.push(&format!(
                    "The output stops at its limit here. `outline` with turn {} goes on.",
                    turn.number
                ));
                return Ok(());
            }
            output.push(&block);
        }
        if last < total {
            output.push(&format!("`outline` with turn {} goes on.", last + 1));
        }
        Ok(())
    }

    fn read(
        &self,
        transcript: &Transcript,
        number: usize,
        output: &mut Output,
    ) -> Result<(), String> {
        let at = self
            .turn
            .ok_or("`read` needs `turn`, a turn's number such as 12.")?;
        let first = transcript.turn(at.turn, number)?;
        let start = part_index(first, at)?.unwrap_or(0);
        let total = transcript.turns.len();
        let last = self.through.unwrap_or(at.turn).min(total);
        if last < at.turn {
            return Err(format!(
                "`through` ({last}) comes before `turn` ({}).",
                at.turn
            ));
        }
        output.push(&if last == at.turn {
            format!("Conversation {number}, turn {} of {total}.", at.turn)
        } else {
            format!(
                "Conversation {number}, turns {} to {last} of {total}.",
                at.turn
            )
        });
        let through = if last > at.turn {
            format!(" and through {last}")
        } else {
            String::new()
        };
        for turn in &transcript.turns[at.turn - 1..last] {
            let from = if turn.number == at.turn { start } else { 0 };
            let mut heading = turn.summary();
            if from > 0 {
                heading.push_str(&format!(", from part {}.{}", turn.number, from + 1));
            }
            if turn.parts.is_empty() {
                heading.push_str("\nIt holds nothing.");
            }
            if !output.fits(&heading) {
                output.push(&format!(
                    "The output stops at its limit here. `read` with turn {}{through} goes on.",
                    turn.number
                ));
                return Ok(());
            }
            output.push(&heading);
            for (index, part) in turn.parts.iter().enumerate().skip(from) {
                let handle = format!("{}.{}", turn.number, index + 1);
                let block = part.block(&handle);
                if !output.fits(&block) {
                    output.push(&format!(
                        "The output stops at its limit here. `read` with turn {handle}{through} goes on."
                    ));
                    return Ok(());
                }
                output.push(&block);
            }
        }
        Ok(())
    }

    fn search(
        &self,
        transcript: &Transcript,
        number: usize,
        output: &mut Output,
    ) -> Result<(), String> {
        let words = self
            .words
            .as_deref()
            .ok_or("`search` needs `words`, such as a path, a name or an error.")?;
        let mut terms: Vec<String> = Vec::new();
        for word in words.split_whitespace() {
            let term = word
                .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ',' | ';' | '(' | ')'))
                .to_ascii_lowercase();
            if !term.is_empty() && !terms.contains(&term) {
                terms.push(term);
            }
        }
        if terms.is_empty() {
            return Err("`search` needs `words`, such as a path, a name or an error.".into());
        }
        let mut hits = search_hits(transcript, &terms);
        if hits.is_empty() {
            output.push(&format!(
                "No turn of conversation {number} mentions {}.",
                quoted(&terms)
            ));
            return Ok(());
        }
        let found = hits.len();
        hits.truncate(HITS);
        output.push(&format!(
            "{} of conversation {number} {} {}, the best first{}.",
            counted(found, "turn", "turns"),
            if found == 1 { "matches" } else { "match" },
            quoted(&terms),
            if found > HITS {
                format!(", and here are the first {HITS}")
            } else {
                String::new()
            }
        ));
        for (shown, hit) in hits.iter().enumerate() {
            let turn = &transcript.turns[hit.turn];
            let part = &turn.parts[hit.part];
            let mut block = format!("{}.{} {}", turn.number, hit.part + 1, part.label());
            if hit.matched < terms.len() {
                block.push_str(&format!(", {} of {} words", hit.matched, terms.len()));
            }
            for line in snippet(&part.searched(), hit.at) {
                block.push_str("\n  ");
                block.push_str(&line);
            }
            if !output.fits(&block) {
                output.push(&format!(
                    "The output stops at its limit after {}. A higher `limit` shows more.",
                    counted(shown, "hit", "hits")
                ));
                break;
            }
            output.push(&block);
        }
        Ok(())
    }

    fn result(
        &self,
        transcript: &Transcript,
        number: usize,
        output: &mut Output,
    ) -> Result<(), String> {
        let at = self
            .turn
            .ok_or("`result` needs `turn`, a part's handle such as 12.3.")?;
        let turn = transcript.turn(at.turn, number)?;
        let Some(index) = part_index(turn, at)? else {
            return Err(format!(
                "`result` needs a part's handle such as {}.1, not a turn alone.",
                at.turn
            ));
        };
        let part = &turn.parts[index];
        let handle = format!("{}.{}", at.turn, index + 1);
        let body = part.body();
        let total = body.chars().count();
        if self.offset > 0 && self.offset >= total {
            return Err(format!(
                "Part {handle} has {} {}, so offset {} is past its end.",
                thousands(total),
                if total == 1 {
                    "character"
                } else {
                    "characters"
                },
                self.offset
            ));
        }
        // A page within the output's cap, in bytes too.
        let room = output.cap.saturating_sub(400);
        let mut text = String::new();
        for c in body.chars().skip(self.offset).take(RESULT_PAGE) {
            if text.len() + c.len_utf8() > room {
                break;
            }
            text.push(c);
        }
        let end = self.offset + text.chars().count();
        output.push(&if self.offset == 0 && end == total {
            format!(
                "{handle} {}, {} characters.",
                part.label(),
                thousands(total)
            )
        } else {
            format!(
                "{handle} {}, characters {} to {} of {}.",
                part.label(),
                thousands(self.offset + 1),
                thousands(end),
                thousands(total)
            )
        });
        output.push(&text);
        if end < total {
            output.push(&format!("`result` {handle} with offset {end} goes on."));
        }
        Ok(())
    }
}

/// The part `at` names in `turn` (`None` for a turn alone).
fn part_index(turn: &Turn, at: At) -> Result<Option<usize>, String> {
    let Some(part) = at.part else {
        return Ok(None);
    };
    if part == 0 || part > turn.parts.len() {
        return Err(format!(
            "There is no part {}.{part}. Turn {} has {}.",
            turn.number,
            turn.number,
            counted(turn.parts.len(), "part", "parts")
        ));
    }
    Ok(Some(part - 1))
}

/// How `outline` lists a turn: its summary, and the first lines of the
/// user's message and of the reply.
fn outline_block(turn: &Turn) -> String {
    let mut block = turn.summary();
    match turn.reports() {
        0 => {}
        1 => block.push_str(&format!(
            "\n  After a handover. Its report is {}.1.",
            turn.number
        )),
        reports => block.push_str(&format!(
            "\n  After {reports} handovers. Their reports are {}.1 to {}.{reports}.",
            turn.number, turn.number
        )),
    }
    let line = |text: &str| {
        crate::ui::cut(
            text.lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default(),
            LINE,
        )
    };
    match (turn.words.first(), &turn.notice) {
        (Some(words), _) => {
            block.push_str(&format!("\n  User  {}", line(words)));
            if turn.words.len() > 1 {
                block.push_str(&format!(" (and {} more)", turn.words.len() - 1));
            }
        }
        (None, Some(notice)) => block.push_str(&format!("\n  Arko  {}", line(notice))),
        (None, None) => {}
    }
    match (&turn.reply, &turn.last_text) {
        (Some(reply), _) => block.push_str(&format!("\n  Reply  {}", line(reply))),
        (None, Some(text)) => block.push_str(&format!("\n  Agent  {}", line(text))),
        (None, None) => block.push_str("\n  No reply"),
    }
    block
}

/// A turn `search` found: the part that matched best, where, and how well
/// the turn matched.
struct Hit {
    /// Indices in the transcript and in the turn's parts.
    turn: usize,
    part: usize,
    /// Where the snippet goes in the part's searched text.
    at: usize,
    /// How many of the words the turn holds.
    matched: usize,
    /// The turn holds the words as they were given, in that order.
    phrase: bool,
    /// How often they come in it, each counted up to 3 times in a part, so
    /// a long output that repeats one doesn't outweigh the rest, and twice
    /// outside tool calls.
    score: usize,
}

/// The turns that hold any of `terms` (lowercase), the best first. Those
/// with the most of them come first, then those that hold them as a phrase,
/// then those with the most matches, then the newest.
fn search_hits(transcript: &Transcript, terms: &[String]) -> Vec<Hit> {
    let phrase = (terms.len() > 1).then(|| terms.join(" "));
    let mut hits = Vec::new();
    for (index, turn) in transcript.turns.iter().enumerate() {
        let mut matched = vec![false; terms.len()];
        let (mut score, mut phrased) = (0, false);
        // The best part: how well it matched, which it is, and where.
        let mut best: Option<((usize, bool, usize), usize, usize)> = None;
        for (part_index, part) in turn.parts.iter().enumerate() {
            // ASCII lowercase keeps every byte where it was.
            let lower = part.searched().to_ascii_lowercase();
            let (mut distinct, mut matches) = (0, 0);
            for (term_index, term) in terms.iter().enumerate() {
                let count = lower.match_indices(term.as_str()).take(3).count();
                if count > 0 {
                    distinct += 1;
                    matches += count;
                    matched[term_index] = true;
                }
            }
            if distinct == 0 {
                continue;
            }
            // What the user and the agent wrote counts twice as much as
            // what tools returned, which repeats words more.
            if !matches!(part, Part::Call(_)) {
                matches *= 2;
            }
            let found = phrase.as_deref().and_then(|phrase| lower.find(phrase));
            phrased |= found.is_some();
            score += matches;
            let rank = (distinct, found.is_some(), matches);
            if best.as_ref().is_none_or(|(best, ..)| rank > *best) {
                let at = found.unwrap_or_else(|| best_line(&lower, terms));
                best = Some((rank, part_index, at));
            }
        }
        if let Some((_, part, at)) = best {
            hits.push(Hit {
                turn: index,
                part,
                at,
                matched: matched.iter().filter(|matched| **matched).count(),
                phrase: phrased,
                score,
            });
        }
    }
    hits.sort_by(|a, b| {
        (b.matched, b.phrase, b.score, b.turn).cmp(&(a.matched, a.phrase, a.score, a.turn))
    });
    hits
}

/// Where the first of `terms` is in the line of `lower` that holds the
/// most of them, closest together.
fn best_line(lower: &str, terms: &[String]) -> usize {
    // The most terms, the least span, and where.
    let mut best = (0, usize::MAX, 0);
    let mut start = 0;
    for line in lower.split('\n') {
        let found: Vec<usize> = terms
            .iter()
            .filter_map(|term| line.find(term.as_str()))
            .collect();
        if let (Some(first), Some(last)) = (found.iter().min(), found.iter().max()) {
            let span = last - first;
            if found.len() > best.0 || (found.len() == best.0 && span < best.1) {
                best = (found.len(), span, start + first);
            }
        }
        start += line.len() + 1;
    }
    best.2
}

/// The three lines of `text` around byte `at`: its line, and the ones
/// before and after it, each cut to about 200 characters around `at`.
fn snippet(text: &str, at: usize) -> Vec<String> {
    let start = text[..at].rfind('\n').map_or(0, |newline| newline + 1);
    let end = text[at..]
        .find('\n')
        .map_or(text.len(), |newline| at + newline);
    let mut lines = Vec::new();
    if start > 0 {
        let before = &text[..start - 1];
        let line = &before[before.rfind('\n').map_or(0, |newline| newline + 1)..];
        lines.push(crate::ui::cut(line, 160));
    }
    lines.push(around(&text[start..end], at - start, 200));
    if end < text.len() {
        let after = &text[end + 1..];
        lines.push(crate::ui::cut(
            &after[..after.find('\n').unwrap_or(after.len())],
            160,
        ));
    }
    lines.into_iter().filter(|line| !line.is_empty()).collect()
}

/// About `width` characters of `line` around byte `at`.
fn around(line: &str, at: usize, width: usize) -> String {
    if line.chars().count() <= width {
        return crate::ui::cut(line, width);
    }
    let mut start = at.saturating_sub(width / 3);
    while !line.is_char_boundary(start) {
        start -= 1;
    }
    let shown: String = line[start..].chars().take(width).collect();
    let mut text = String::new();
    if start > 0 {
        text.push('…');
    }
    text.push_str(&crate::ui::cut(&shown, width));
    if start + shown.len() < line.len() && !text.ends_with('…') {
        text.push('…');
    }
    text
}

/// `terms` as the output names them: “parse_args” and “config”.
fn quoted(terms: &[String]) -> String {
    let quoted: Vec<String> = terms.iter().map(|term| format!("“{term}”")).collect();
    match quoted.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// A tool output within its cap, its parts a blank line apart.
struct Output {
    text: String,
    /// In bytes.
    cap: usize,
}

impl Output {
    /// The room kept for the line that says how to go on.
    const RESERVE: usize = 300;

    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            cap: limit * 4,
        }
    }

    fn fits(&self, more: &str) -> bool {
        self.text.len() + 2 + more.len() + Self::RESERVE <= self.cap
    }

    fn push(&mut self, more: &str) {
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(more);
    }
}

// ----- text -----

/// The first `chars` characters of `text`, and whether that cuts it.
fn cut_chars(text: &str, chars: usize) -> (&str, bool) {
    match text.char_indices().nth(chars) {
        Some((end, _)) => (&text[..end], true),
        None => (text, false),
    }
}

/// At most `bytes` bytes of `text`, cut at a character's end, and whether
/// that cuts it.
fn cut_bytes(text: &str, bytes: usize) -> (&str, bool) {
    if text.len() <= bytes {
        return (text, false);
    }
    let mut end = bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// `text` as a sentence, with a full stop at its end.
fn sentence(text: &str) -> String {
    let text = text.trim();
    if text.ends_with(['.', '!', '?']) {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

/// "1 turn", "212 turns".
fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// A token count, short: "820", "8.2k", "142k", "1.2M".
fn short(count: usize) -> String {
    let value = count as f64;
    let trimmed = |value: f64| {
        let text = format!("{value:.1}");
        text.strip_suffix(".0").map_or(text.clone(), str::to_string)
    };
    if count < 1_000 {
        count.to_string()
    } else if count < 9_950 {
        format!("{}k", trimmed(value / 1_000.))
    } else if count < 999_500 {
        format!("{}k", (value / 1_000.).round())
    } else {
        format!("{}M", trimmed(value / 1_000_000.))
    }
}

/// `5312` as `5,312`.
fn thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut text = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            text.push(',');
        }
        text.push(digit);
    }
    text
}

/// When, as the outputs say it: "2026-10-29 14:02 UTC".
fn time(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

#[cfg(test)]
mod tests;

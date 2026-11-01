//! The Quiet view's rows, which say each call in a few words and merge
//! neighbouring calls of a kind into one row ("Made 3 edits to app.rs",
//! "Read 4 files in src/tui"), the same in every client.
//!
//! A run is the stretch of an agent's calls and thinking between two other
//! things the view shows. Within a run a call joins the row before it when
//! it is of the same kind, or reads a file the row edits or writes. A row
//! holds calls that follow each other, so rows never reorder, and the
//! thinking between two calls of one row goes inside it. A call joins as it
//! comes, so a long call (a command, a wait, an MCP call) shows on the row
//! while it runs ("Ran 4 commands · running cargo build") and no row comes
//! and goes with it. A call that failed, a test run and a call of another
//! kind have rows of their own. The images a call's tool returned and the
//! buttons of its job show under its row's line, in whatever row it is. A
//! row keeps its calls once anything shows after it ([`rows`]), so only the
//! last row changes as calls join it.
//!
//! The web page compiles this file beside `protocol.rs` as a module of its
//! own, so it depends on the standard library and that module alone.

use std::collections::HashMap;

use super::protocol::{Act, ActKind, CallStatus, Came};

/// A call's outcome when a stop cancelled it.
pub const STOPPED: &str = "stopped";
/// A call's outcome when Claude Code was about to compact the conversation, so
/// the agent makes the call again after.
pub const HELD: &str = "held for the compaction";
/// A call's outcome when the session stopped before its result came.
pub const INTERRUPTED: &str = "interrupted";

/// The longest command a row names, in characters.
pub const COMMAND_CHARS: usize = 48;

/// An item of a transcript, as the Quiet view reads it, in the order shown.
#[derive(Debug, Clone, Copy)]
pub enum Step<'a> {
    Call(Call<'a>),
    /// The agent's thinking, which doesn't end a run.
    Thinking,
    /// Anything else the view shows, by its item's ID, which ends a run.
    Other(u64),
    /// What shows beside the agent's work without ending it (a note the
    /// agent hasn't read, which waits at the end).
    Aside,
}

/// A call, as its row needs it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Call<'a> {
    /// The item's ID, by which a sealed run keeps its rows.
    pub id: u64,
    /// What it did. A call without one shows as it does in Full.
    pub act: Option<&'a Act>,
    pub status: Option<CallStatus>,
    pub outcome: Option<&'a str>,
    /// It made a job (it moved to the background, or started a terminal or
    /// a monitor), whose state its row says.
    pub job: bool,
    pub turn: Option<u64>,
}

/// What the view shows for its steps, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    /// The step at this index, as Full shows it.
    Step(usize),
    Row(Row),
}

/// A row of calls.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The steps it holds, by index, in order. They are its calls and the
    /// thinking between them.
    pub members: Vec<usize>,
    pub words: Words,
}

/// What a row says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Words {
    pub text: String,
    /// Why it failed, said after a ✗ in the error colour.
    pub failed: Option<String>,
    /// More, said after it, muted, such as how a call's job is going.
    pub note: Option<String>,
    /// A call a stop or a compaction cut short, whose row is muted.
    pub muted: bool,
    /// A call of it runs.
    pub running: bool,
}

/// The rows of `steps`, with the steps that show as they are. They follow
/// from the items alone, each call's place and the point its result came
/// at ([`Act::ended`]), so every client, a reload and a restored session
/// show the same rows. A row keeps its calls once another row or anything
/// else shows after it. A call that fails, or goes on in the background,
/// while its row is the last has a row of its own, and one that does later
/// stays in its row, which says so.
pub fn rows(steps: &[Step]) -> Vec<Piece> {
    // The ID of what shows after each step, among calls and other items.
    let mut next: Vec<Option<u64>> = vec![None; steps.len()];
    let mut after: Option<u64> = None;
    for (index, step) in steps.iter().enumerate().rev() {
        next[index] = after;
        match step {
            Step::Call(call) => after = Some(call.id),
            Step::Other(id) => after = Some(*id),
            Step::Thinking | Step::Aside => {}
        }
    }
    let mut out = Vec::new();
    let mut names = Names::default();
    let mut start = 0;
    while start < steps.len() {
        let end = steps[start..]
            .iter()
            .position(|step| matches!(step, Step::Other(_)))
            .map_or(steps.len(), |at| start + at);
        let slots = slots(steps, start, end, &next, &mut names);
        let groups = plan(slots);
        place(steps, start, end, &groups, &mut out);
        if end < steps.len() {
            out.push(Piece::Step(end));
        }
        start = end + 1;
    }
    out
}

/// What a call does while it runs, in a few words, as its row says it
/// ("Running cargo test -p arko", "Waiting for r91").
pub fn doing(act: &Act) -> String {
    let member = Member {
        index: 0,
        call: Call {
            act: Some(act),
            ..Call::default()
        },
        act,
        name: Names::default().of(None, act),
        next: None,
    };
    phrases(&member).doing
}

/// A call of a row.
#[derive(Debug, Clone)]
struct Member<'a> {
    index: usize,
    call: Call<'a>,
    act: &'a Act,
    /// Its target as the row names it (a file by its name).
    name: String,
    /// The ID of the call or item that shows after it.
    next: Option<u64>,
}

impl Member<'_> {
    fn running(&self) -> bool {
        self.call.status == Some(CallStatus::Running)
    }

    fn failed(&self) -> bool {
        self.call.status == Some(CallStatus::Failed)
    }

    /// A call that can run long, which a row shows as it runs ("· running
    /// cargo build").
    fn long(&self) -> bool {
        matches!(
            self.act.kind,
            ActKind::Command | ActKind::Wait | ActKind::Mcp | ActKind::Check
        )
    }

    /// Whether it has a row of its own as it comes (a call of another kind,
    /// a test run). With `settled`, also for what its result decided, a
    /// failure or a job that went on in the background.
    fn alone(&self, settled: bool) -> bool {
        self.act.kind == ActKind::Other || self.act.alone || (settled && self.settled_alone())
    }

    /// Whether its result made it stand alone.
    fn settled_alone(&self) -> bool {
        self.failed() || self.call.job
    }

    /// Whether its result made it stand alone while its row was the last,
    /// which `freeze` (what shows after the row) ended.
    fn alone_early(&self, freeze: Option<u64>) -> bool {
        self.settled_alone()
            && match (self.act.ended, freeze) {
                (Some(ended), Some(freeze)) => ended <= freeze,
                _ => true,
            }
    }
}

/// A run's calls in order, each with its act, or `None` for a call without
/// one, which shows as it is.
fn slots<'a>(
    steps: &[Step<'a>],
    start: usize,
    end: usize,
    next: &[Option<u64>],
    names: &mut Names,
) -> Vec<Option<Member<'a>>> {
    let mut slots = Vec::new();
    for (index, step) in steps.iter().enumerate().take(end).skip(start) {
        let Step::Call(call) = *step else {
            continue;
        };
        slots.push(call.act.map(|act| Member {
            index,
            call,
            act,
            name: names.of(call.turn, act),
            next: next[index],
        }));
    }
    slots
}

/// A row being made.
struct Group<'a> {
    kind: ActKind,
    /// The calls it counts.
    calls: Vec<Member<'a>>,
    /// The reads of a file it changes, which it holds without counting.
    absorbed: Vec<Member<'a>>,
    /// It takes no more calls.
    alone: bool,
}

impl<'a> Group<'a> {
    fn new(member: Member<'a>, settled: bool) -> Self {
        Self {
            kind: member.act.kind,
            alone: member.alone(settled),
            calls: vec![member],
            absorbed: Vec::new(),
        }
    }

    /// What shows after it.
    fn freeze(&self) -> Option<u64> {
        self.calls
            .iter()
            .chain(&self.absorbed)
            .max_by_key(|member| member.index)
            .and_then(|member| member.next)
    }

    /// Its calls by step index, in order.
    fn members(&self) -> Vec<usize> {
        let mut members: Vec<usize> = self
            .calls
            .iter()
            .chain(&self.absorbed)
            .map(|member| member.index)
            .collect();
        members.sort_unstable();
        members
    }

    /// The files it changes or reads.
    fn files(&self) -> impl Iterator<Item = &str> {
        self.calls.iter().map(|call| call.act.target.as_str())
    }

    /// Takes `member` if it belongs in this row. A call joins as it comes,
    /// a long one too, so no row shows only while a call runs.
    fn take(&mut self, member: Member<'a>, settled: bool) -> Result<(), Member<'a>> {
        if self.alone || member.alone(settled) {
            return Err(member);
        }
        let kind = member.act.kind;
        let target = member.act.target.as_str();
        let changes = matches!(self.kind, ActKind::Edit | ActKind::Write);
        if kind == self.kind {
            // A job's checks and an MCP tool's calls merge one target only.
            let one_target = matches!(kind, ActKind::Check | ActKind::Mcp);
            if one_target && self.calls[0].act.target != target {
                return Err(member);
            }
            self.calls.push(member);
            return Ok(());
        }
        // Reading back a file it changes is part of changing it.
        if changes && kind == ActKind::Read && self.files().any(|file| file == target) {
            self.absorbed.push(member);
            return Ok(());
        }
        // A row that only read one file becomes a row that changes it.
        let changing = matches!(kind, ActKind::Edit | ActKind::Write);
        if self.kind == ActKind::Read && changing && self.files().all(|file| file == target) {
            self.kind = kind;
            self.absorbed.append(&mut self.calls);
            self.calls.push(member);
            return Ok(());
        }
        Err(member)
    }

    /// Takes the calls of `next`, a row of the same kind after it.
    fn absorb(&mut self, mut next: Group<'a>) {
        self.calls.append(&mut next.calls);
        self.absorbed.append(&mut next.absorbed);
    }
}

/// The rows of a run's calls `slots`. A row whose call failed, or went on
/// in the background, while the row was the last is worked out again with
/// that call on its own.
fn plan(slots: Vec<Option<Member<'_>>>) -> Vec<Group<'_>> {
    let mut groups = Vec::new();
    for group in build(slots, false) {
        let freeze = group.freeze();
        let early = group
            .calls
            .iter()
            .chain(&group.absorbed)
            .any(|member| member.alone_early(freeze));
        if early && group.calls.len() + group.absorbed.len() > 1 {
            let mut members: Vec<Member> = group.calls.into_iter().chain(group.absorbed).collect();
            members.sort_by_key(|member| member.index);
            groups.extend(build(members.into_iter().map(Some).collect(), true));
        } else {
            groups.push(group);
        }
    }
    groups
}

/// The rows of a run's calls `slots`, each call joining the row before it
/// when it belongs there. `settled` keeps a call that failed or went on in
/// the background on its own.
fn build(slots: Vec<Option<Member<'_>>>, settled: bool) -> Vec<Group<'_>> {
    let mut groups: Vec<Group> = Vec::new();
    for slot in slots {
        let Some(member) = slot else {
            // A call without an act ends the row before it.
            if let Some(last) = groups.last_mut() {
                last.alone = true;
            }
            continue;
        };
        let member = match groups.last_mut() {
            Some(last) => match last.take(member, settled) {
                Ok(()) => {
                    // A read that became an edit may follow an edit row.
                    let at = groups.len() - 1;
                    if at > 0
                        && !groups[at - 1].alone
                        && groups[at - 1].kind == groups[at].kind
                        && matches!(groups[at].kind, ActKind::Edit | ActKind::Write)
                    {
                        let last = groups.pop().expect("a row");
                        groups[at - 1].absorb(last);
                    }
                    continue;
                }
                Err(member) => member,
            },
            None => member,
        };
        groups.push(Group::new(member, settled));
    }
    groups
}

/// Places a run's rows `groups` in `steps[start..end]`, onto `out`, each row
/// where its first call is, with the thinking between two of its calls, and
/// the rest as they are.
fn place(steps: &[Step], start: usize, end: usize, groups: &[Group], out: &mut Vec<Piece>) {
    // Which row each call is in.
    let mut row_of: HashMap<usize, usize> = HashMap::new();
    for (at, group) in groups.iter().enumerate() {
        for member in group.members() {
            row_of.insert(member, at);
        }
    }
    let calls: Vec<usize> = (start..end)
        .filter(|index| matches!(steps[*index], Step::Call(_)))
        .collect();
    // The row a thinking between two calls of one row goes in.
    let inside = |index: usize| {
        let before = calls.iter().rev().find(|call| **call < index)?;
        let after = calls.iter().find(|call| **call > index)?;
        let row = row_of.get(before)?;
        (row_of.get(after) == Some(row)).then_some(*row)
    };
    let mut rows: Vec<Option<Row>> = groups
        .iter()
        .map(|group| {
            Some(Row {
                members: Vec::new(),
                words: words(group),
            })
        })
        .collect();
    // Each row's first step.
    let mut first: HashMap<usize, usize> = HashMap::new();
    for (index, step) in steps.iter().enumerate().take(end).skip(start) {
        let row = match step {
            Step::Call(_) => row_of.get(&index).copied(),
            Step::Thinking => inside(index),
            Step::Other(_) | Step::Aside => None,
        };
        if let Some(row) = row
            && let Some(built) = rows[row].as_mut()
        {
            if built.members.is_empty() {
                first.insert(index, row);
            }
            built.members.push(index);
        }
    }
    for (index, step) in steps.iter().enumerate().take(end).skip(start) {
        if let Some(row) = first.get(&index) {
            if let Some(row) = rows[*row].take() {
                out.push(Piece::Row(row));
            }
            continue;
        }
        let in_row = match step {
            Step::Call(_) => row_of.contains_key(&index),
            Step::Thinking => inside(index).is_some(),
            Step::Other(_) | Step::Aside => false,
        };
        if !in_row {
            out.push(Piece::Step(index));
        }
    }
}

/// How the rows of a turn name its files, by name, and with the folder too
/// for a file whose name another file of the turn had first.
#[derive(Default)]
struct Names {
    turn: Option<u64>,
    /// The path that first had each name.
    first: HashMap<String, String>,
}

impl Names {
    fn of(&mut self, turn: Option<u64>, act: &Act) -> String {
        if !matches!(
            act.kind,
            ActKind::Read | ActKind::Write | ActKind::Edit | ActKind::List
        ) {
            return act.target.clone();
        }
        if turn != self.turn {
            self.turn = turn;
            self.first.clear();
        }
        let path = act.target.trim_end_matches('/');
        let name = file_name(path);
        if name.is_empty() || name == "." {
            return path.to_string();
        }
        let first = self
            .first
            .entry(name.to_string())
            .or_insert_with(|| path.to_string());
        if *first == path {
            name.to_string()
        } else {
            // The folder too (`remote/mod.rs`).
            last_parts(path, 2)
        }
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// The last `count` parts of `path`.
fn last_parts(path: &str, count: usize) -> String {
    let parts: Vec<&str> = path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    parts[parts.len().saturating_sub(count)..].join("/")
}

/// The folder `paths` share, by its last two parts ("src/tui").
fn shared_folder<'a>(paths: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut shared: Option<Vec<&str>> = None;
    for path in paths {
        let mut parts: Vec<&str> = path
            .split(['/', '\\'])
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        // The folder it's in.
        parts.pop();
        shared = Some(match shared {
            None => parts,
            Some(shared) => shared
                .iter()
                .zip(&parts)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    let shared = shared?;
    (!shared.is_empty()).then(|| shared[shared.len().saturating_sub(2)..].join("/"))
}

/// `names` in a sentence ("a", "a and b", "a, b and c", and past `most`
/// "a, b, c and 2 more").
fn listed(names: &[String], most: usize) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        _ if names.len() > most => {
            format!(
                "{} and {} more",
                names[..most].join(", "),
                names.len() - most
            )
        }
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `names` inside parentheses ("rg, sed, git", and past `most` "rg, sed,
/// git and 2 more").
fn parenthesized(names: &[String], most: usize) -> String {
    if names.len() > most {
        format!(
            "{} and {} more",
            names[..most].join(", "),
            names.len() - most
        )
    } else {
        names.join(", ")
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// `items` once each, in order.
fn distinct<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        if !item.is_empty() && !out.iter().any(|seen| seen == item) {
            out.push(item.to_string());
        }
    }
    out
}

/// What a row of `group` says.
fn words(group: &Group) -> Words {
    // A long call that runs shows after what the calls before it did ("Ran
    // 4 commands · running cargo build").
    let (live, done): (Vec<&Member>, Vec<&Member>) = group
        .calls
        .iter()
        .partition(|member| member.long() && member.running());
    if !live.is_empty() && !done.is_empty() {
        let before = Group {
            kind: group.kind,
            calls: done.into_iter().cloned().collect(),
            absorbed: group.absorbed.clone(),
            alone: false,
        };
        let mut words = words(&before);
        let running = match live.as_slice() {
            [one] => lower_first(&phrases(one).doing),
            many => format!("running {} more", many.len()),
        };
        words.text = format!("{} · {running}", words.text);
        words.running = true;
        return words;
    }
    let calls = &group.calls;
    let every = || calls.iter().chain(&group.absorbed);
    let running = every().any(Member::running);
    // (A sealed row keeps a call that failed after it was sealed, and says
    // so.)
    let failed = every().filter(|member| member.failed()).count();
    let failures = |words: &mut Words| {
        if failed > 0 && words.failed.is_none() {
            words.failed = Some(format!("{failed} failed"));
        }
    };
    if calls.len() == 1 {
        let mut words = one(&calls[0]);
        words.running = running;
        if !calls[0].failed() {
            failures(&mut words);
        }
        return words;
    }
    let now = calls.iter().all(Member::running);
    let tense = |present: &str, past: &str| if now { present } else { past }.to_string();
    let count = calls.len();
    // The files it names, up to two, else how many and their folder.
    let files = |noun: (&str, &str)| {
        let names = distinct(calls.iter().map(|call| call.name.as_str()));
        if names.len() <= 2 {
            listed(&names, 2)
        } else {
            let paths = distinct(calls.iter().map(|call| call.act.target.as_str()));
            let counted = plural(paths.len(), noun.0, noun.1);
            match shared_folder(paths.iter().map(String::as_str)) {
                Some(folder) => format!("{counted} in {folder}"),
                None => counted,
            }
        }
    };
    let targets = || distinct(calls.iter().map(|call| call.act.target.as_str()));
    let text = match group.kind {
        ActKind::Read => format!("{} {}", tense("Reading", "Read"), files(("file", "files"))),
        ActKind::List => format!(
            "{} {}",
            tense("Listing", "Listed"),
            files(("folder", "folders"))
        ),
        ActKind::Write => format!("{} {}", tense("Writing", "Wrote"), files(("file", "files"))),
        ActKind::Edit => format!(
            "{} {count} edits to {}",
            tense("Making", "Made"),
            files(("file", "files"))
        ),
        ActKind::Command => {
            let programs = distinct(calls.iter().map(|call| program(&call.act.target)));
            format!(
                "{} {count} commands ({})",
                tense("Running", "Ran"),
                parenthesized(&programs, 3)
            )
        }
        ActKind::Search => {
            let indexes = distinct(
                calls
                    .iter()
                    .flat_map(|call| call.act.target.split(", "))
                    .map(str::trim),
            );
            let verb = tense("Searching", "Searched");
            match indexes.as_slice() {
                [] => format!("{verb} {count} times"),
                [one] => format!("{verb} the {one} index {count} times"),
                [a, b] => format!("{verb} the {a} and {b} indexes {count} times"),
                many => format!("{verb} {} indexes {count} times", many.len()),
            }
        }
        ActKind::Skill => {
            let skills = targets();
            match skills.as_slice() {
                [one] => format!("{} the {one} skill", tense("Loading", "Loaded")),
                _ => format!(
                    "{} {} skills ({})",
                    tense("Loading", "Loaded"),
                    skills.len(),
                    parenthesized(&skills, 3)
                ),
            }
        }
        ActKind::Message => {
            let to = targets();
            match to.as_slice() {
                [one] => format!("{} {one} {count} times", tense("Messaging", "Messaged")),
                _ => format!("{} {}", tense("Messaging", "Messaged"), listed(&to, 3)),
            }
        }
        ActKind::Wait => {
            let mut runs: Vec<String> = Vec::new();
            for call in calls {
                for run in call.act.target.split(',').map(str::trim) {
                    if !run.is_empty() && !runs.iter().any(|seen| seen == run) {
                        runs.push(run.to_string());
                    }
                }
            }
            let who = match runs.len() {
                0 => "a message".to_string(),
                1..=3 => listed(&runs, 3),
                _ => "workers".to_string(),
            };
            let mut came = Came::default();
            let mut any = false;
            for call in calls {
                if let Some(theirs) = &call.act.came {
                    any = true;
                    came.finished.extend(theirs.finished.iter().cloned());
                    came.stopped.extend(theirs.stopped.iter().cloned());
                    came.messages += theirs.messages;
                    came.updates += theirs.updates;
                }
            }
            let mut text = format!("{} for {who}", tense("Waiting", "Waited"));
            if any {
                text.push_str(" · ");
                text.push_str(&came_words(&came));
            }
            text
        }
        ActKind::Check => format!(
            "{} {} {count} times",
            tense("Checking", "Checked"),
            calls[0].name
        ),
        ActKind::Note => format!("{} {count} notes", tense("Keeping", "Kept")),
        ActKind::Mcp => format!(
            "{} {} {count} times",
            tense("Calling", "Called"),
            calls[0].name
        ),
        // (A call of another kind has a row of its own.)
        ActKind::Other => calls[0].act.done.clone(),
    };
    let mut words = Words {
        text,
        running,
        ..Words::default()
    };
    failures(&mut words);
    words
}

/// Who a message is from and who it's for, as the user reads it: "r12 to
/// main", "main to r12", "r3 to r12", and "r1 to you" for one to the user.
pub fn mail_label(from: &str, to: &str) -> String {
    format!("{from} to {to}")
}

/// What came while waits waited, in a few words ("r91 finished, 3 updates").
pub fn came_words(came: &Came) -> String {
    let mut parts = Vec::new();
    let finished = distinct(came.finished.iter().map(String::as_str));
    if !finished.is_empty() {
        parts.push(format!("{} finished", listed(&finished, 3)));
    }
    let stopped = distinct(came.stopped.iter().map(String::as_str));
    if !stopped.is_empty() {
        parts.push(format!("{} stopped", listed(&stopped, 3)));
    }
    if came.messages > 0 {
        parts.push(plural(came.messages as usize, "message", "messages"));
    }
    if came.updates > 0 {
        parts.push(plural(came.updates as usize, "update", "updates"));
    }
    if parts.is_empty() {
        "nothing new".to_string()
    } else {
        parts.join(", ")
    }
}

/// The program a command runs (`cargo` of `cargo test -p arko`), after any
/// variables set before it.
fn program(command: &str) -> &str {
    let word = command
        .split_whitespace()
        .find(|word| !word.contains('='))
        .unwrap_or_default();
    // (A word cut short names no program.)
    if word.ends_with('…') {
        return "";
    }
    file_name(word)
}

/// A call's words while it runs, once it ended and after "Couldn’t".
struct Phrases {
    doing: String,
    done: String,
    verb: String,
}

fn phrases(member: &Member) -> Phrases {
    let act = member.act;
    let name = member.name.as_str();
    let three = |doing: String, done: String, verb: String| Phrases { doing, done, verb };
    match act.kind {
        ActKind::Read => three(
            format!("Reading {name}"),
            format!("Read {name}"),
            format!("read {name}"),
        ),
        ActKind::List => three(
            format!("Listing {name}"),
            format!("Listed {name}"),
            format!("list {name}"),
        ),
        ActKind::Write => three(
            format!("Writing {name}"),
            format!("Wrote {name}"),
            format!("write {name}"),
        ),
        ActKind::Edit => three(
            format!("Editing {name}"),
            format!("Made 1 edit to {name}"),
            format!("edit {name}"),
        ),
        ActKind::Command => three(
            format!("Running {name}"),
            format!("Ran {name}"),
            format!("run {name}"),
        ),
        ActKind::Search => {
            let query = format!("“{}”", act.query);
            let indexes: Vec<String> = indexes(name);
            let which = match indexes.as_slice() {
                [] => String::new(),
                [one] => format!(" the {one} index"),
                [a, b] => format!(" the {a} and {b} indexes"),
                many => format!(" {} indexes", many.len()),
            };
            three(
                format!("Searching{which} for {query}"),
                format!("Searched{which} for {query}"),
                format!("search{which} for {query}"),
            )
        }
        ActKind::Skill => three(
            format!("Loading the {name} skill"),
            format!("Loaded the {name} skill"),
            format!("load the {name} skill"),
        ),
        ActKind::Message => three(
            format!("Messaging {name}"),
            format!("Messaged {name}"),
            format!("message {name}"),
        ),
        ActKind::Wait => {
            let runs: Vec<String> = name
                .split(',')
                .map(str::trim)
                .filter(|run| !run.is_empty())
                .map(str::to_string)
                .collect();
            let who = match runs.len() {
                0 => "a message".to_string(),
                1..=3 => listed(&runs, 3),
                _ => "workers".to_string(),
            };
            three(
                format!("Waiting for {who}"),
                format!("Waited for {who}"),
                format!("wait for {who}"),
            )
        }
        ActKind::Check => three(
            format!("Checking {name}"),
            format!("Checked {name}"),
            format!("check {name}"),
        ),
        ActKind::Note => {
            let to = if name.is_empty() {
                String::new()
            } else {
                format!(" for {name}")
            };
            three(
                format!("Keeping a note{to}"),
                format!("Kept a note{to}"),
                format!("keep a note{to}"),
            )
        }
        ActKind::Mcp => three(
            format!("Calling {name}"),
            format!("Called {name}"),
            format!("call {name}"),
        ),
        ActKind::Other => {
            let doing = act.doing.clone();
            let done = if act.done.is_empty() {
                doing.clone()
            } else {
                act.done.clone()
            };
            let verb = if act.verb.is_empty() {
                lower_first(&doing)
            } else {
                act.verb.clone()
            };
            three(doing, done, verb)
        }
    }
}

/// The indexes a search's target names ("arko, gpui").
fn indexes(target: &str) -> Vec<String> {
    distinct(target.split(", ").map(str::trim))
}

fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// What the row of one call says.
fn one(member: &Member) -> Words {
    let act = member.act;
    let Phrases { doing, done, verb } = phrases(member);
    let outcome = member
        .call
        .outcome
        .map(str::trim)
        .filter(|outcome| !outcome.is_empty());
    let news = (!act.news.is_empty()).then(|| act.news.clone());
    match member.call.status {
        Some(CallStatus::Failed) => match outcome {
            Some(STOPPED) => Words {
                text: format!("Stopped {}", lower_first(&doing)),
                muted: true,
                ..Words::default()
            },
            Some(outcome @ (HELD | INTERRUPTED)) => Words {
                text: doing,
                note: Some(outcome.to_string()),
                muted: true,
                ..Words::default()
            },
            // It ran, and how it went is the news.
            _ if act.ran || member.call.job => Words {
                text: done,
                failed: news.or(outcome.map(str::to_string)),
                ..Words::default()
            },
            _ => Words {
                text: format!("Couldn’t {verb}"),
                failed: Some(outcome.unwrap_or("failed").to_string()),
                ..Words::default()
            },
        },
        Some(CallStatus::Ok) => {
            let mut text = done;
            if act.kind == ActKind::Wait
                && let Some(came) = &act.came
            {
                text.push_str(" · ");
                text.push_str(&came_words(came));
            } else if let Some(news) = news {
                text.push_str(&format!(" ({news})"));
            }
            Words {
                text,
                // How its job is going.
                note: outcome.filter(|_| member.call.job).map(str::to_string),
                ..Words::default()
            }
        }
        _ => Words {
            text: doing,
            ..Words::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(kind: ActKind, target: &str) -> Act {
        Act {
            kind,
            target: target.into(),
            ..Act::default()
        }
    }

    /// The rows of `calls` (thinking is `None`), each a row's words or
    /// `step N` for a step that shows as it is.
    fn shown(calls: &[Option<(Act, CallStatus)>]) -> Vec<String> {
        let steps: Vec<Step> = calls
            .iter()
            .enumerate()
            .map(|(index, call)| match call {
                Some((act, status)) => Step::Call(Call {
                    id: index as u64 + 1,
                    act: Some(act),
                    status: Some(*status),
                    turn: Some(1),
                    ..Call::default()
                }),
                None => Step::Thinking,
            })
            .collect();
        said(&steps)
    }

    fn said(steps: &[Step]) -> Vec<String> {
        rows(steps)
            .into_iter()
            .map(|piece| match piece {
                Piece::Step(index) => format!("step {index}"),
                Piece::Row(row) => {
                    let mut text = row.words.text;
                    if let Some(failed) = row.words.failed {
                        text.push_str(&format!(" · ✗ {failed}"));
                    }
                    if let Some(note) = row.words.note {
                        text.push_str(&format!(" · {note}"));
                    }
                    text
                }
            })
            .collect()
    }

    use ActKind::{Command, Edit, Message, Read, Wait, Write};
    use CallStatus::{Failed, Ok, Running};

    fn ok(kind: ActKind, target: &str) -> Option<(Act, CallStatus)> {
        Some((act(kind, target), Ok))
    }

    #[test]
    fn neighbours_of_a_kind_merge_and_name_their_targets() {
        assert_eq!(
            shown(&[ok(Edit, "src/app.rs"), ok(Edit, "src/app.rs")]),
            ["Made 2 edits to app.rs"]
        );
        assert_eq!(shown(&[ok(Edit, "src/app.rs")]), ["Made 1 edit to app.rs"]);
        assert_eq!(
            shown(&[
                ok(Read, "src/tui/app.rs"),
                ok(Read, "src/tui/theme.rs"),
                ok(Read, "src/tui/mod.rs"),
                ok(Read, "src/tui/screen.rs"),
            ]),
            ["Read 4 files in src/tui"]
        );
        assert_eq!(
            shown(&[ok(Read, "a.rs"), ok(Read, "b.rs")]),
            ["Read a.rs and b.rs"]
        );
        // A file read twice counts once.
        assert_eq!(
            shown(&[ok(Read, "src/a.rs"), ok(Read, "src/a.rs")]),
            ["Read a.rs"]
        );
        assert_eq!(
            shown(&[
                ok(Command, "rg -n cache src"),
                ok(Command, "sed -n 1,9p a.rs"),
                ok(Command, "git log --oneline"),
                ok(Command, "rg x"),
            ]),
            ["Ran 4 commands (rg, sed, git)"]
        );
        assert_eq!(
            shown(&[ok(Message, "r3"), ok(Message, "r4"), ok(Message, "r5")]),
            ["Messaged r3, r4 and r5"]
        );
        assert_eq!(
            shown(&[ok(Message, "r3"), ok(Message, "r3")]),
            ["Messaged r3 2 times"]
        );
    }

    /// Reads of a file being edited join its edits, interleaved edits of
    /// two files are one row that names both, and a different kind or a
    /// failure between them splits the run.
    #[test]
    fn mixed_runs_merge_by_what_they_change() {
        assert_eq!(
            shown(&[
                ok(Read, "app.rs"),
                ok(Edit, "app.rs"),
                ok(Read, "app.rs"),
                ok(Edit, "app.rs"),
            ]),
            ["Made 2 edits to app.rs"]
        );
        assert_eq!(
            shown(&[
                ok(Edit, "app.rs"),
                ok(Edit, "theme.rs"),
                ok(Edit, "app.rs"),
                ok(Edit, "theme.rs"),
            ]),
            ["Made 4 edits to app.rs and theme.rs"]
        );
        assert_eq!(
            shown(&[
                ok(Read, "src/a.rs"),
                ok(Read, "src/b.rs"),
                ok(Read, "src/c.rs"),
                ok(Edit, "src/a.rs"),
            ]),
            ["Read 3 files in src", "Made 1 edit to a.rs"]
        );
        assert_eq!(
            shown(&[
                ok(Read, "a.rs"),
                ok(Edit, "a.rs"),
                ok(Read, "b.rs"),
                ok(Edit, "b.rs"),
            ]),
            ["Made 2 edits to a.rs and b.rs"]
        );
        assert_eq!(
            shown(&[ok(Write, "new.rs"), ok(Edit, "new.rs"), ok(Edit, "new.rs")]),
            ["Wrote new.rs", "Made 2 edits to new.rs"]
        );
        assert_eq!(
            shown(&[
                ok(Edit, "a.rs"),
                ok(Command, "cargo check"),
                ok(Edit, "a.rs")
            ]),
            [
                "Made 1 edit to a.rs",
                "Ran cargo check",
                "Made 1 edit to a.rs"
            ]
        );
        assert_eq!(
            shown(&[
                ok(Edit, "a.rs"),
                Some((act(Edit, "a.rs"), Failed)),
                ok(Edit, "a.rs")
            ]),
            [
                "Made 1 edit to a.rs",
                "Couldn’t edit a.rs · ✗ failed",
                "Made 1 edit to a.rs"
            ]
        );
        // Thinking between calls of one row goes inside it.
        let calls = [ok(Read, "a.rs"), None, ok(Read, "b.rs")];
        let pieces = {
            let steps: Vec<Step> = calls
                .iter()
                .map(|call| match call {
                    Some((act, status)) => Step::Call(Call {
                        act: Some(act),
                        status: Some(*status),
                        ..Call::default()
                    }),
                    None => Step::Thinking,
                })
                .collect();
            rows(&steps)
        };
        assert!(
            matches!(pieces.as_slice(), [Piece::Row(row)] if row.members == [0, 1, 2]),
            "{pieces:?}"
        );
        // Thinking elsewhere stays as it is.
        assert_eq!(
            shown(&[None, ok(Read, "a.rs"), None, ok(Command, "ls"), None]),
            ["step 0", "Read a.rs", "step 2", "Ran ls", "step 4"]
        );
    }

    /// Anything else the view shows ends a run, and a call without an act
    /// shows as it is.
    #[test]
    fn other_steps_end_runs() {
        let a = act(Read, "a.rs");
        let b = act(Read, "b.rs");
        let call = |act| {
            Step::Call(Call {
                act: Some(act),
                status: Some(Ok),
                ..Call::default()
            })
        };
        assert_eq!(
            said(&[call(&a), Step::Other(9), call(&b)]),
            ["Read a.rs", "step 1", "Read b.rs"]
        );
        let plain = Step::Call(Call {
            status: Some(Ok),
            ..Call::default()
        });
        assert_eq!(
            said(&[call(&a), plain, call(&b)]),
            ["Read a.rs", "step 1", "Read b.rs"]
        );
    }

    /// A long call joins the row above as it comes and shows there while it
    /// runs, as quick calls join at once. A row of calls that all run
    /// speaks in the present.
    #[test]
    fn running_calls() {
        assert_eq!(
            shown(&[
                ok(Command, "rg x"),
                Some((act(Command, "cargo build"), Running))
            ]),
            ["Ran rg x · running cargo build"]
        );
        assert_eq!(
            shown(&[ok(Command, "rg x"), ok(Command, "cargo build")]),
            ["Ran 2 commands (rg, cargo)"]
        );
        assert_eq!(
            shown(&[ok(Edit, "app.rs"), Some((act(Edit, "app.rs"), Running))]),
            ["Made 2 edits to app.rs"]
        );
        assert_eq!(
            shown(&[
                Some((act(Read, "src/a.rs"), Running)),
                Some((act(Read, "src/b.rs"), Running)),
                Some((act(Read, "src/c.rs"), Running)),
            ]),
            ["Reading 3 files in src"]
        );
        assert_eq!(
            shown(&[Some((act(Edit, "app.rs"), Running))]),
            ["Editing app.rs"]
        );
    }

    /// Failures, stops, test runs and calls with jobs have rows of their own.
    #[test]
    fn what_stands_alone() {
        let mut test = act(Command, "cargo test -p arko");
        test.news = "24 passed".into();
        test.alone = true;
        test.ran = true;
        assert_eq!(
            shown(&[
                ok(Command, "rg x"),
                Some((test.clone(), Ok)),
                ok(Command, "ls")
            ]),
            ["Ran rg x", "Ran cargo test -p arko (24 passed)", "Ran ls"]
        );
        test.news = "1 failed, 23 passed".into();
        assert_eq!(
            shown(&[Some((test, Failed))]),
            ["Ran cargo test -p arko · ✗ 1 failed, 23 passed"]
        );
        let steps = [Step::Call(Call {
            act: Some(&act(Read, "src/cache.rs")),
            status: Some(Failed),
            outcome: Some("No such file or directory (os error 2)"),
            ..Call::default()
        })];
        assert_eq!(
            said(&steps),
            ["Couldn’t read cache.rs · ✗ No such file or directory (os error 2)"]
        );
        let build = act(Command, "cargo build");
        let stopped = [Step::Call(Call {
            act: Some(&build),
            status: Some(Failed),
            outcome: Some(STOPPED),
            ..Call::default()
        })];
        assert_eq!(said(&stopped), ["Stopped running cargo build"]);
        let moved = [
            Step::Call(Call {
                act: Some(&build),
                status: Some(Ok),
                outcome: Some("in the background · job 4d2c1a9b"),
                job: true,
                ..Call::default()
            }),
            Step::Call(Call {
                act: Some(&build),
                status: Some(Ok),
                ..Call::default()
            }),
        ];
        assert_eq!(
            said(&moved),
            [
                "Ran cargo build · in the background · job 4d2c1a9b",
                "Ran cargo build"
            ]
        );
    }

    /// A row keeps its calls once anything shows after it. A long call
    /// that ends while its row is the last joins the row above, and one that
    /// ends after something else showed keeps its own row. A call that
    /// fails while its row is the last stands alone, and one that fails
    /// later stays in its row, which says so. All of it follows from where
    /// each result came (`ended`), so it's the same in every layout.
    #[test]
    fn a_row_keeps_its_calls_once_anything_shows_after_it() {
        fn ended(kind: ActKind, target: &str, ended: Option<u64>) -> Act {
            Act {
                ended,
                ..act(kind, target)
            }
        }
        fn call(id: u64, act: &Act, status: CallStatus) -> Step<'_> {
            Step::Call(Call {
                id,
                act: Some(act),
                status: Some(status),
                ..Call::default()
            })
        }
        // rg ended at once, the build ended after the read showed.
        let rg = ended(Command, "rg x", Some(2));
        let late = ended(Command, "cargo build", Some(9));
        let read = ended(Read, "a.rs", Some(4));
        let running = [
            call(1, &rg, Ok),
            call(2, &late, Running),
            call(3, &read, Ok),
        ];
        // The build joins the row as it comes and shows there while it runs,
        // so no row comes and goes with it.
        assert_eq!(
            said(&running),
            ["Ran rg x · running cargo build", "Read a.rs"]
        );
        let settled = [call(1, &rg, Ok), call(2, &late, Ok), call(3, &read, Ok)];
        assert_eq!(said(&settled), ["Ran 2 commands (rg, cargo)", "Read a.rs"]);
        // As the last row, the same.
        let alone = [call(1, &rg, Ok), call(2, &late, Running)];
        assert_eq!(said(&alone), ["Ran rg x · running cargo build"]);
        assert_eq!(said(&settled[..2]), ["Ran 2 commands (rg, cargo)"]);
        // A build that fails after the read came stays in the row, and one
        // that fails while its row is the last stands alone.
        let failed = [call(1, &rg, Ok), call(2, &late, Failed), call(3, &read, Ok)];
        assert_eq!(
            said(&failed),
            ["Ran 2 commands (rg, cargo) · ✗ 1 failed", "Read a.rs"]
        );
        let early = ended(Command, "cargo build", Some(3));
        let failed = [
            call(1, &rg, Ok),
            call(2, &early, Failed),
            call(3, &read, Ok),
        ];
        assert_eq!(
            said(&failed),
            [
                "Ran rg x",
                "Couldn’t run cargo build · ✗ failed",
                "Read a.rs"
            ]
        );
        // A test run has a row of its own from the start.
        let tests = Act {
            alone: true,
            ..ended(Command, "cargo test", Some(9))
        };
        let run = [call(1, &rg, Ok), call(2, &tests, Running)];
        assert_eq!(said(&run), ["Ran rg x", "Running cargo test"]);
        // A read that fails after the edit came stays in its row.
        let (a, b) = (ended(Read, "a.rs", Some(9)), ended(Read, "b.rs", Some(9)));
        let edit = act(Edit, "c.rs");
        let failed = [call(1, &a, Ok), call(2, &b, Failed), call(3, &edit, Ok)];
        assert_eq!(
            said(&failed),
            ["Read a.rs and b.rs · ✗ 1 failed", "Made 1 edit to c.rs"]
        );
        // One that failed while its row was the last stands alone.
        let early = ended(Read, "b.rs", Some(3));
        let failed = [call(1, &a, Ok), call(2, &early, Failed), call(3, &edit, Ok)];
        assert_eq!(
            said(&failed),
            [
                "Read a.rs",
                "Couldn’t read b.rs · ✗ failed",
                "Made 1 edit to c.rs"
            ]
        );
        // A note waiting at the end seals nothing.
        let build = early_build();
        let waiting = [call(1, &rg, Ok), call(2, &build, Ok), Step::Aside];
        assert_eq!(said(&waiting), ["Ran 2 commands (rg, cargo)", "step 2"]);
    }

    fn early_build() -> Act {
        Act {
            ended: Some(3),
            ..act(Command, "cargo build")
        }
    }

    /// A call that runs says what it does as its row would.
    #[test]
    fn a_call_says_what_it_does() {
        assert_eq!(doing(&act(Wait, "r91")), "Waiting for r91");
        assert_eq!(doing(&act(Wait, "r1, r2, r3, r4")), "Waiting for workers");
        assert_eq!(doing(&act(Command, "cargo test")), "Running cargo test");
        assert_eq!(doing(&act(Read, "src/tui/app.rs")), "Reading app.rs");
    }

    /// Back-to-back waits are one row that says what came.
    #[test]
    fn waits_merge_and_say_what_came() {
        let wait = |runs: &str, came: Came| Act {
            kind: Wait,
            target: runs.into(),
            came: Some(came),
            ..Act::default()
        };
        let update = Came {
            updates: 1,
            ..Came::default()
        };
        let finished = Came {
            finished: vec!["r91".into()],
            ..Came::default()
        };
        assert_eq!(
            shown(&[
                Some((wait("r91, r92, r93, r94", update.clone()), Ok)),
                Some((wait("r91, r92, r93, r94", update.clone()), Ok)),
                Some((wait("r91, r92, r93, r94", finished.clone()), Ok)),
                Some((wait("r92, r93, r94", update.clone()), Ok)),
            ]),
            ["Waited for workers · r91 finished, 3 updates"]
        );
        assert_eq!(
            shown(&[Some((wait("r3", finished), Ok))]),
            ["Waited for r3 · r91 finished"]
        );
        assert_eq!(
            shown(&[Some((wait("", Came::default()), Ok))]),
            ["Waited for a message · nothing new"]
        );
        // One that waits shows on the row while it waits.
        assert_eq!(
            shown(&[
                Some((wait("r3", update.clone()), Ok)),
                Some((act(Wait, "r3"), Running))
            ]),
            ["Waited for r3 · 1 update · waiting for r3"]
        );
    }

    /// A file whose name another file of the turn had first gets its folder.
    #[test]
    fn a_name_two_files_have_gets_its_folder() {
        assert_eq!(
            shown(&[
                ok(Read, "src/tui/mod.rs"),
                ok(Command, "ls"),
                ok(Read, "src/remote/mod.rs"),
            ]),
            ["Read mod.rs", "Ran ls", "Read remote/mod.rs"]
        );
    }
}

//! The Quiet view's rows, which say each call in a few words and merge
//! neighbouring calls of a kind into one row ("Made 3 edits to app.rs",
//! "Read 4 files in src/tui"), the same in every client.
//!
//! A run is the stretch of an agent's calls and thinking between two other
//! things the view shows. Within a run a call joins the row before it when
//! it is of the row's one kind, or when it reads or changes the one file
//! every call of the row reads or changes ("Read app.rs and made 3 edits").
//! A row holds calls that follow each other, so rows never reorder. A
//! thinking between two calls of one row goes inside it, between them, and
//! any other thinking is a step of its own, so everything shows where it
//! happened. A call's row is
//! decided when the call comes, from what is known then, and nothing that
//! comes later moves it. So a long call (a command, a wait, an MCP call)
//! shows on its row while it runs ("Ran 4 commands · running cargo build"),
//! and a call that fails or goes on in the background stays in its row,
//! which shows one ✗. A failure ends its row, so a call that comes after it
//! starts a row of its own, and so does a row of one call that the reader
//! opened, which keeps its form ([`Call::sealed`]). A test run and a call of another kind have rows of
//! their own. The images a call's tool returned and the buttons of its job
//! show under its row's line, in whatever row it is. Rows only grow at the
//! end, and a row keeps its calls once anything shows after it ([`rows`]).
//!
//! A row's words are whole. Each client cuts them to its width, and keeps
//! what follows them whole ([`Words::news`], [`Words::note`]), so a cut
//! line still says how the call went.
//!
//! An open row of one call shows the call's content right under its line
//! ([`content`](super::content)), after its detail ([`Act::detail`]). An
//! open row of several shows each call's Full line, which opens to the
//! call's content in turn, and the thinking between them.
//!
//! The web page compiles this file beside `protocol.rs` as a module of its
//! own, so it depends on the standard library and that module alone.

use std::collections::HashMap;

#[cfg(test)]
use super::protocol::ContentKind;
use super::protocol::{Act, ActKind, CallStatus, Came, Kind};

/// A call's outcome when a stop cancelled it.
pub const STOPPED: &str = "stopped";
/// A call's outcome when Claude Code was about to compact the conversation, so
/// the agent makes the call again after.
pub const HELD: &str = "held for the compaction";
/// A call's outcome when the session stopped before its result came.
pub const INTERRUPTED: &str = "interrupted";

/// An item of a transcript, as the Quiet view reads it, in the order shown.
#[derive(Debug, Clone, Copy)]
pub enum Step<'a> {
    Call(Call<'a>),
    /// The agent's thinking, which doesn't end a run. One the reader opened
    /// while it stood on a line of its own is [`Step::Other`] instead, so it
    /// stays there and the next call starts a row of its own.
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
    /// It has content, a block of its result, which its open row shows.
    pub content: bool,
    /// For a call that failed, the ID the next item had when it failed
    /// (`Item::ended`). A call with that ID or a higher one came after the
    /// failure.
    pub ended: Option<u64>,
    /// The reader opened its row while it held this call alone. The row
    /// keeps that form, so it takes no more calls, and the next one starts a
    /// row of its own instead of turning it into a row of several. It stays
    /// so once the row closes, so no later row goes into it.
    pub sealed: bool,
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
    /// It holds several calls, each of which shows as its Full line when
    /// it's open. A row of one call shows the call's detail and content
    /// right under its line.
    pub merged: bool,
    /// It has more to show than its words, so it opens. A row that doesn't
    /// has no fold.
    pub opens: bool,
}

/// What a row says. A client cuts `text` to its width and shows the rest
/// whole after it, `news` in parentheses and then `note`, both muted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Words {
    /// What its calls did ("Ran cargo test -p arko", "Read 3 files in src").
    pub text: String,
    /// How it went, as a test run's counts ("1 failed, 23 passed").
    pub news: Option<String>,
    /// A call of it failed, which one ✗ at the row's right says.
    pub failed: bool,
    /// More, said after it, muted, such as how a call's job is going, what
    /// came while it waited, or what of it still runs.
    pub note: Option<String>,
    /// A call a stop or a compaction cut short, whose row is muted.
    pub muted: bool,
    /// A call of it runs.
    pub running: bool,
}

impl Words {
    /// All it says in one line, as a row shows it whole ("Ran cargo test
    /// (2 passed)", "Waited for r3 · r3 finished").
    pub fn said(&self) -> String {
        let mut said = self.text.clone();
        if let Some(news) = &self.news {
            said.push_str(&format!(" ({news})"));
        }
        if let Some(note) = &self.note {
            said.push_str(&format!(" · {note}"));
        }
        said
    }
}

/// The rows of `steps`, with the steps that show as they are. They follow
/// from the items alone, each call's place and what is known of it when it
/// comes (its kind, its target, whether it runs tests), so every client, a
/// reload and a restored session show the same rows, and a row once shown
/// never splits, goes, or takes another row's calls. A call joins the last
/// row or starts a new one. A row keeps its calls once anything else shows
/// after it.
pub fn rows(steps: &[Step]) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut names = Names::default();
    let mut start = 0;
    while start < steps.len() {
        let end = steps[start..]
            .iter()
            .position(|step| matches!(step, Step::Other(_)))
            .map_or(steps.len(), |at| start + at);
        let groups = build(slots(steps, start, end, &mut names));
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
}

impl Member<'_> {
    fn running(&self) -> bool {
        self.call.status == Some(CallStatus::Running)
    }

    /// It failed, which a call a stop or a compaction cut short didn't.
    fn failed(&self) -> bool {
        self.call.status == Some(CallStatus::Failed) && !cut_short(self.call.outcome)
    }

    /// A call that can run long, which a row shows as it runs ("· running
    /// cargo build").
    fn long(&self) -> bool {
        matches!(
            self.act.kind,
            ActKind::Command | ActKind::Wait | ActKind::Mcp | ActKind::Check
        )
    }

    /// Whether it has a row of its own, which is known as it comes (a call
    /// of another kind, a test run).
    fn alone(&self) -> bool {
        self.act.kind == ActKind::Other || self.act.alone
    }

    /// Whether opening it shows something, its detail or its content.
    fn opens(&self) -> bool {
        !self.act.detail.is_empty() || self.call.content
    }
}

/// A run's calls in order, each with its act, or `None` for a call without
/// one, which shows as it is.
fn slots<'a>(
    steps: &[Step<'a>],
    start: usize,
    end: usize,
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
        }));
    }
    slots
}

/// A row being made.
#[derive(Clone)]
struct Group<'a> {
    /// Its calls, in order.
    calls: Vec<Member<'a>>,
    /// It takes no more calls.
    alone: bool,
}

/// Whether a call of `kind` reads or changes a file.
fn of_a_file(kind: ActKind) -> bool {
    matches!(kind, ActKind::Read | ActKind::Write | ActKind::Edit)
}

impl<'a> Group<'a> {
    fn new(member: Member<'a>) -> Self {
        Self {
            alone: member.alone(),
            calls: vec![member],
        }
    }

    /// Its calls by step index, in order.
    fn members(&self) -> Vec<usize> {
        self.calls.iter().map(|member| member.index).collect()
    }

    /// The kind of all its calls, unless it holds the reads and changes of
    /// one file.
    fn kind(&self) -> Option<ActKind> {
        let first = self.calls[0].act.kind;
        self.calls
            .iter()
            .all(|call| call.act.kind == first)
            .then_some(first)
    }

    /// Takes `member` if it belongs in this row. A call joins as it comes,
    /// a long one too, so no row shows only while a call runs.
    fn take(&mut self, member: Member<'a>) -> Result<(), Member<'a>> {
        if self.alone || member.alone() {
            return Err(member);
        }
        // A row of one call that the reader opened keeps its form.
        if self.calls.len() == 1 && self.calls[0].call.sealed {
            return Err(member);
        }
        // A failure ends the row. A call that came while the failed one ran
        // stays in it.
        let after_failure = self.calls.iter().any(|call| {
            call.failed() && call.call.ended.is_some_and(|ended| member.call.id >= ended)
        });
        if after_failure {
            return Err(member);
        }
        let kind = member.act.kind;
        let target = member.act.target.as_str();
        if self.kind() == Some(kind) {
            // A job's checks and an MCP tool's calls merge one target only.
            let one_target = matches!(kind, ActKind::Check | ActKind::Mcp);
            if one_target && self.calls[0].act.target != target {
                return Err(member);
            }
            self.calls.push(member);
            return Ok(());
        }
        // The reads and changes of one file are one thing to the reader.
        let one_file = self
            .calls
            .iter()
            .all(|call| of_a_file(call.act.kind) && call.act.target == target);
        if of_a_file(kind) && one_file {
            self.calls.push(member);
            return Ok(());
        }
        Err(member)
    }
}

/// The rows of a run's calls `slots`, each call joining the last row when
/// it belongs there, else starting a row. A row never takes the calls of
/// a row after it.
fn build(slots: Vec<Option<Member<'_>>>) -> Vec<Group<'_>> {
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
            Some(last) => match last.take(member) {
                Ok(()) => continue,
                Err(member) => member,
            },
            None => member,
        };
        groups.push(Group::new(member));
    }
    groups
}

/// Places a run's rows `groups` in `steps[start..end]`, onto `out`, each row
/// where its first call is, with the thinking between its calls, and the
/// rest as they are.
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
    // A thinking goes in a row when the calls on either side of it are
    // that row's, so a closed row hides only what happened between its
    // first call and its last. Any other thinking has a line of its own,
    // where it happened. The last one stands at the end until the next call
    // comes, and goes in the row above if that call joins it.
    let inside = |index: usize| {
        let before = calls.iter().rev().find(|call| **call < index)?;
        let after = calls.iter().find(|call| **call > index)?;
        let row = row_of.get(before)?;
        (row_of.get(after) == Some(row)).then_some(*row)
    };
    // Each row, and whether its call has something to show.
    let mut more = Vec::with_capacity(groups.len());
    let mut rows: Vec<Option<Row>> = groups
        .iter()
        .map(|group| {
            more.push(group.calls.iter().any(Member::opens));
            Some(Row {
                members: Vec::new(),
                words: words(group),
                merged: group.calls.len() > 1,
                opens: false,
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
    // A row opens when it holds more than one call, or its call has
    // something to show. (A thinking is only ever between two calls.)
    for (row, more) in rows.iter_mut().zip(more) {
        if let Some(row) = row.as_mut() {
            row.opens = row.members.len() > 1 || more;
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
            calls: done.into_iter().cloned().collect(),
            alone: false,
        };
        let mut words = words(&before);
        let running = match live.as_slice() {
            [one] => lower_first(&phrases(one).doing),
            many => format!("running {} more", many.len()),
        };
        words.note = Some(match words.note {
            Some(note) => format!("{note} · {running}"),
            None => running,
        });
        words.running = true;
        return words;
    }
    let calls = &group.calls;
    let running = calls.iter().any(Member::running);
    // (A call that failed stays in its row, which shows one ✗.)
    let failed = calls.iter().any(Member::failed);
    if calls.len() == 1 {
        let mut words = one(&calls[0]);
        words.running = running;
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
    let mut note = Vec::new();
    let text = match group.kind() {
        // The reads and changes of one file, each kind named.
        None => file_words(calls, now),
        Some(ActKind::Read) => {
            format!("{} {}", tense("Reading", "Read"), files(("file", "files")))
        }
        Some(ActKind::List) => format!(
            "{} {}",
            tense("Listing", "Listed"),
            files(("folder", "folders"))
        ),
        Some(ActKind::Write) => {
            format!("{} {}", tense("Writing", "Wrote"), files(("file", "files")))
        }
        Some(ActKind::Edit) => format!(
            "{} {count} edits to {}",
            tense("Making", "Made"),
            files(("file", "files"))
        ),
        Some(ActKind::Command) => {
            let programs = distinct(calls.iter().map(|call| program(&call.act.target)));
            format!(
                "{} {count} commands ({})",
                tense("Running", "Ran"),
                parenthesized(&programs, 3)
            )
        }
        Some(ActKind::Search) => {
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
        Some(ActKind::Skill) => {
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
        Some(ActKind::Message) => {
            let to = targets();
            match to.as_slice() {
                [one] => format!("{} {one} {count} times", tense("Messaging", "Messaged")),
                _ => format!("{} {}", tense("Messaging", "Messaged"), listed(&to, 3)),
            }
        }
        Some(ActKind::Wait) => {
            let mut runs: Vec<String> = Vec::new();
            for call in calls {
                for run in call.act.target.split(',').map(str::trim) {
                    if !run.is_empty() && !runs.iter().any(|seen| seen == run) {
                        runs.push(run.to_string());
                    }
                }
            }
            // (A wait that names no runs waits for any of the workers.)
            let implicit = calls.iter().any(|call| call.act.target.trim().is_empty());
            let who = match runs.len() {
                1..=3 if !implicit => listed(&runs, 3),
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
            if any {
                note.push(came_words(&came));
            }
            format!("{} for {who}", tense("Waiting", "Waited"))
        }
        Some(ActKind::Check) => format!(
            "{} {} {count} times",
            tense("Checking", "Checked"),
            calls[0].name
        ),
        Some(ActKind::Note) => format!("{} {count} notes", tense("Keeping", "Kept")),
        Some(ActKind::Mcp) => format!(
            "{} {} {count} times",
            tense("Calling", "Called"),
            calls[0].name
        ),
        // (A call of another kind has a row of its own.)
        Some(ActKind::Other) => calls[0].act.done.clone(),
    };
    // How the jobs of its calls go, which stay in the row.
    let jobs: Vec<&Member> = calls.iter().filter(|member| member.call.job).collect();
    match jobs.as_slice() {
        [] => {}
        [one] => note.extend(
            one.call
                .outcome
                .map(str::trim)
                .filter(|outcome| !outcome.is_empty())
                .map(str::to_string),
        ),
        many => note.push(plural(many.len(), "job", "jobs")),
    }
    Words {
        text,
        running,
        failed,
        note: (!note.is_empty()).then(|| note.join(" · ")),
        ..Words::default()
    }
}

/// What a row of the reads and changes of one file says, each kind in
/// turn, the file named once ("Read app.rs and made 3 edits", "Read app.rs,
/// wrote it and made 1 edit"). Edits are counted, and a file read or
/// written again is still one.
fn file_words(calls: &[Member], now: bool) -> String {
    let tense = |present: &str, past: &str| if now { present } else { past }.to_string();
    let name = calls[0].name.as_str();
    let has = |kind: ActKind| calls.iter().any(|call| call.act.kind == kind);
    let edits = calls
        .iter()
        .filter(|call| call.act.kind == ActKind::Edit)
        .count();
    let mut parts: Vec<String> = Vec::new();
    if has(ActKind::Read) {
        parts.push(format!("{} {name}", tense("Reading", "Read")));
    }
    if has(ActKind::Write) {
        parts.push(match parts.is_empty() {
            true => format!("{} {name}", tense("Writing", "Wrote")),
            false => format!("{} it", tense("writing", "wrote")),
        });
    }
    if edits > 0 {
        let made = plural(edits, "edit", "edits");
        parts.push(match parts.is_empty() {
            true => format!("{} {made} to {name}", tense("Making", "Made")),
            false => format!("{} {made}", tense("making", "made")),
        });
    }
    listed(&parts, 3)
}

/// What a worker's item is in its lead's view, which shows a worker only
/// as its reports and its messages to that lead (the main agent, or the
/// worker that started it). Each is a row of its own, a status and not a
/// call, that opens to its words. The rest of the worker's, its text, its
/// calls and its conversation with the user, is in its own view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerRow {
    /// A report, the reply a run of the worker ended with.
    Reported,
    /// A message to the lead, as its call stands.
    Message(Sending),
}

/// How a worker's message to its lead stands, from its call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sending {
    /// Its call runs.
    Running,
    /// It reached the lead.
    Sent,
    /// It failed or was refused.
    Failed,
    /// A stop, a compaction or the session's end cut its call short.
    CutShort,
}

impl WorkerRow {
    /// The row of a worker's item of `kind` in the view of `lead` (the
    /// main agent's "main", or a worker's ID), whose worker it is, with its
    /// `act`, `status` and `outcome` for a call, if it has one there.
    pub fn of(
        kind: Kind,
        act: Option<&Act>,
        status: Option<CallStatus>,
        outcome: Option<&str>,
        lead: &str,
    ) -> Option<Self> {
        match kind {
            Kind::Reply => Some(WorkerRow::Reported),
            Kind::Call
                if act.is_some_and(|act| act.kind == ActKind::Message && act.target == lead) =>
            {
                Some(WorkerRow::Message(match status {
                    Some(CallStatus::Ok) => Sending::Sent,
                    Some(CallStatus::Failed) if cut_short(outcome) => Sending::CutShort,
                    Some(CallStatus::Failed) => Sending::Failed,
                    Some(CallStatus::Running) | None => Sending::Running,
                }))
            }
            _ => None,
        }
    }

    /// What the row says after the worker's name ("r1 reported", "r1
    /// messaged the lead"), with its call's `outcome` after it when a stop
    /// cut the call short, ✗ when it failed, and the spinner while it runs.
    pub fn words(self, outcome: Option<&str>) -> Words {
        let text = match self {
            WorkerRow::Reported => "reported",
            WorkerRow::Message(Sending::Running) => "messaging the lead",
            WorkerRow::Message(Sending::Sent) => "messaged the lead",
            WorkerRow::Message(Sending::Failed | Sending::CutShort) => "couldn’t message the lead",
        };
        let cut = self == WorkerRow::Message(Sending::CutShort);
        Words {
            text: text.into(),
            failed: self == WorkerRow::Message(Sending::Failed),
            note: outcome.map(str::trim).filter(|_| cut).map(str::to_string),
            muted: cut,
            running: self == WorkerRow::Message(Sending::Running),
            ..Words::default()
        }
    }
}

/// Whether a call's `outcome` says a stop, a compaction or the session's
/// end cut it short, which isn't a failure.
fn cut_short(outcome: Option<&str>) -> bool {
    matches!(outcome.map(str::trim), Some(STOPPED | HELD | INTERRUPTED))
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
            // (One that names no runs waits for the agent's workers.)
            let who = match runs.len() {
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
        // A stop or a compaction cut it short.
        Some(CallStatus::Failed) if !member.failed() => match outcome {
            Some(STOPPED) => Words {
                text: format!("Stopped {}", lower_first(&doing)),
                muted: true,
                ..Words::default()
            },
            outcome => Words {
                text: doing,
                note: outcome.map(str::to_string),
                muted: true,
                ..Words::default()
            },
        },
        // It ran, and its tests' counts are news.
        Some(CallStatus::Failed) if act.ran || member.call.job => Words {
            text: done,
            news,
            failed: true,
            ..Words::default()
        },
        Some(CallStatus::Failed) => Words {
            text: format!("Couldn’t {verb}"),
            failed: true,
            ..Words::default()
        },
        // What came while it waited, else its news, and how its job is
        // going.
        Some(CallStatus::Ok) => match &act.came {
            Some(came) if act.kind == ActKind::Wait => Words {
                text: done,
                note: Some(came_words(came)),
                ..Words::default()
            },
            _ => Words {
                text: done,
                news,
                note: outcome.filter(|_| member.call.job).map(str::to_string),
                ..Words::default()
            },
        },
        _ => Words {
            text: doing,
            ..Words::default()
        },
    }
}

/// A call as `render` makes its item and its result's block from its tool,
/// parameters and result, and what its Quiet row says and opens to. The
/// clients' tests share them ([`fixtures`]).
#[cfg(test)]
pub(crate) struct Fixture {
    pub name: &'static str,
    pub tool: &'static str,
    /// Its parameters, as JSON.
    pub parameters: &'static str,
    /// Its result, or why it failed when `status` is failed.
    pub output: &'static str,
    /// For an edit, its file's text as the edit started, which the tests
    /// edit with `edit_file` for its receipt.
    pub before: Option<&'static str>,
    /// Its act, once its result came.
    pub act: Act,
    /// Its Full line, and the detail Full shows under it.
    pub full: &'static str,
    pub full_detail: Option<&'static str>,
    pub status: CallStatus,
    /// Its outcome on its Full line.
    pub outcome: &'static str,
    /// For a failure, the ID the next item had when it failed, which the
    /// feed keeps (`Item::ended`).
    pub ended: Option<u64>,
    pub job: bool,
    /// Its content, the kind and the preview, and the preview's door.
    pub content: Option<(ContentKind, &'static str)>,
    pub more: Option<&'static str>,
    /// Its Quiet row's words, whether they show a ✗, and whether it opens.
    pub words: &'static str,
    pub failed: bool,
    pub opens: bool,
}

/// Calls of every kind (a plain tool's result, an edit's diff, a read of
/// many lines, a monitor whose command the words cut, a message), and
/// commands cut short or run in a folder,
/// a failed and a refused command, searches, a wait, a worker start, an
/// MCP call, a note, a moved call, and calls with nothing to show.
#[cfg(test)]
pub(crate) fn fixtures() -> Vec<Fixture> {
    use ContentKind::{Diff, Lines, Muted, Plain};
    let act = |kind: ActKind, target: &str| Act {
        kind,
        target: target.into(),
        ..Act::default()
    };
    let other = |doing: &str, done: &str, verb: &str| Act {
        doing: doing.into(),
        done: done.into(),
        verb: verb.into(),
        ..Act::default()
    };
    let fixture = |name, tool, parameters, output, act, full, words| Fixture {
        name,
        tool,
        parameters,
        output,
        before: None,
        act,
        full,
        full_detail: None,
        status: CallStatus::Ok,
        outcome: "",
        ended: None,
        job: false,
        content: None,
        more: None,
        words,
        failed: false,
        opens: true,
    };
    let command = |target: &str| Act {
        ran: true,
        ..act(ActKind::Command, target)
    };
    vec![
        Fixture {
            outcome: "Prefer the models on the user's plans (`cc:` models, the Cha…",
            content: Some((
                Muted,
                "Prefer the models on the user's plans (`cc:` models, the ChatGPT login) when they do the job.\n\nProfiles:\n- reviewer: cc:opus, xhigh effort\n- builder: cc:sonnet, high effort",
            )),
            ..fixture(
                "worker options",
                "worker_options",
                "{}",
                "Prefer the models on the user's plans (`cc:` models, the ChatGPT login) when they do the job.\n\nProfiles:\n- reviewer: cc:opus, xhigh effort\n- builder: cc:sonnet, high effort\n",
                other(
                    "Checking worker options",
                    "Checked worker options",
                    "check worker options",
                ),
                "list worker options",
                "Checked worker options",
            )
        },
        Fixture {
            outcome: "1 edit",
            before: Some(
                "fn branched(&self, index: usize) -> Message {\n    let branch = &self.branches[index];\n    Message {\n        text: branch.text.clone(),\n        summary: branch.summary.clone(),\n        message_index: None,\n        at: branch.at,\n    }\n}\n",
            ),
            content: Some((
                Diff,
                " 3\t    Message {\n 4\t        text: branch.text.clone(),\n 5\t        summary: branch.summary.clone(),\n-6\t        message_index: None,\n+6\t        message_index: Some(index),\n 7\t        at: branch.at,\n 8\t    }\n 9\t}",
            )),
            ..fixture(
                "an edit",
                "edit_file",
                r#"{"path": "src/engine.rs", "old_string": "message_index: None,", "new_string": "message_index: Some(index),"}"#,
                "Edited src/engine.rs at line 6:\n3 |     Message {\n4 |         text: branch.text.clone(),\n5 |         summary: branch.summary.clone(),\n6 |         message_index: Some(index),\n7 |         at: branch.at,\n8 |     }\n9 | }",
                act(ActKind::Edit, "src/engine.rs"),
                "edit src/engine.rs",
                "Made 1 edit to engine.rs",
            )
        },
        Fixture {
            outcome: "3 replacements",
            content: Some((Diff, "-\tlet a = 1;\n+\tlet b = 1;")),
            ..fixture(
                "an edit of every match",
                "edit_file",
                r#"{"path": "src/a.rs", "old_string": "let a = 1;", "new_string": "let b = 1;", "replace_all": true}"#,
                "Replaced 3 occurrences in src/a.rs, at lines 3, 7 and 12.",
                act(ActKind::Edit, "src/a.rs"),
                "edit src/a.rs (all)",
                "Made 1 edit to a.rs",
            )
        },
        Fixture {
            outcome: "20 lines",
            content: Some((
                Lines,
                "120\t- Quiet rows carry no ✓.\n121\t- A failure shows one ✗ at the right of its row.\n122\t- Opened content shows 16 lines.\n123\t- A read's preview shows its line numbers, muted.\n124\t- Full and Quiet share one content model.\n125\t\n126\t## Workers\n127\t\n128\t- A worker's model shows on its line when it differs.\n129\t- Workers run on the plans.\n130\t- A queued worker keeps its row.\n131\t\n132\t## The page\n133\t\n134\t- The door is muted text.\n135\t- Diffs follow Studio's colours.",
            )),
            more: Some("… 4 more lines"),
            ..fixture(
                "a read of many lines",
                "read_file",
                r#"{"path": "/home/user/work/NOTES.md", "start_line": 120, "end_line": 139}"#,
                "120 | - Quiet rows carry no ✓.\n121 | - A failure shows one ✗ at the right of its row.\n122 | - Opened content shows 16 lines.\n123 | - A read's preview shows its line numbers, muted.\n124 | - Full and Quiet share one content model.\n125 | \n126 | ## Workers\n127 | \n128 | - A worker's model shows on its line when it differs.\n129 | - Workers run on the plans.\n130 | - A queued worker keeps its row.\n131 | \n132 | ## The page\n133 | \n134 | - The door is muted text.\n135 | - Diffs follow Studio's colours.\n136 | \n137 | ## Studio\n138 | \n139 | - Studio adopts the crate's rows.\n[Lines 120-139 of 300.]",
                act(ActKind::Read, "/home/user/work/NOTES.md"),
                "read /home/user/work/NOTES.md:120-139",
                "Read NOTES.md",
            )
        },
        Fixture {
            outcome: "monitoring",
            job: true,
            full_detail: Some("every 30 s, when its output changes, for 30 min"),
            ..fixture(
                "a monitor of a long command",
                "monitor_start",
                r#"{"command": "grep -c \"^EXIT\" /home/user/work/logs/build-linux.log /home/user/work/logs/build-windows.log", "every_seconds": 30, "alert_when": "output", "expires_minutes": 30}"#,
                "Monitor 7f3e9a1c-1111-2222-3333-444455556666 started.",
                Act {
                    detail: "every 30 s, when its output changes, for 30 min".into(),
                    ..other(
                        "Starting to watch grep -c \"^EXIT\" /home/user/work/logs/build-linux.log /home/user/work/logs/build-windows.log",
                        "Started watching grep -c \"^EXIT\" /home/user/work/logs/build-linux.log /home/user/work/logs/build-windows.log",
                        "watch grep -c \"^EXIT\" /home/user/work/logs/build-linux.log /home/user/work/logs/build-windows.log",
                    )
                },
                "monitor $ grep -c \"^EXIT\" /home/user/work/logs/build-linux.log /home/user/work/logs/build-windows.log",
                "Started watching grep -c \"^EXIT\" /home/user/work/logs/build-linux.log /home/user/work/logs/build-windows.log · monitoring",
            )
        },
        Fixture {
            outcome: "sent",
            content: Some((
                Plain,
                "Review the fix in src/engine.rs and the test in src/engine_tests.rs. The branched message now carries its index through a handover.",
            )),
            ..fixture(
                "a message",
                "message_agent",
                r#"{"to": "r136", "message": "Review the fix in src/engine.rs and the test in src/engine_tests.rs. The branched message now carries its index through a handover."}"#,
                "Sent: r136 reads it at its next step.",
                act(ActKind::Message, "r136"),
                "message r136 “Review the fix in src/engine.rs and the test in src/engine_tests.rs. The branched message now carries its index through a handover.”",
                "Messaged r136",
            )
        },
        Fixture {
            outcome: "0.0 s",
            content: Some((Plain, "this-is-a-command-with-a-critical-suffix")),
            ..fixture(
                "a command whose words leave out its variable",
                "shell",
                r#"{"command": "CARGO_INCREMENTAL=0 printf '%s' this-is-a-command-with-a-critical-suffix"}"#,
                "exit code 0, 0.0 s\nthis-is-a-command-with-a-critical-suffix",
                Act {
                    detail: "CARGO_INCREMENTAL=0 printf '%s' this-is-a-command-with-a-critical-suffix".into(),
                    news: String::new(),
                    ..command("printf '%s' this-is-a-command-with-a-critical-suffix")
                },
                "$ CARGO_INCREMENTAL=0 printf '%s' this-is-a-command-with-a-critical-suffix",
                "Ran printf '%s' this-is-a-command-with-a-critical-suffix",
            )
        },
        Fixture {
            full_detail: Some("printf one\nprintf two"),
            outcome: "2.0 s",
            content: Some((Plain, "one\ntwo")),
            ..fixture(
                "a command of two lines in a folder",
                "shell",
                r#"{"command": "printf one\nprintf two", "cwd": "src/specific/work"}"#,
                "exit code 0, 2.0 s\none\ntwo",
                Act {
                    detail: "(in src/specific/work) printf one\nprintf two".into(),
                    ..command("printf one in work")
                },
                "(in src/specific/work) $ printf one  (+1 lines)",
                "Ran printf one in work",
            )
        },
        Fixture {
            outcome: "2 passed",
            content: Some((
                Plain,
                "running 2 tests\ntest a ... ok\ntest b ... ok\n\ntest result: ok. 2 passed; 0 failed",
            )),
            ..fixture(
                "a test run",
                "shell",
                r#"{"command": "cargo test -p arko-tui engine_tests"}"#,
                "exit code 0, 41.8 s\nrunning 2 tests\ntest a ... ok\ntest b ... ok\n\ntest result: ok. 2 passed; 0 failed",
                Act {
                    alone: true,
                    news: "2 passed".into(),
                    ..command("cargo test -p arko-tui engine_tests")
                },
                "$ cargo test -p arko-tui engine_tests",
                "Ran cargo test -p arko-tui engine_tests (2 passed)",
            )
        },
        Fixture {
            status: CallStatus::Failed,
            ended: Some(2),
            outcome: "exit 101",
            content: Some((
                Plain,
                "error: could not compile `arko-tui` (lib test) due to 1 previous error",
            )),
            words: "Ran cargo clippy",
            failed: true,
            ..fixture(
                "a failed command",
                "shell",
                r#"{"command": "cargo clippy"}"#,
                "exit code 101, 3.0 s\nerror: could not compile `arko-tui` (lib test) due to 1 previous error",
                command("cargo clippy"),
                "$ cargo clippy",
                "",
            )
        },
        Fixture {
            status: CallStatus::Failed,
            ended: Some(2),
            outcome: "refused",
            content: Some((
                Muted,
                "Refused. ../old-notes is outside the working directory, and outside access is off.",
            )),
            words: "Couldn’t run rm -rf ../old-notes",
            failed: true,
            ..fixture(
                "a refused command",
                "shell",
                r#"{"command": "rm -rf ../old-notes"}"#,
                "refused (../old-notes is outside the working directory, and outside access is off.)",
                act(ActKind::Command, "rm -rf ../old-notes"),
                "$ rm -rf ../old-notes",
                "",
            )
        },
        Fixture {
            outcome: "0.1 s",
            opens: false,
            ..fixture(
                "a command with no output",
                "shell",
                r#"{"command": "sed -i 's/a/b/' src/a.rs"}"#,
                "exit code 0, 0.1 s\n(no output)",
                command("sed -i 's/a/b/' src/a.rs"),
                "$ sed -i 's/a/b/' src/a.rs",
                "Ran sed -i 's/a/b/' src/a.rs",
            )
        },
        Fixture {
            outcome: "in the background",
            job: true,
            opens: false,
            ..fixture(
                "a moved call",
                "shell",
                r#"{"command": "cargo build"}"#,
                "in the background as job 4d2c1a9b-1111-2222-3333-444455556666, 600.0 s\nterminal_status shows how it goes.",
                command("cargo build"),
                "$ cargo build",
                "Ran cargo build · in the background",
            )
        },
        Fixture {
            outcome: "2 results from arko (code, hybrid)",
            content: Some((
                Plain,
                "2 results from arko (code, hybrid)\nsrc/engine.rs:96-105 · fn branched\nsrc/sessions.rs:412-440 · fn restore_branch",
            )),
            ..fixture(
                "a search",
                "index_search",
                r#"{"index": "arko", "query": "branched message index"}"#,
                "2 results from arko (code, hybrid):\n\nsrc/engine.rs:96-105 · fn branched\n96 | fn branched() {\n97 | }\nsrc/sessions.rs:412-440 · fn restore_branch (part of a 200-line definition at lines 400-600: read_file shows the rest)\n412 | fn restore_branch() {",
                Act {
                    query: "branched message index".into(),
                    ..act(ActKind::Search, "arko")
                },
                "index search “branched message index”",
                "Searched the arko index for “branched message index”",
            )
        },
        Fixture {
            outcome: "1 result from arko (code, hybrid)",
            full_detail: None,
            content: Some((
                Plain,
                "1 result from arko (code, hybrid)\nsrc/remote/quiet.rs:138-155 · fn rows",
            )),
            ..fixture(
                "a search past the row's length",
                "index_search",
                r#"{"index": "arko", "query": "how does the feed decide where a long call goes when its row is the last one"}"#,
                "1 result from arko (code, hybrid):\nsrc/remote/quiet.rs:138-155 · fn rows\n    pub fn rows(steps: &[Step]) -> Vec<Piece> {",
                Act {
                    query: "how does the feed decide where a long call goes when its row…".into(),
                    detail: "how does the feed decide where a long call goes when its row is the last one".into(),
                    ..act(ActKind::Search, "arko")
                },
                "index search “how does the feed decide where a long call goes when its row is the last one”",
                "Searched the arko index for “how does the feed decide where a long call goes when its row…”",
            )
        },
        Fixture {
            outcome: "r3 finished, 1 message",
            content: Some((
                Plain,
                "r3 finished. Its report:\n\nThe fix is right.\n\nr4 sent a message:\n\nHalfway.",
            )),
            ..fixture(
                "a wait",
                "wait_for_agents",
                r#"{"runs": "r3, r4"}"#,
                "[mail 2] r3 (agent `worker`) finished. Its report:\n\nThe fix is right.\n\n[end of mail 2]\n\n[mail 3] r4 (agent `worker`) sent you a message:\n\nHalfway.\n\n[end of mail 3]",
                Act {
                    came: Some(Came {
                        finished: vec!["r3".into()],
                        messages: 1,
                        ..Came::default()
                    }),
                    ..act(ActKind::Wait, "r3, r4")
                },
                "waiting for r3, r4",
                "Waited for r3 and r4 · r3 finished, 1 message",
            )
        },
        Fixture {
            outcome: "nothing new",
            opens: false,
            ..fixture(
                "a wait that got nothing",
                "wait_for_agents",
                r#"{"runs": "r1"}"#,
                "Nothing to wait for: r1 is finished. Reports that came earlier reached you already.",
                Act {
                    came: Some(Came::default()),
                    ..act(ActKind::Wait, "r1")
                },
                "waiting for r1",
                "Waited for r1 · nothing new",
            )
        },
        Fixture {
            outcome: "r12 queued",
            content: Some((Muted, "Count the files in src that have no tests.")),
            ..fixture(
                "a worker start",
                "start_agent",
                r#"{"task": "Count the files in src that have no tests.", "model": "sonnet"}"#,
                "Queued r12 (agent `worker`). It starts when a place is free.",
                Act {
                    done: "Queued r12".into(),
                    ..other("Starting a worker", "Started a worker", "start a worker")
                },
                "start worker (sonnet) “Count the files in src that have no tests.”",
                "Queued r12",
            )
        },
        Fixture {
            outcome: "Created #412 https://github.com/mesa-hills-research/arko/iss…",
            content: Some((
                Plain,
                "{\n  \"title\": \"Branched message loses its index\"\n}\nCreated #412 https://github.com/mesa-hills-research/arko/issues/412",
            )),
            ..fixture(
                "an MCP call",
                "mcp",
                r#"{"action": "call", "server": "github", "tool": "create_issue", "arguments": {"title": "Branched message loses its index"}}"#,
                "Created #412 https://github.com/mesa-hills-research/arko/issues/412",
                act(ActKind::Mcp, "create_issue on github"),
                "mcp create_issue on github",
                "Called create_issue on github",
            )
        },
        Fixture {
            outcome: "Kept note 1.",
            content: Some((Plain, "Use tabs in the tests.")),
            ..fixture(
                "a note",
                "keep_note",
                r#"{"text": "Use tabs in the tests."}"#,
                "Kept note 1.",
                act(ActKind::Note, ""),
                "keep_note",
                "Kept a note",
            )
        },
        Fixture {
            content: Some((Lines, "1\tuse super::*;\n2\t\n3\t#[test]\n4\tfn a() {}")),
            ..fixture(
                "a write",
                "write_file",
                r#"{"path": "src/engine_tests.rs", "content": "use super::*;\n\n#[test]\nfn a() {}\n"}"#,
                "Wrote src/engine_tests.rs.",
                act(ActKind::Write, "src/engine_tests.rs"),
                "write src/engine_tests.rs (4 lines)",
                "Wrote engine_tests.rs",
            )
        },
        Fixture {
            outcome: "4 entries",
            content: Some((Plain, "engine.rs\nengine_tests.rs\nmain.rs\nsessions.rs")),
            ..fixture(
                "a list",
                "list_files",
                r#"{"directory": "src"}"#,
                "engine.rs\nengine_tests.rs\nmain.rs\nsessions.rs",
                act(ActKind::List, "src"),
                "list src",
                "Listed src",
            )
        },
        Fixture {
            content: Some((
                Plain,
                "# Ponytail\n\nSolve the whole problem with the best change.",
            )),
            ..fixture(
                "a skill",
                "load_skill",
                r#"{"name": "ponytail"}"#,
                "--- BEGIN SKILL: ponytail ---\n# Ponytail\n\nSolve the whole problem with the best change.\n--- END SKILL: ponytail ---",
                act(ActKind::Skill, "ponytail"),
                "skill ponytail",
                "Loaded the ponytail skill",
            )
        },
    ]
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

    /// A worker's row in its lead's view keeps how its message's call
    /// stands, and a message counts only in the view of the lead it's to.
    #[test]
    fn a_workers_row_keeps_its_messages_call() {
        let message = act(ActKind::Message, "r1");
        let row = |status, outcome| {
            WorkerRow::of(Kind::Call, Some(&message), Some(status), outcome, "r1")
                .map(|row| row.words(outcome))
        };
        let said = |words: Option<Words>| {
            words.map(|words| (words.text, words.note, words.failed, words.running))
        };
        assert_eq!(
            said(row(CallStatus::Running, None)),
            Some(("messaging the lead".into(), None, false, true))
        );
        assert_eq!(
            said(row(CallStatus::Ok, Some("sent"))),
            Some(("messaged the lead".into(), None, false, false))
        );
        assert_eq!(
            said(row(CallStatus::Failed, Some("refused"))),
            Some(("couldn’t message the lead".into(), None, true, false))
        );
        assert_eq!(
            said(row(CallStatus::Failed, Some(STOPPED))),
            Some((
                "couldn’t message the lead".into(),
                Some(STOPPED.into()),
                false,
                false
            ))
        );
        // To another agent than the view's, it's no row there.
        let to_main = act(ActKind::Message, "main");
        assert_eq!(
            WorkerRow::of(Kind::Call, Some(&to_main), None, None, "r1"),
            None
        );
        assert_eq!(
            WorkerRow::of(Kind::Reply, None, None, None, "r1"),
            Some(WorkerRow::Reported)
        );
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
                    let mut text = row.words.said();
                    if row.words.failed {
                        text.push_str(" ✗");
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

    /// The one row of `fixture`'s call alone.
    fn row_of(fixture: &Fixture) -> Row {
        let steps = [Step::Call(Call {
            id: 1,
            act: Some(&fixture.act),
            status: Some(fixture.status),
            outcome: Some(fixture.outcome),
            job: fixture.job,
            turn: Some(1),
            content: fixture.content.is_some(),
            ended: fixture.ended,
            sealed: false,
        })];
        match rows(&steps).as_slice() {
            [Piece::Row(row)] => row.clone(),
            other => panic!("{}: {other:?}", fixture.name),
        }
    }

    /// A row of one call says what it did in the past tense, with one ✗
    /// when it failed, and opens when it has a detail or content to show.
    /// A row with nothing to show has no fold.
    #[test]
    fn a_rows_words_and_fold_follow_the_fixtures() {
        for fixture in fixtures() {
            let row = row_of(&fixture);
            assert_eq!(row.words.said(), fixture.words, "{}", fixture.name);
            assert_eq!(row.words.failed, fixture.failed, "{}", fixture.name);
            assert_eq!(row.opens, fixture.opens, "{}", fixture.name);
            assert!(!row.merged, "{}", fixture.name);
        }
    }

    /// A row of several calls opens to each call's Full line, the reads an
    /// edit holds among them, and one with a failed call shows one ✗.
    #[test]
    fn a_row_of_several_calls_shows_each_as_its_line() {
        fn call<'a>(id: u64, act: &'a Act, status: CallStatus) -> Step<'a> {
            Step::Call(Call {
                id,
                act: Some(act),
                status: Some(status),
                turn: Some(1),
                ..Call::default()
            })
        }
        let one = |steps: &[Step]| -> Row {
            let pieces = rows(steps);
            let [Piece::Row(row)] = pieces.as_slice() else {
                panic!("{pieces:?}");
            };
            row.clone()
        };
        let (remote, tui) = (act(Read, "src/remote/mod.rs"), act(Read, "src/tui/mod.rs"));
        let row = one(&[call(1, &remote, Ok), call(2, &tui, Ok)]);
        assert!(row.merged && row.opens && !row.words.failed);
        let (a, edit) = (act(Read, "src/a.rs"), act(Edit, "src/a.rs"));
        let row = one(&[call(1, &a, Ok), call(2, &edit, Ok)]);
        assert_eq!(row.words.text, "Read a.rs and made 1 edit");
        assert!(row.merged && row.opens);
        let (rg, git) = (act(Command, "rg x"), act(Command, "git diff"));
        let row = one(&[call(1, &rg, Ok), call(2, &git, Failed)]);
        assert_eq!(row.words.text, "Ran 2 commands (rg, git)");
        assert!(row.words.failed);
    }

    /// A failure ends its row. The calls that come after it start a row of
    /// their own, and those that came while it ran stay in its row, so no
    /// row moves when the failure comes.
    #[test]
    fn a_failure_ends_its_row() {
        let call = |id: u64, act: &'static Act, status: CallStatus, ended: Option<u64>| {
            Step::Call(Call {
                id,
                act: Some(act),
                status: Some(status),
                ended,
                turn: Some(1),
                ..Call::default()
            })
        };
        let leak = |kind: ActKind, target: &str| -> &'static Act {
            Box::leak(Box::new(act(kind, target)))
        };
        let (clippy, rg, sed, git) = (
            leak(Command, "cargo clippy"),
            leak(Command, "rg x"),
            leak(Command, "sed -i x"),
            leak(Command, "git diff"),
        );
        // In turn, each call after the one before ended.
        assert_eq!(
            said(&[
                call(1, rg, Ok, None),
                call(3, clippy, Failed, Some(5)),
                call(6, sed, Ok, None),
                call(8, git, Ok, None),
            ]),
            ["Ran 2 commands (rg, cargo) ✗", "Ran 2 commands (sed, git)"]
        );
        // Called together, the later ones came while the failed one ran.
        assert_eq!(
            said(&[
                call(1, clippy, Failed, Some(5)),
                call(2, sed, Ok, None),
                call(3, git, Ok, None),
                call(6, rg, Ok, None),
            ]),
            ["Ran 3 commands (cargo, sed, git) ✗", "Ran rg x"]
        );
        // A stop isn't a failure, and the next call joins its row.
        let mut stopped = call(1, clippy, Failed, Some(2));
        if let Step::Call(stopped) = &mut stopped {
            stopped.outcome = Some(STOPPED);
        }
        assert_eq!(
            said(&[stopped, call(3, sed, Ok, None)]),
            ["Ran 2 commands (cargo, sed)"]
        );
    }

    /// A row of one call that the reader opened keeps its form, so the next
    /// call of its kind starts a row of its own.
    #[test]
    fn an_opened_row_of_one_call_keeps_its_form() {
        let (a, b, c) = (act(Read, "a.rs"), act(Read, "b.rs"), act(Read, "c.rs"));
        let call = |id: u64, act: &'static Act, sealed: bool| {
            Step::Call(Call {
                id,
                act: Some(act),
                status: Some(Ok),
                sealed,
                turn: Some(1),
                ..Call::default()
            })
        };
        let (a, b, c): (&'static Act, &'static Act, &'static Act) = (
            Box::leak(Box::new(a)),
            Box::leak(Box::new(b)),
            Box::leak(Box::new(c)),
        );
        assert_eq!(
            said(&[call(1, a, false), call(2, b, false)]),
            ["Read a.rs and b.rs"]
        );
        assert_eq!(
            said(&[call(1, a, true), call(2, b, false)]),
            ["Read a.rs", "Read b.rs"]
        );
        // (An edit of the file an opened read read starts its own row too.)
        let edit: &'static Act = Box::leak(Box::new(act(Edit, "a.rs")));
        assert_eq!(
            said(&[call(1, a, true), call(2, edit, false)]),
            ["Read a.rs", "Made 1 edit to a.rs"]
        );
        // A row the reader didn't open takes more calls, and so does a row
        // of several that it opened (the clients mark only a row of one).
        assert_eq!(
            said(&[call(1, a, true), call(2, b, false), call(3, c, false)]),
            ["Read a.rs", "Read b.rs and c.rs"]
        );
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

    /// The reads and changes of one file are one row whose title names
    /// each kind, interleaved edits of two files are one row that names
    /// both, and a different kind between them splits the run. A row never
    /// takes the calls of a row after it.
    #[test]
    fn mixed_runs_merge_by_what_they_change() {
        assert_eq!(
            shown(&[
                ok(Read, "app.rs"),
                ok(Edit, "app.rs"),
                ok(Read, "app.rs"),
                ok(Edit, "app.rs"),
            ]),
            ["Read app.rs and made 2 edits"]
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
            ["Read a.rs and made 1 edit", "Read b.rs and made 1 edit"]
        );
        assert_eq!(
            shown(&[ok(Write, "new.rs"), ok(Edit, "new.rs"), ok(Edit, "new.rs")]),
            ["Wrote new.rs and made 2 edits"]
        );
        // A row of one file's reads and changes takes no other file, and a
        // row of edits of two files no read.
        assert_eq!(
            shown(&[ok(Read, "a.rs"), ok(Edit, "a.rs"), ok(Edit, "b.rs")]),
            ["Read a.rs and made 1 edit", "Made 1 edit to b.rs"]
        );
        assert_eq!(
            shown(&[ok(Edit, "a.rs"), ok(Edit, "b.rs"), ok(Read, "a.rs")]),
            ["Made 2 edits to a.rs and b.rs", "Read a.rs"]
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
        // A failure stays in its row, which says so.
        assert_eq!(
            shown(&[
                ok(Edit, "a.rs"),
                Some((act(Edit, "a.rs"), Failed)),
                ok(Edit, "a.rs")
            ]),
            ["Made 3 edits to a.rs ✗"]
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
        // Any other thinking is a line of its own, where it happened.
        assert_eq!(
            shown(&[None, ok(Read, "a.rs"), None, ok(Command, "ls"), None]),
            ["step 0", "Read a.rs", "step 2", "Ran ls", "step 4"]
        );
        // So does one after a test run, which takes no more calls.
        let test = Act {
            alone: true,
            ..act(Command, "cargo test")
        };
        assert_eq!(
            shown(&[Some((test, Ok)), None, ok(Command, "ls")]),
            ["Ran cargo test", "step 1", "Ran ls"]
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

    /// Test runs and calls of another kind have rows of their own. A call
    /// that failed, stopped or went on in the background says so, in the
    /// row it joined.
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
            ["Ran cargo test -p arko (1 failed, 23 passed) ✗"]
        );
        let steps = [Step::Call(Call {
            act: Some(&act(Read, "src/cache.rs")),
            status: Some(Failed),
            outcome: Some("No such file or directory (os error 2)"),
            ..Call::default()
        })];
        assert_eq!(said(&steps), ["Couldn’t read cache.rs ✗"]);
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
                outcome: Some("in the background"),
                job: true,
                ..Call::default()
            }),
            Step::Call(Call {
                act: Some(&build),
                status: Some(Ok),
                ..Call::default()
            }),
        ];
        assert_eq!(said(&moved[..1]), ["Ran cargo build · in the background"]);
        assert_eq!(said(&moved), ["Ran 2 commands (cargo) · in the background"]);
    }

    /// A call keeps the row it joined, whatever its result, and a row keeps
    /// its calls once anything shows after it.
    #[test]
    fn a_call_keeps_the_row_it_joined() {
        fn call(id: u64, act: &Act, status: CallStatus) -> Step<'_> {
            Step::Call(Call {
                id,
                act: Some(act),
                status: Some(status),
                ..Call::default()
            })
        }
        let (rg, build, read) = (
            act(Command, "rg x"),
            act(Command, "cargo build"),
            act(Read, "a.rs"),
        );
        let running = [
            call(1, &rg, Ok),
            call(2, &build, Running),
            call(3, &read, Ok),
        ];
        assert_eq!(
            said(&running),
            ["Ran rg x · running cargo build", "Read a.rs"]
        );
        let settled = [call(1, &rg, Ok), call(2, &build, Ok), call(3, &read, Ok)];
        assert_eq!(said(&settled), ["Ran 2 commands (rg, cargo)", "Read a.rs"]);
        let failed = [
            call(1, &rg, Ok),
            call(2, &build, Failed),
            call(3, &read, Ok),
        ];
        assert_eq!(said(&failed), ["Ran 2 commands (rg, cargo) ✗", "Read a.rs"]);
        // As the last row, the same.
        assert_eq!(said(&failed[..2]), ["Ran 2 commands (rg, cargo) ✗"]);
        // A test run has a row of its own from the start.
        let tests = Act {
            alone: true,
            ..act(Command, "cargo test")
        };
        let run = [call(1, &rg, Ok), call(2, &tests, Running)];
        assert_eq!(said(&run), ["Ran rg x", "Running cargo test"]);
        // A note waiting at the end seals nothing.
        let waiting = [call(1, &rg, Ok), call(2, &build, Ok), Step::Aside];
        assert_eq!(said(&waiting), ["Ran 2 commands (rg, cargo)", "step 2"]);
    }

    /// The keys of the rows of `steps`, each by its first call's ID, and
    /// the steps that show as they are.
    fn keys(steps: &[Step]) -> Vec<String> {
        let id = |at: usize| match steps[at] {
            Step::Call(call) => call.id,
            _ => 0,
        };
        rows(steps)
            .into_iter()
            .map(|piece| match piece {
                Piece::Row(row) => format!("q{}", id(row.members[0])),
                Piece::Step(at) => format!("s{at}"),
            })
            .collect()
    }

    /// Each of `snapshots` is how the steps stand at a moment, the later
    /// ones with more steps or later results. A row once shown keeps its
    /// key, and rows only come at the end.
    fn assert_rows_only_grow(snapshots: &[Vec<Step>]) {
        for pair in snapshots.windows(2) {
            let (before, after) = (keys(&pair[0]), keys(&pair[1]));
            assert!(
                after.starts_with(&before),
                "{before:?} then {after:?}\n{:#?}",
                said(&pair[1])
            );
        }
    }

    /// No row splits, goes or takes another's calls as calls start, run,
    /// end, fail, go on in the background or end out of order.
    #[test]
    fn rows_only_grow_at_the_end() {
        fn call(id: u64, act: &Act, status: CallStatus) -> Step<'_> {
            Step::Call(Call {
                id,
                act: Some(act),
                status: Some(status),
                turn: Some(1),
                ..Call::default()
            })
        }
        fn moved(id: u64, act: &Act) -> Step<'_> {
            Step::Call(Call {
                id,
                act: Some(act),
                status: Some(Ok),
                outcome: Some("in the background"),
                job: true,
                turn: Some(1),
                ..Call::default()
            })
        }
        let (rg, build) = (act(Command, "rg x"), act(Command, "cargo build"));
        let start = vec![call(1, &rg, Ok)];
        let running = vec![call(1, &rg, Ok), call(2, &build, Running)];
        for end in [
            call(2, &build, Ok),
            call(2, &build, Failed),
            moved(2, &build),
        ] {
            let ended = vec![call(1, &rg, Ok), end];
            let next = vec![call(1, &rg, Ok), end, call(3, &rg, Running)];
            assert_rows_only_grow(&[start.clone(), running.clone(), ended, next]);
        }
        // An edit of a file read after an edit row.
        let (a, b, edit_b) = (act(Edit, "a.rs"), act(Read, "b.rs"), act(Edit, "b.rs"));
        assert_rows_only_grow(&[
            vec![call(1, &a, Ok)],
            vec![call(1, &a, Ok), call(2, &b, Ok)],
            vec![call(1, &a, Ok), call(2, &b, Ok), call(3, &edit_b, Running)],
            vec![call(1, &a, Ok), call(2, &b, Ok), call(3, &edit_b, Ok)],
        ]);
        // Parallel calls of a kind that end in the other order, one failing.
        let (x, y) = (act(Read, "src/x.rs"), act(Read, "src/y.rs"));
        assert_rows_only_grow(&[
            vec![call(1, &x, Running)],
            vec![call(1, &x, Running), call(2, &y, Running)],
            vec![call(1, &x, Running), call(2, &y, Failed)],
            vec![call(1, &x, Ok), call(2, &y, Failed)],
            vec![call(1, &x, Ok), call(2, &y, Failed), call(3, &rg, Ok)],
        ]);
        // Thinking after a row stands at the end, and goes in the row when
        // the next call joins it. Nothing else moves.
        let thinking = [
            vec![call(1, &rg, Ok)],
            vec![call(1, &rg, Ok), Step::Thinking],
            vec![call(1, &rg, Ok), Step::Thinking, call(3, &build, Running)],
            vec![call(1, &rg, Ok), Step::Thinking, call(3, &build, Ok)],
        ];
        assert_eq!(
            thinking.iter().map(|steps| keys(steps)).collect::<Vec<_>>(),
            [vec!["q1"], vec!["q1", "s1"], vec!["q1"], vec!["q1"]]
        );
        assert_rows_only_grow(&thinking[2..]);
        // A failure after something showed, then more calls.
        assert_rows_only_grow(&[
            vec![call(1, &rg, Ok), call(2, &build, Running), Step::Other(3)],
            vec![call(1, &rg, Ok), call(2, &build, Failed), Step::Other(3)],
            vec![
                call(1, &rg, Ok),
                call(2, &build, Failed),
                Step::Other(3),
                call(4, &rg, Ok),
            ],
        ]);
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
            shown(&[Some((wait("r3", finished.clone()), Ok))]),
            ["Waited for r3 · r91 finished"]
        );
        // One that names no runs waits for the agent's workers, also beside
        // one that names some.
        assert_eq!(
            shown(&[Some((wait("", Came::default()), Ok))]),
            ["Waited for workers · nothing new"]
        );
        assert_eq!(
            shown(&[
                Some((wait("", Came::default()), Ok)),
                Some((wait("r1", finished.clone()), Ok))
            ]),
            ["Waited for workers · r91 finished"]
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

    /// A thinking sits where it happened. Between two calls of one row it is
    /// inside that row, between them, so a row of one call never holds
    /// one. Before the first call, after the last, and between rows it is a
    /// step of its own. One the reader opened on its own line comes as
    /// `Other`, so it stays there and the next call starts a row.
    #[test]
    fn a_thinking_sits_where_it_happened() {
        let (a, b, c) = (
            act(Read, "src/a.rs"),
            act(Read, "src/b.rs"),
            act(Read, "src/c.rs"),
        );
        let edit = act(Edit, "src/a.rs");
        let call = |id: u64, act| {
            Step::Call(Call {
                id,
                act: Some(act),
                status: Some(Ok),
                turn: Some(1),
                ..Call::default()
            })
        };
        let pieces = |steps: &[Step]| -> Vec<String> {
            rows(steps)
                .into_iter()
                .map(|piece| match piece {
                    Piece::Step(at) => format!("step {at}"),
                    Piece::Row(row) => format!("{} {:?}", row.words.text, row.members),
                })
                .collect()
        };
        // The lab's reads, a thinking between each, then a long one and an
        // edit of another kind.
        let steps = [
            call(1, &a),
            Step::Thinking,
            call(3, &b),
            Step::Thinking,
            call(5, &c),
            Step::Thinking,
            call(7, &edit),
        ];
        assert_eq!(
            pieces(&steps),
            [
                "Read 3 files in src [0, 1, 2, 3, 4]",
                "step 5",
                "Made 1 edit to a.rs [6]"
            ]
        );
        // The last thinking stands at the end until the next call comes.
        assert_eq!(
            pieces(&steps[..4]),
            ["Read a.rs and b.rs [0, 1, 2]", "step 3"]
        );
        // A row of one call holds no thinking.
        assert_eq!(
            pieces(&[Step::Thinking, call(2, &a), Step::Thinking]),
            ["step 0", "Read a.rs [1]", "step 2"]
        );
        // Opened on its own line, it stays there, and the next read starts
        // a row of its own.
        assert_eq!(
            pieces(&[call(1, &a), Step::Other(2), call(3, &b)]),
            ["Read a.rs [0]", "step 1", "Read b.rs [2]"]
        );
    }

    /// A call that runs with nothing to show has no fold, and its row gets
    /// one when its result brings content, keeping its key.
    #[test]
    fn a_running_row_opens_once_its_result_comes() {
        let wait = act(Wait, "r3");
        let step = |status: CallStatus, content: bool| {
            Step::Call(Call {
                id: 1,
                act: Some(&wait),
                status: Some(status),
                content,
                turn: Some(1),
                ..Call::default()
            })
        };
        let row = |step: Step| match rows(&[step]).as_slice() {
            [Piece::Row(row)] => (row.opens, row.words.running),
            other => panic!("{other:?}"),
        };
        assert_eq!(row(step(Running, false)), (false, true));
        assert_eq!(row(step(Ok, true)), (true, false));
        assert_eq!(keys(&[step(Running, false)]), keys(&[step(Ok, true)]));
    }

    /// A row of one file's reads and changes names each kind once, the
    /// file once, and counts the edits.
    #[test]
    fn a_files_reads_and_changes_name_each_kind() {
        assert_eq!(
            shown(&[ok(Read, "src/app.rs"), ok(Edit, "src/app.rs")]),
            ["Read app.rs and made 1 edit"]
        );
        assert_eq!(
            shown(&[
                ok(Read, "src/app.rs"),
                ok(Write, "src/app.rs"),
                ok(Edit, "src/app.rs"),
                ok(Read, "src/app.rs"),
            ]),
            ["Read app.rs, wrote it and made 1 edit"]
        );
        assert_eq!(
            shown(&[ok(Read, "src/app.rs"), ok(Write, "src/app.rs")]),
            ["Read app.rs and wrote it"]
        );
        assert_eq!(
            shown(&[
                Some((act(Read, "src/app.rs"), Running)),
                Some((act(Edit, "src/app.rs"), Running)),
            ]),
            ["Reading app.rs and making 1 edit"]
        );
    }

    /// A row's words leave out how it went, which a client keeps whole
    /// when it cuts the words to its width.
    #[test]
    fn a_rows_news_and_note_come_apart_from_its_words() {
        let mut test = act(Command, "cargo test -p arko");
        test.news = "24 passed".into();
        test.alone = true;
        test.ran = true;
        let steps = [Step::Call(Call {
            act: Some(&test),
            status: Some(Ok),
            ..Call::default()
        })];
        let [Piece::Row(row)]: [Piece; 1] = rows(&steps).try_into().unwrap() else {
            panic!()
        };
        assert_eq!(row.words.text, "Ran cargo test -p arko");
        assert_eq!(row.words.news.as_deref(), Some("24 passed"));
        let started = Act {
            done: "Started npm run dev in a terminal".into(),
            ..act(ActKind::Other, "")
        };
        let steps = [Step::Call(Call {
            act: Some(&started),
            status: Some(Ok),
            outcome: Some("running"),
            job: true,
            ..Call::default()
        })];
        let [Piece::Row(row)]: [Piece; 1] = rows(&steps).try_into().unwrap() else {
            panic!()
        };
        assert_eq!(row.words.text, "Started npm run dev in a terminal");
        assert_eq!(row.words.note.as_deref(), Some("running"));
        assert_eq!(
            shown(&[
                ok(Command, "rg x"),
                Some((act(Command, "cargo build"), Running))
            ]),
            ["Ran rg x · running cargo build"]
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

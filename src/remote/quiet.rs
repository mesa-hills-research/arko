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
//! happened. A call's row is decided when the call comes, from what is
//! known then, and nothing that comes later moves it, but for a long call
//! (a command, a wait, an MCP call). That one has a row of its own while
//! it runs ("Running cargo build"), and once it ends it joins the row
//! before it as any call would ("Ran 4 commands (cargo)"), unless anything
//! came below it meanwhile, where it stays on its own row. A call that
//! fails or goes on in the background stays in its row, which shows one ✗.
//! A failure ends its row, so a call that comes after it starts a row of
//! its own, and so does a row of one call that the reader opened, which
//! keeps its form ([`Call::sealed`]). A test run and a call of another kind
//! have rows of their own. The images a call's tool returned and the
//! buttons of its job show under its row's line, in whatever row it is.
//! Rows only grow at the end, and a row keeps its calls once anything shows
//! after it ([`rows`]).
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

use std::collections::{HashMap, HashSet};

#[cfg(test)]
use super::protocol::ContentKind;
use super::protocol::{Act, ActKind, CallStatus, Came, JobFound, Kind, plural};

/// A call's outcome when a stop cancelled it.
pub const STOPPED: &str = "stopped";
/// A call's outcome when Claude Code was about to compact the conversation, so
/// the agent makes the call again after.
pub const HELD: &str = "held for the compaction";
/// A call's outcome when the session stopped before its result came.
pub const INTERRUPTED: &str = "interrupted";
/// A call's outcome when its tool failed and its error is more than its
/// line holds (an edit's), which opening the call shows.
pub const FAILED: &str = "Failed";

/// An item of a transcript, as the Quiet view reads it, in the order shown.
#[derive(Debug, Clone, Copy)]
pub enum Step<'a> {
    Call(Call<'a>),
    /// The agent's thinking, by its item's ID, which doesn't end a run. One
    /// the reader opened while it stood on a line of its own is
    /// [`Step::Other`] instead, so it stays there and the next call starts a
    /// row of its own.
    Thinking(u64),
    /// Anything else the view shows, by its item's ID, which ends a run. A
    /// note the agent read goes by the ID it had while it waited
    /// (`Item::waited_as`), so reading it moves no row.
    Other(u64),
    /// What shows beside the agent's work without ending it (a note the
    /// agent hasn't read, which waits at the end), by its item's ID.
    Aside(u64),
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
    /// For a call that ended, the ID the next item had when it first
    /// stopped running (`Item::settled_at`). A step with a lower ID came
    /// below it while it ran.
    pub settled_at: Option<u64>,
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
    /// More, said after it, muted, such as how a call's job is going, or
    /// what came while it waited.
    pub note: Option<String>,
    /// Its note says how its job runs, while it runs ("running",
    /// "monitoring"), which gives way before its words do
    /// ([`WORDS_KEEP_RUNNING`]) and stands through the run.
    pub note_runs: bool,
    /// How it runs, said after its note, muted, in the room its words leave,
    /// its last part going first (a code run's count of calls, then the call
    /// that runs).
    pub live: Vec<String>,
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
        for live in &self.live {
            said.push_str(&format!(" · {live}"));
        }
        said
    }

    /// What an open row shows of what its line left out, where the line
    /// showed `shown` of the parts after its words ([`Fit::parts`]). With
    /// `words` (its words whole, where the line cut them), the parts it left
    /// out follow them as the line says them. Without, the parts alone
    /// ("failed (exit 1)"), since the line shows its words. None where it
    /// left out neither.
    pub fn left_out(&self, words: Option<&str>, shown: usize) -> Option<String> {
        let parts = self
            .news
            .iter()
            .map(|news| (true, news))
            .chain(self.note.iter().chain(&self.live).map(|part| (false, part)))
            .skip(shown);
        let mut said = words.unwrap_or_default().to_string();
        for (news, part) in parts {
            match (said.is_empty(), news) {
                (true, _) => said.push_str(part),
                (false, true) => said.push_str(&format!(" ({part})")),
                (false, false) => said.push_str(&format!(" · {part}")),
            }
        }
        (!said.is_empty()).then_some(said)
    }
}

/// The rows of `steps`, with the steps that show as they are. They follow
/// from the items alone, each call's place and what is known of it when it
/// comes (its kind, its target, whether it runs tests), and for a long call
/// whether it runs and whether anything came below it before it ended, so
/// every client, a reload and a restored session show the same rows. A row
/// once shown never splits. A long call has a row of its own while it runs,
/// which goes into the row before it once the call ended if it was still
/// the last, and else stays, so nothing above what came below it moves. A
/// call joins the last row or starts a new one. A row keeps its calls once
/// anything else shows after it.
pub fn rows(steps: &[Step]) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut names = Names::default();
    let below = below(steps);
    let mut start = 0;
    while start < steps.len() {
        let end = steps[start..]
            .iter()
            .position(|step| matches!(step, Step::Other(_)))
            .map_or(steps.len(), |at| start + at);
        let groups = build(slots(steps, start, end, &below, &mut names));
        place(steps, start, end, &groups, &mut out);
        if end < steps.len() {
            out.push(Piece::Step(end));
        }
        start = end + 1;
    }
    out
}

/// The mark a call's line shows before its outcome, from how the call went
/// (`status`), ✓ for one that went well ([`CallStatus::Ok`]), ✗ for one that
/// failed, and none while it runs. A look at a job or a wait on it went well
/// whatever became of the job, so its mark says what it found
/// ([`Act::found`]), ✗ for a job that failed, none for one that asks for
/// input or was stopped, and ✓ for one that runs or finished.
pub fn mark(status: Option<CallStatus>, act: Option<&Act>) -> Option<CallStatus> {
    match (status?, act.and_then(|act| act.found)) {
        (CallStatus::Ok, Some(JobFound::Failed)) => Some(CallStatus::Failed),
        (CallStatus::Ok, Some(JobFound::Input | JobFound::Stopped)) => None,
        (CallStatus::Running, _) => None,
        (status, _) => Some(status),
    }
}

/// The words a call's line says after its mark once it ended, from how it
/// went (`status`) and its outcome's words ("41.0 s", "failed (exit 1)").
/// A call that failed says so, whatever else it has to say.
pub fn ended(status: Option<CallStatus>, outcome: Option<&str>) -> Option<&str> {
    match status {
        Some(CallStatus::Failed) => Some(outcome.unwrap_or(FAILED)),
        Some(CallStatus::Ok) => outcome,
        _ => None,
    }
    .map(str::trim)
    .filter(|outcome| !outcome.is_empty())
}

/// What an open call shows of what its line left out, each once. That is
/// its `words` whole where the line cut them (and what it opens to doesn't
/// hold them, [`holds_call`]), then its `outcome`'s words where the line
/// left them out, alone where the line shows its words. Never its mark,
/// which its line shows.
pub fn call_left_out(words: Option<&str>, outcome: Option<&str>) -> Option<String> {
    match (words, outcome) {
        (Some(words), Some(outcome)) => Some(format!("{words} · {outcome}")),
        (words, outcome) => words.or(outcome).map(str::to_string),
    }
}

/// Whether what an open call shows under its line (`held`, its detail and
/// its content) holds what its call as written (`line`) says past its verb
/// and target, so the call as written would only say it again. That is the
/// line from its first "“" or "$ " (a message, a task, a query, a command),
/// else after its first word (a path), without the line's own marks of
/// what it left out (its ellipsis, "(3 more lines)"). Spaces and line
/// breaks count alike.
pub fn holds_call(line: &str, held: &[&str]) -> bool {
    let line = line.trim();
    // (Each where a word starts, so not one inside a path.)
    let start = ["“", "$ "]
        .iter()
        .filter_map(|mark| {
            line.match_indices(mark)
                .map(|(at, _)| at)
                .find(|at| {
                    line[..*at]
                        .chars()
                        .next_back()
                        .is_none_or(char::is_whitespace)
                })
                .map(|at| at + mark.len())
        })
        .min()
        .or_else(|| line.find(char::is_whitespace));
    let Some(start) = start else {
        return false;
    };
    let mut says = line[start..].trim();
    if let Some(at) = says.rfind("  (")
        && says.ends_with(')')
        && says[at + 3..].split_once(' ').is_some_and(|(count, rest)| {
            count.chars().all(|c| c.is_ascii_digit()) && rest.starts_with("more line")
        })
    {
        says = &says[..at];
    }
    let says = one_line(says.trim_end_matches('”').trim_end_matches('…'));
    !says.is_empty() && held.iter().any(|held| one_line(held).contains(&says))
}

/// `text` with each run of spaces and line breaks one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a look at a job or a wait on it found its job failed, which its
/// row's ✗ says as a call's failure does.
fn found_failed(act: &Act) -> bool {
    act.found == Some(JobFound::Failed)
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
        late: false,
    };
    phrases(&member).doing
}

/// An item as the Messages view reads it for its turns' counts of calls
/// ([`counts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// One of the agent's calls, which Messages counts on its turn's line.
    Call {
        id: u64,
        turn: Option<u64>,
        status: Option<CallStatus>,
    },
    /// The agent's thinking, which Messages leaves out.
    Thinking { id: u64, turn: Option<u64> },
    /// Anything Messages shows.
    Shown,
    /// Anything else it leaves out.
    Hidden,
}

/// A turn's calls, which the Messages view counts on one line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Count {
    pub turn: Option<u64>,
    /// Its first call, by which it opens.
    pub first: u64,
    /// The item its line stands at, the first call or the thinking of the
    /// same turn that led to it with nothing Messages shows between them.
    /// Open, the count shows the turn's work from there.
    pub at: u64,
    pub calls: usize,
    pub failed: usize,
    pub running: usize,
}

/// Each turn's count of calls in the Messages view, in order, from what the
/// view makes of each item in the order shown. A thought leads only to a
/// call of its own turn, so a turn that starts with nothing Messages shows
/// (one Arko starts for news) still has its line after a turn that ended in
/// a thought. Every client places the counts by it, so they agree.
pub fn counts(seen: impl IntoIterator<Item = Seen>) -> Vec<Count> {
    let mut counts: Vec<Count> = Vec::new();
    let mut by_turn: HashMap<Option<u64>, usize> = HashMap::new();
    // The first thought since the last thing shown, and its turn.
    let mut thought: Option<(u64, Option<u64>)> = None;
    for seen in seen {
        match seen {
            Seen::Call { id, turn, status } => {
                let led = thought.take().filter(|(_, of)| *of == turn);
                let at = *by_turn.entry(turn).or_insert_with(|| {
                    counts.push(Count {
                        turn,
                        first: id,
                        at: led.map_or(id, |(thought, _)| thought),
                        ..Count::default()
                    });
                    counts.len() - 1
                });
                let count = &mut counts[at];
                count.calls += 1;
                count.failed += usize::from(status == Some(CallStatus::Failed));
                count.running += usize::from(status == Some(CallStatus::Running));
            }
            Seen::Thinking { id, turn } => {
                if thought.is_none_or(|(_, of)| of != turn) {
                    thought = Some((id, turn));
                }
            }
            Seen::Shown => thought = None,
            Seen::Hidden => {}
        }
    }
    counts
}

/// A call of a row.
#[derive(Debug, Clone)]
struct Member<'a> {
    index: usize,
    call: Call<'a>,
    act: &'a Act,
    /// Its target as the row names it (a file by its name).
    name: String,
    /// Something came below it while it ran.
    late: bool,
}

impl Member<'_> {
    fn running(&self) -> bool {
        self.call.status == Some(CallStatus::Running)
    }

    /// It failed, which a call a stop or a compaction cut short didn't.
    fn failed(&self) -> bool {
        self.call.status == Some(CallStatus::Failed) && !cut_short(self.call.outcome)
    }

    /// Its row's ✗ says it failed, or that it looked at a job or waited on
    /// it and found the job failed.
    fn shows_failure(&self) -> bool {
        self.failed() || (self.call.status == Some(CallStatus::Ok) && found_failed(self.act))
    }

    /// The edits it made or is making, a call of several (an `apply_diff`
    /// call's blocks, or the places a `replace_all` changed) counting each.
    /// An edit that failed, or that a stop cut short, made none.
    fn edits(&self) -> usize {
        match (self.act.kind, self.call.status) {
            (ActKind::Edit, Some(CallStatus::Failed)) => 0,
            (ActKind::Edit, _) => self.act.edits.max(1) as usize,
            _ => 0,
        }
    }

    /// A call that can run long, which has a row of its own while it runs.
    fn long(&self) -> bool {
        matches!(
            self.act.kind,
            ActKind::Command | ActKind::Wait | ActKind::JobWait | ActKind::Mcp | ActKind::Check
        )
    }

    /// Whether it joins no row before it, as a call of another kind, a code
    /// run and a test run don't, which is known as it comes, and a long call
    /// while it runs. That one joins the row before it once it ended, unless
    /// something came below it meanwhile, which settles it where it is.
    fn alone(&self) -> bool {
        matches!(self.act.kind, ActKind::Other | ActKind::Code)
            || self.act.alone
            || (self.long() && (self.running() || self.late))
    }

    /// When it took its place among the rows (by the IDs of the items): as it
    /// came, and a long one as it ended.
    fn placed(&self) -> u64 {
        match self.long() {
            true => self.call.settled_at.unwrap_or(self.call.id),
            false => self.call.id,
        }
    }

    /// Whether opening it shows something, its detail or its content.
    fn opens(&self) -> bool {
        !self.act.detail.is_empty() || self.call.content
    }
}

/// A run's calls in order, each with its act, or `None` for a call without
/// one, which shows as it is. `below` is the lowest ID of the steps from
/// each on ([`below`]).
fn slots<'a>(
    steps: &[Step<'a>],
    start: usize,
    end: usize,
    below: &[u64],
    names: &mut Names,
) -> Vec<Option<Member<'a>>> {
    let mut slots = Vec::new();
    for (index, step) in steps.iter().enumerate().take(end).skip(start) {
        let Step::Call(call) = *step else {
            continue;
        };
        let late = call
            .settled_at
            .is_some_and(|at| below.get(index + 1).is_some_and(|lowest| *lowest < at));
        slots.push(call.act.map(|act| Member {
            index,
            call,
            act,
            name: names.of(call.turn, act),
            late,
        }));
    }
    slots
}

/// The lowest ID of the steps from each of `steps` on, which tells whether
/// one came below a call before it ended.
fn below(steps: &[Step]) -> Vec<u64> {
    let mut lowest = vec![u64::MAX; steps.len() + 1];
    for (at, step) in steps.iter().enumerate().rev() {
        let id = match step {
            Step::Call(call) => call.id,
            Step::Thinking(id) | Step::Other(id) | Step::Aside(id) => *id,
        };
        lowest[at] = lowest[at + 1].min(id);
    }
    lowest
}

/// A row being made. What a joining call is checked against is kept as
/// calls join, so a row of many calls is made in time growing with them.
#[derive(Clone)]
struct Group<'a> {
    /// Its calls, in order.
    calls: Vec<Member<'a>>,
    /// It takes no more calls, as a row of a call of another kind or of a
    /// test run, or one a call without an act ended.
    alone: bool,
    /// The kind of all its calls, while they're of one kind.
    same: Option<ActKind>,
    /// The file all its calls read or change, while they all do and it's
    /// one file.
    file: Option<&'a str>,
    /// When the first of its calls that failed ended, if one did.
    failed: Option<u64>,
    /// Until when (by the IDs of the items) a long call of it ran, for ever
    /// while one runs. A call that took its place before then started a row
    /// of its own.
    busy: u64,
}

/// Whether a call of `kind` reads or changes a file.
fn of_a_file(kind: ActKind) -> bool {
    matches!(kind, ActKind::Read | ActKind::Write | ActKind::Edit)
}

impl<'a> Group<'a> {
    fn new(member: Member<'a>) -> Self {
        // (A row of a long call takes only what came after it ran, by
        // `busy`.)
        let mut group = Self {
            alone: member.act.kind == ActKind::Other || member.act.alone,
            calls: Vec::new(),
            same: Some(member.act.kind),
            file: of_a_file(member.act.kind).then_some(member.act.target.as_str()),
            failed: None,
            busy: 0,
        };
        group.push(member);
        group
    }

    /// Adds `member`, keeping what later calls are checked against.
    fn push(&mut self, member: Member<'a>) {
        if self.same != Some(member.act.kind) {
            self.same = None;
        }
        if !of_a_file(member.act.kind) || self.file != Some(member.act.target.as_str()) {
            self.file = None;
        }
        if member.failed()
            && let Some(ended) = member.call.ended
        {
            self.failed = Some(self.failed.map_or(ended, |first| first.min(ended)));
        }
        if member.long() {
            let until = match member.running() {
                true => u64::MAX,
                false => member.call.settled_at.unwrap_or(0),
            };
            self.busy = self.busy.max(until);
        }
        self.calls.push(member);
    }

    /// Its calls by step index, in order.
    fn members(&self) -> Vec<usize> {
        self.calls.iter().map(|member| member.index).collect()
    }

    /// The kind of all its calls, unless it holds the reads and changes of
    /// one file.
    fn kind(&self) -> Option<ActKind> {
        self.same
    }

    /// Whether `member` belongs in this row. A call joins as it comes, and a
    /// long one once it ended.
    fn takes(&self, member: &Member) -> bool {
        if self.alone || member.alone() {
            return false;
        }
        // A call the reader opened on a row of its own keeps that row.
        if member.call.sealed {
            return false;
        }
        // A call that took its place while a long call of the row ran has a
        // row of its own, as it had then.
        if member.placed() < self.busy {
            return false;
        }
        // A row of one call that the reader opened keeps its form.
        if self.calls.len() == 1 && self.calls[0].call.sealed {
            return false;
        }
        // A failure ends the row. A call that came while the failed one ran
        // stays in it.
        if self.failed.is_some_and(|ended| member.call.id >= ended) {
            return false;
        }
        let kind = member.act.kind;
        let target = member.act.target.as_str();
        if self.kind() == Some(kind) {
            // A job's checks and waits, and an MCP tool's calls, merge one
            // target only.
            let one_target = matches!(kind, ActKind::Check | ActKind::JobWait | ActKind::Mcp);
            return !one_target || self.calls[0].act.target == target;
        }
        // The reads and changes of one file are one thing to the reader.
        of_a_file(kind) && self.file == Some(target)
    }
}

/// The rows of a run's calls `slots`, each call joining the last row when
/// it belongs there, else starting a row.
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
        if let Some(last) = groups.last_mut()
            && last.takes(&member)
        {
            last.push(member);
        } else {
            groups.push(Group::new(member));
        }
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
    // (`calls` is in order, so a search finds the calls on either side.)
    let inside = |index: usize| {
        let at = calls.partition_point(|call| *call < index);
        let before = calls.get(at.checked_sub(1)?)?;
        let after = calls[at..].iter().find(|call| **call > index)?;
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
            Step::Thinking(_) => inside(index),
            Step::Other(_) | Step::Aside(_) => None,
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
            Step::Thinking(_) => inside(index).is_some(),
            Step::Other(_) | Step::Aside(_) => false,
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
        // (A job's look and wait have its whole ID, by which they merge.)
        if matches!(act.kind, ActKind::JobWait | ActKind::Check) {
            return job_name(&act.target);
        }
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

/// A job as the transcript names it, by the start of its ID `id` ("job
/// 4d2c1a9b").
pub fn job_name(id: &str) -> String {
    let id = id.trim();
    format!("job {}", id.get(..8).unwrap_or(id))
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

/// `items` once each, in order.
fn distinct<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| !item.is_empty() && seen.insert(*item))
        .map(str::to_string)
        .collect()
}

/// What a row of `group` says.
fn words(group: &Group) -> Words {
    let calls = &group.calls;
    let running = calls.iter().any(Member::running);
    // (A call that failed stays in its row, which shows one ✗, as does a
    // look or a wait that found its job failed.)
    let failed = calls.iter().any(Member::shows_failure);
    if calls.len() == 1 {
        let mut words = one(&calls[0]);
        words.running = running;
        return words;
    }
    let now = calls.iter().all(Member::running);
    let tense = |present: &str, past: &str| if now { present } else { past }.to_string();
    let count = calls.len();
    // The files `calls` name, up to two, else how many and their folder.
    let files_of = |calls: &[Member], noun: (&str, &str)| {
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
    let files = |noun: (&str, &str)| files_of(calls, noun);
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
        // The edits made, to the files they went in, else that none did.
        Some(ActKind::Edit) => {
            let made: Vec<Member> = calls
                .iter()
                .filter(|call| call.edits() > 0)
                .cloned()
                .collect();
            match made.as_slice() {
                [] => format!("Couldn’t edit {}", files(("file", "files"))),
                made => format!(
                    "{} {} to {}",
                    tense("Making", "Made"),
                    plural(edits(made), "edit", "edits"),
                    files_of(made, ("file", "files"))
                ),
            }
        }
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
            // (What came to each wait is on its line inside.)
            format!("{} for {who}", tense("Waiting", "Waited"))
        }
        // (What each wait found is on its line inside.)
        Some(ActKind::JobWait) => format!("{} for {}", tense("Waiting", "Waited"), calls[0].name),
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
        // (A call of another kind and a code run have rows of their own.)
        Some(ActKind::Other | ActKind::Code) => calls[0].act.done.clone(),
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

/// How many edits `calls` made ([`Member::edits`]).
fn edits(calls: &[Member]) -> usize {
    calls.iter().map(Member::edits).sum()
}

/// What a row of the reads and changes of one file says, each kind in
/// turn, the file named once ("Read app.rs and made 3 edits", "Read app.rs,
/// wrote it and made 1 edit"). The edits made are counted, and edits that
/// all failed say so ("Read app.rs and couldn’t edit it"). A file read or
/// written again is still one.
fn file_words(calls: &[Member], now: bool) -> String {
    let tense = |present: &str, past: &str| if now { present } else { past }.to_string();
    let name = calls[0].name.as_str();
    let has = |kind: ActKind| calls.iter().any(|call| call.act.kind == kind);
    let edits = edits(calls);
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
    } else if has(ActKind::Edit) {
        parts.push(match parts.is_empty() {
            true => format!("Couldn’t edit {name}"),
            false => "couldn’t edit it".to_string(),
        });
    }
    listed(&parts, 3)
}

/// What a worker's item is in its lead's view, which shows a worker only
/// as its reports and its messages to that lead (the main agent, or the
/// worker that started it). Each is a row of its own, a status and not a
/// call, that opens to its words. The rest of the worker's, its text, its
/// calls and its conversation with the user, is in its own view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkerRow {
    /// A report, the reply a run of the worker ended with.
    Reported,
    /// A message to the lead, as its call stands.
    Message(Sending),
}

/// How a worker's message to its lead stands, from its call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// What came while waits for the runs `waited` waited, in a few words
/// ("r91 finished, 3 updates"). `waited` are the runs the waits name, and
/// none when they wait for any of the agent's workers. The wait's own words
/// name those runs, so where the runs that finished (or stopped) are just
/// those, they aren't named again ("finished", "both finished", "all
/// finished").
pub fn came_words(came: &Came, waited: &[String]) -> String {
    let mut parts = Vec::new();
    let waited = distinct(waited.iter().map(|run| run.trim()));
    let ended = |runs: Vec<String>, verb: &str| match runs.len() {
        0 => None,
        count if count == waited.len() && runs.iter().all(|run| waited.contains(run)) => {
            Some(match count {
                1 => verb.to_string(),
                2 => format!("both {verb}"),
                _ => format!("all {verb}"),
            })
        }
        _ => Some(format!("{} {verb}", listed(&runs, 3))),
    };
    parts.extend(ended(
        distinct(came.finished.iter().map(String::as_str)),
        "finished",
    ));
    parts.extend(ended(
        distinct(came.stopped.iter().map(String::as_str)),
        "stopped",
    ));
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
            format!(
                "Made {} to {name}",
                plural(edits(std::slice::from_ref(member)), "edit", "edits")
            ),
            format!("edit {name}"),
        ),
        ActKind::Command => three(
            format!("Running {name}"),
            format!("Ran {name}"),
            format!("run {name}"),
        ),
        // An index search names its indexes, and its query shows once,
        // when it opens. A search of files (Claude Code's) names its
        // pattern.
        ActKind::Search => {
            let indexes: Vec<String> = indexes(name);
            let which = match indexes.as_slice() {
                [] if act.query.is_empty() => " the indexes".to_string(),
                [] => String::new(),
                [one] => format!(" the {one} index"),
                [a, b] => format!(" the {a} and {b} indexes"),
                many => format!(" {} indexes", many.len()),
            };
            let what = match act.query.as_str() {
                "" => which,
                query => format!("{which} for “{query}”"),
            };
            three(
                format!("Searching{what}"),
                format!("Searched{what}"),
                format!("search{what}"),
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
            let runs = waited_runs(name);
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
        ActKind::JobWait => three(
            format!("Waiting for {name}"),
            format!("Waited for {name}"),
            format!("wait for {name}"),
        ),
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
        // ("Ran code to count TODO comments per crate")
        ActKind::Code => {
            let to = if name.is_empty() {
                String::new()
            } else {
                format!(" to {name}")
            };
            three(
                format!("Running code{to}"),
                format!("Ran code{to}"),
                format!("run code{to}"),
            )
        }
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

/// The runs a wait's target names ("r3, r4"), none for a wait on any of
/// the agent's workers.
fn waited_runs(target: &str) -> Vec<String> {
    target
        .split(',')
        .map(str::trim)
        .filter(|run| !run.is_empty())
        .map(str::to_string)
        .collect()
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

/// What the row of `act`'s call alone says, with its `status` and
/// `outcome` ("Couldn’t read app.rs").
pub fn alone(act: &Act, status: CallStatus, outcome: Option<&str>) -> Words {
    let member = Member {
        index: 0,
        call: Call {
            act: Some(act),
            status: Some(status),
            outcome,
            ..Call::default()
        },
        act,
        name: Names::default().of(None, act),
        late: false,
    };
    one(&member)
}

/// What the calls of a code run that went well did, in one sentence, each
/// kind in the order it first came ("Listed 1 folder and read 31 files").
/// A file read, written or listed again counts once.
pub fn called(acts: &[Act]) -> String {
    let mut kinds: Vec<ActKind> = Vec::new();
    for act in acts {
        if !kinds.contains(&act.kind) {
            kinds.push(act.kind);
        }
    }
    let of = |kind: ActKind| acts.iter().filter(move |act| act.kind == kind);
    let files = |kind: ActKind| distinct(of(kind).map(|act| act.target.as_str())).len();
    let times = |kind: ActKind| of(kind).count();
    let parts: Vec<String> = kinds
        .into_iter()
        .map(|kind| match kind {
            ActKind::Read => format!("read {}", plural(files(kind), "file", "files")),
            ActKind::List => format!("listed {}", plural(files(kind), "folder", "folders")),
            ActKind::Write => format!("wrote {}", plural(files(kind), "file", "files")),
            ActKind::Edit => {
                let calls: Vec<Member> = of(kind)
                    .map(|act| Member {
                        index: 0,
                        call: Call::default(),
                        act,
                        name: String::new(),
                        late: false,
                    })
                    .collect();
                format!(
                    "made {} to {}",
                    plural(edits(&calls), "edit", "edits"),
                    plural(files(kind), "file", "files")
                )
            }
            ActKind::Command => format!("ran {}", plural(times(kind), "command", "commands")),
            ActKind::Search => format!("searched {}", plural(times(kind), "time", "times")),
            // (Another kind, such as a look at the indexes, by its words.)
            _ => {
                let mut done: Vec<String> = Vec::new();
                for act in of(kind) {
                    let words = lower_first(&alone(act, CallStatus::Ok, None).text);
                    if !done.contains(&words) {
                        done.push(words);
                    }
                }
                listed(&done, 2)
            }
        })
        .collect();
    upper_first(&listed(&parts, parts.len()))
}

fn upper_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// What the row of a code run says, which is what its code is for, and
/// while it runs how many calls the code made and the one that runs. A stop leaves
/// it in the past tense, since what the code did stands. One that failed
/// says it couldn't, as any call that failed does.
fn code_run(member: &Member, Phrases { doing, done, verb }: Phrases) -> Words {
    let calls = member.act.calls.as_ref();
    match member.call.status {
        Some(CallStatus::Running) | None => {
            let mut live = Vec::new();
            if let Some(calls) = calls.filter(|calls| calls.count > 0) {
                live.push(plural(calls.count as usize, "tool call", "tool calls"));
                if !calls.running.is_empty() {
                    live.push(lower_first(&calls.running));
                }
            }
            Words {
                text: doing,
                live,
                ..Words::default()
            }
        }
        Some(CallStatus::Failed) if member.call.outcome.map(str::trim) == Some(STOPPED) => Words {
            text: done,
            news: Some(STOPPED.into()),
            muted: true,
            ..Words::default()
        },
        // Held for a compaction, or the session ended first.
        Some(CallStatus::Failed) if !member.failed() => Words {
            text: doing,
            note: member
                .call
                .outcome
                .map(|outcome| outcome.trim().to_string()),
            muted: true,
            ..Words::default()
        },
        Some(CallStatus::Failed) => Words {
            text: format!("Couldn’t {verb}"),
            failed: true,
            ..Words::default()
        },
        Some(CallStatus::Ok) => Words {
            text: done,
            ..Words::default()
        },
    }
}

/// What the row of one call says.
fn one(member: &Member) -> Words {
    let act = member.act;
    let Phrases { doing, done, verb } = phrases(member);
    if act.kind == ActKind::Code {
        return code_run(member, Phrases { doing, done, verb });
    }
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
        // What came while it waited, what it found of its job, else its
        // news, and how its job is going.
        Some(CallStatus::Ok) => match &act.came {
            Some(came) if act.kind == ActKind::Wait => Words {
                text: done,
                note: Some(came_words(came, &waited_runs(&act.target))),
                ..Words::default()
            },
            _ if act.kind == ActKind::JobWait => Words {
                text: done,
                note: news,
                failed: found_failed(act),
                ..Words::default()
            },
            _ => Words {
                text: done,
                news,
                note: outcome.filter(|_| member.call.job).map(str::to_string),
                note_runs: member.call.job && act.job_runs,
                failed: found_failed(act),
                ..Words::default()
            },
        },
        _ => Words {
            text: doing,
            ..Words::default()
        },
    }
}

// ----- How a row's line fits its width -----
//
// Every client draws a row's line on one line and cuts it by one rule, in
// whatever unit it measures (the terminal's columns, the page's pixels).
// What never gives way stays, meaning a marker, a worker's name, ✓ or ✗, how
// long a call has run, the spinner and the buttons, each in the room its
// state keeps for it from the start. How a row runs shows only in the room
// its words leave ([`WORDS_KEEP_LIVE`]). A job's state while it runs gives
// way while the words have under [`WORDS_KEEP_RUNNING`] columns. What
// happened (a call's outcome, a row's news and note) gives way only when the
// words would have under [`WORDS_KEEP`], since it says more than the rest of
// what was done, such as a job's ID. Each part goes whole, the last first.
// The words take what's left, cut where a word ends. A file's line keeps
// its file's name in their place, and its folders give way before it. A row
// opens to what its line as drawn left out.

/// The columns a line's words keep before what happened gives way, enough
/// to say what was done ("Waited for job…", "$ cargo build…"), or all of
/// them where they're shorter.
pub const WORDS_KEEP: usize = 12;

/// The columns a line's words keep before a job's state gives way while
/// the job runs ("running", "monitoring").
pub const WORDS_KEEP_RUNNING: usize = 24;

/// What a line's words keep before how a row runs gives way (a code run's
/// count of calls and the call that runs), all of them. It shows only in
/// the room the words leave, so it never cuts them, and the line cuts its
/// words and opens the same as it changes with every call.
pub const WORDS_KEEP_LIVE: usize = usize::MAX;

/// The columns a call line's words keep before its outcome's words give
/// way, from its act. That says how its job runs while its job runs
/// ([`Act::job_runs`]), and else what happened.
pub fn outcome_keep(act: Option<&Act>) -> usize {
    match act.is_some_and(|act| act.job_runs) {
        true => WORDS_KEEP_RUNNING,
        false => WORDS_KEEP,
    }
}

/// The columns a Quiet row's words keep before its note gives way, the
/// same way ([`Words::note_runs`]).
pub fn note_keep(words: &Words) -> usize {
    match words.note_runs {
        true => WORDS_KEEP_RUNNING,
        false => WORDS_KEEP,
    }
}

/// A part of a row's line after its words, which gives way whole ([`fit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part {
    /// The room it takes, with the gap and the dot before it.
    pub width: usize,
    /// The room the words keep before it gives way ([`WORDS_KEEP`],
    /// [`WORDS_KEEP_RUNNING`], in the unit `width` is in).
    pub keep: usize,
}

/// How a row's line fits its room ([`fit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit {
    /// The room left for its words.
    pub words: usize,
    /// How many of the parts after its words show, from the first.
    pub parts: usize,
}

/// How a row's line fits `room`, from the width of its words, the width of
/// what never gives way (`fixed`), and the parts after its words (`parts`),
/// which give way the last first, each whole, once the words would have
/// less than the part keeps them (or less than all of them where they're
/// shorter).
pub fn fit(words: usize, fixed: usize, parts: &[Part], room: usize) -> Fit {
    let after = |shown: usize| parts[..shown].iter().map(|part| part.width).sum::<usize>();
    let mut shown = parts.len();
    if fixed + words + after(shown) > room {
        while shown > 0 && fixed + parts[shown - 1].keep.min(words) + after(shown) > room {
            shown -= 1;
        }
    }
    Fit {
        words: room.saturating_sub(fixed + after(shown)),
        parts: shown,
    }
}

/// How many of a line's words' `clusters` (its characters, each with its
/// width) show in `room`, which leaves out the ellipsis after them. The cut
/// falls where a word ends. A group in parentheses no wider than twice
/// `long` ("(7 more lines)") shows whole or not at all, and a wider one (a
/// long command's subshell) is cut at its words as any words are, though a
/// short group inside it still goes whole. Only a word wider than `long` (a
/// path, an ID, in the unit the widths are in) is cut inside, where the last
/// word that ends keeps less than half the room, and then a space before it
/// goes. So is a first word that doesn't fit, where nothing ends before it.
pub fn cut_at_word<S: AsRef<str>>(clusters: &[(S, usize)], room: usize, long: usize) -> usize {
    let space = |at: usize| clusters[at].0.as_ref().chars().all(char::is_whitespace);
    let group = whole_groups(clusters, long.saturating_mul(2));
    let mut used = 0;
    // The end of the last whole word that fits, and the room it takes.
    let mut word = (0, 0);
    let mut inside = clusters.len();
    for (at, (_, width)) in clusters.iter().enumerate() {
        if at > 0 && group[at].is_none() && space(at) && !space(at - 1) {
            word = (at, used);
        }
        if used + width > room {
            inside = at;
            break;
        }
        used += width;
    }
    if inside == clusters.len() {
        return inside;
    }
    // The width of the word the cut falls in, whole.
    let cut_word = || {
        let start = (0..inside)
            .rev()
            .find(|at| space(*at))
            .map_or(0, |at| at + 1);
        let end = (inside..clusters.len())
            .find(|at| space(*at))
            .unwrap_or(clusters.len());
        clusters[start..end]
            .iter()
            .map(|(_, width)| width)
            .sum::<usize>()
    };
    if word.0 > 0 && (word.1 * 2 >= room || cut_word() <= long) {
        return word.0;
    }
    // (Never inside a group that goes whole.)
    let mut kept = group[inside].unwrap_or(inside);
    while kept > 0 && space(kept - 1) {
        kept -= 1;
    }
    kept
}

/// Where the group in parentheses no wider than `widest` that each of
/// `clusters` is in starts, the outermost where one holds another.
/// Parentheses pair as written, in quotes or not, and one without a pair
/// starts or ends none.
fn whole_groups<S: AsRef<str>>(clusters: &[(S, usize)], widest: usize) -> Vec<Option<usize>> {
    let mut group = vec![None; clusters.len()];
    // Each open group's "(" and the width before it.
    let mut open = Vec::new();
    let mut used = 0;
    for (at, (cluster, width)) in clusters.iter().enumerate() {
        for c in cluster.as_ref().chars() {
            match c {
                '(' => open.push((at, used)),
                ')' => {
                    if let Some((start, before)) = open.pop()
                        && used + width - before <= widest
                    {
                        group[start..=at].fill(Some(start));
                    }
                }
                _ => {}
            }
        }
        used += width;
    }
    group
}

/// Where a path's `folders` (characters with their widths, each folder
/// ending in "/") start to show in `room`, cut where a folder starts so
/// whole folders show after the "…/" that stands for the rest (which `room`
/// leaves out). Their length when none fits.
pub fn folders_from<S: AsRef<str>>(folders: &[(S, usize)], room: usize) -> usize {
    let mut used = 0;
    let mut from = folders.len();
    while from > 0 {
        let mut start = from - 1;
        while start > 0 && !matches!(folders[start - 1].0.as_ref(), "/" | "\\") {
            start -= 1;
        }
        let width: usize = folders[start..from].iter().map(|(_, width)| width).sum();
        if used + width > room {
            break;
        }
        used += width;
        from = start;
    }
    from
}

/// How a file tool's line fits its room ([`file_fit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFit {
    /// Its outcome's words show.
    pub outcome: bool,
    /// The room its folders have. Under their width they show from where
    /// [`folders_from`] says, after "…/".
    pub folders: usize,
    /// The room its file's name has, with the rest of the line after it.
    pub name: usize,
}

/// How a file tool's line fits `room`, from the widths of what never gives
/// way (its verb, its ✓ or ✗ or how long it has run), its outcome's words,
/// its folders, its file's name, and the "…/" that stands for folders left
/// out (`elided`). The name keeps its room whole, after "…/" where it has
/// folders, and the outcome's words show only beside that. Then the folders
/// give way from their start, a folder at a time, and only then the name,
/// at its end.
pub fn file_fit(
    fixed: usize,
    outcome: usize,
    folders: usize,
    name: usize,
    elided: usize,
    room: usize,
) -> FileFit {
    if fixed + outcome + folders + name <= room {
        return FileFit {
            outcome: true,
            folders,
            name,
        };
    }
    let least = if folders > 0 { elided.min(folders) } else { 0 };
    let outcome_shows = outcome > 0 && fixed + least + name + outcome <= room;
    let left = room.saturating_sub(fixed + if outcome_shows { outcome } else { 0 });
    let folders = if folders + name <= left {
        folders
    } else {
        left.saturating_sub(name).clamp(least, folders)
    };
    FileFit {
        outcome: outcome_shows,
        folders,
        name: left.saturating_sub(folders),
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
            outcome: "2 edits",
            before: Some(
                "fn branched(&self, index: usize) -> Message {\n    let branch = &self.branches[index];\n    Message {\n        text: branch.text.clone(),\n        summary: branch.summary.clone(),\n        message_index: None,\n        at: branch.at,\n    }\n}\n",
            ),
            content: Some((
                Diff,
                " 3\t    Message {\n 4\t        text: branch.text.clone(),\n 5\t        summary: branch.summary.clone(),\n-6\t        message_index: None,\n+6\t        message_index: Some(index),\n-7\t        at: branch.at,\n+7\t        at: branch.at + 1,\n 8\t    }\n 9\t}",
            )),
            ..fixture(
                "an edit of two blocks",
                "apply_diff",
                r#"{"path": "src/engine.rs", "diff": "<<<<<<< SEARCH\n        message_index: None,\n=======\n        message_index: Some(index),\n>>>>>>> REPLACE\n<<<<<<< SEARCH\n        at: branch.at,\n=======\n        at: branch.at + 1,\n>>>>>>> REPLACE\n"}"#,
                "Edited src/engine.rs at lines 6 and 7:\n4 |         text: branch.text.clone(),\n5 |         summary: branch.summary.clone(),\n6 |         message_index: Some(index),\n7 |         at: branch.at + 1,\n8 |     }\n9 | }",
                Act {
                    edits: 2,
                    ..act(ActKind::Edit, "src/engine.rs")
                },
                "edit src/engine.rs",
                "Made 2 edits to engine.rs",
            )
        },
        Fixture {
            outcome: "3 edits",
            content: Some((Diff, "-\tlet a = 1;\n+\tlet b = 1;")),
            ..fixture(
                "an edit of every match",
                "edit_file",
                r#"{"path": "src/a.rs", "old_string": "let a = 1;", "new_string": "let b = 1;", "replace_all": true}"#,
                "Replaced 3 occurrences in src/a.rs, at lines 3, 7 and 12.",
                Act {
                    edits: 3,
                    ..act(ActKind::Edit, "src/a.rs")
                },
                "edit src/a.rs (all)",
                "Made 3 edits to a.rs",
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
                    job_runs: true,
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
                "(in src/specific/work) $ printf one  (1 more line)",
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
                Act {
                    job_runs: true,
                    ..command("cargo build")
                },
                "$ cargo build",
                "Ran cargo build · in the background",
            )
        },
        Fixture {
            outcome: "2 results from arko (code, hybrid)",
            content: Some((
                Plain,
                "src/engine.rs:96-105 · fn branched\nsrc/sessions.rs:412-440 · fn restore_branch",
            )),
            ..fixture(
                "a search",
                "index_search",
                r#"{"index": "arko", "query": "branched message index"}"#,
                "2 results from arko (code, hybrid):\n\nsrc/engine.rs:96-105 · fn branched\n96 | fn branched() {\n97 | }\nsrc/sessions.rs:412-440 · fn restore_branch (part of a 200-line definition at lines 400-600: read_file shows the rest)\n412 | fn restore_branch() {",
                Act {
                    detail: "branched message index".into(),
                    ..act(ActKind::Search, "arko")
                },
                "index search “branched message index”",
                "Searched the arko index",
            )
        },
        Fixture {
            outcome: "1 result from arko (code, hybrid)",
            full_detail: None,
            content: Some((
                Plain,
                "src/remote/quiet.rs:138-155 · fn rows",
            )),
            ..fixture(
                "a search past the row's length",
                "index_search",
                r#"{"index": "arko", "query": "how does the feed decide where a long call goes when its row is the last one"}"#,
                "1 result from arko (code, hybrid):\nsrc/remote/quiet.rs:138-155 · fn rows\n    pub fn rows(steps: &[Step]) -> Vec<Piece> {",
                Act {
                    detail: "how does the feed decide where a long call goes when its row is the last one".into(),
                    ..act(ActKind::Search, "arko")
                },
                "index search “how does the feed decide where a long call goes when its row is the last one”",
                "Searched the arko index",
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
                "# Prose\n\nWrite the way a person explains something to a colleague beside them.",
            )),
            ..fixture(
                "a skill",
                "load_skill",
                r#"{"name": "prose"}"#,
                "--- BEGIN SKILL: prose ---\n# Prose\n\nWrite the way a person explains something to a colleague beside them.\n--- END SKILL: prose ---",
                act(ActKind::Skill, "prose"),
                "skill prose",
                "Loaded the prose skill",
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

    /// A turn's count stands at the thought of its turn that led to its
    /// first call with nothing shown between, else at that call.
    #[test]
    fn a_turns_count_stands_at_a_thought_of_its_own_turn() {
        let call = |id, turn, status| Seen::Call {
            id,
            turn: Some(turn),
            status: Some(status),
        };
        let thought = |id, turn| Seen::Thinking {
            id,
            turn: Some(turn),
        };
        let places = |seen: Vec<Seen>| -> Vec<(Option<u64>, u64, u64, usize)> {
            counts(seen)
                .into_iter()
                .map(|count| (count.turn, count.first, count.at, count.calls))
                .collect()
        };
        // A turn that ended in a thought, and one for news that calls at once.
        assert_eq!(
            places(vec![
                Seen::Shown,
                thought(2, 1),
                Seen::Hidden,
                call(4, 2, CallStatus::Ok)
            ]),
            vec![(Some(2), 4, 4, 1)]
        );
        // The first thought of the turn since the last thing shown leads.
        assert_eq!(
            places(vec![
                thought(1, 1),
                thought(2, 2),
                Seen::Hidden,
                thought(4, 2),
                call(5, 2, CallStatus::Ok),
                Seen::Shown,
                thought(7, 2),
                call(8, 2, CallStatus::Failed),
            ]),
            vec![(Some(2), 5, 2, 2)]
        );
        // Something shown between them ends the thought's lead.
        assert_eq!(
            places(vec![
                thought(1, 1),
                Seen::Shown,
                call(3, 1, CallStatus::Running)
            ]),
            vec![(Some(1), 3, 3, 1)]
        );
        let tally = counts(vec![
            call(1, 1, CallStatus::Failed),
            call(2, 1, CallStatus::Running),
            call(3, 1, CallStatus::Ok),
        ]);
        assert_eq!(
            (tally[0].calls, tally[0].failed, tally[0].running),
            (3, 1, 1)
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
                None => Step::Thinking(index as u64 + 1),
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

    use ActKind::{Command, Edit, JobWait, Message, Read, Wait, Write};
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
            settled_at: None,
            sealed: false,
        })];
        match rows(&steps).as_slice() {
            [Piece::Row(row)] => row.clone(),
            other => panic!("{}: {other:?}", fixture.name),
        }
    }

    /// A row counts only the edits its calls made, so a call that failed
    /// adds none, and names only the files they went in. A row whose edits
    /// all failed counts none and names its files. Either way it says a call
    /// failed.
    #[test]
    fn a_row_counts_the_edits_made() {
        let number = |text: &str| {
            text.split_whitespace()
                .find_map(|word| word.parse::<usize>().ok())
        };
        let two = |target: &str| Act {
            edits: 2,
            ..act(Edit, target)
        };
        let failed = |act: Act| Some((act, Failed));
        /// A row's calls, the edits they made, and the files its words
        /// name and don't.
        struct Case {
            calls: Vec<Option<(Act, CallStatus)>>,
            made: Option<usize>,
            named: &'static [&'static str],
            unnamed: &'static [&'static str],
        }
        let cases = [
            Case {
                calls: vec![Some((two("a.rs"), Ok)), failed(two("a.rs"))],
                made: Some(2),
                named: &["a.rs"],
                unnamed: &[],
            },
            Case {
                calls: vec![
                    ok(Edit, "a.rs"),
                    failed(two("b.rs")),
                    Some((two("c.rs"), Ok)),
                ],
                made: Some(3),
                named: &["a.rs", "c.rs"],
                unnamed: &["b.rs"],
            },
            Case {
                calls: vec![failed(two("a.rs")), failed(act(Edit, "a.rs"))],
                made: None,
                named: &["a.rs"],
                unnamed: &[],
            },
            Case {
                calls: vec![failed(act(Edit, "a.rs")), failed(act(Edit, "b.rs"))],
                made: None,
                named: &["a.rs", "b.rs"],
                unnamed: &[],
            },
            Case {
                calls: vec![ok(Read, "a.rs"), failed(two("a.rs"))],
                made: None,
                named: &["a.rs"],
                unnamed: &[],
            },
        ];
        for Case {
            calls,
            made,
            named,
            unnamed,
        } in cases
        {
            let steps: Vec<Step> = calls
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    let (act, status) = call.as_ref().unwrap();
                    Step::Call(Call {
                        id: index as u64 + 1,
                        act: Some(act),
                        status: Some(*status),
                        turn: Some(1),
                        ..Call::default()
                    })
                })
                .collect();
            let pieces = rows(&steps);
            let [Piece::Row(row)] = pieces.as_slice() else {
                panic!("{pieces:?}");
            };
            let text = &row.words.text;
            assert_eq!(number(text), made, "{text}");
            assert!(row.words.failed, "{text}");
            assert!(named.iter().all(|name| text.contains(name)), "{text}");
            assert!(!unnamed.iter().any(|name| text.contains(name)), "{text}");
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
        let failing = shown(&[
            ok(Edit, "a.rs"),
            Some((act(Edit, "a.rs"), Failed)),
            ok(Edit, "a.rs"),
        ]);
        let [row] = failing.as_slice() else {
            panic!("{failing:?}");
        };
        assert!(row.ends_with('✗'), "{row}");
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
                    None => Step::Thinking(2),
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

    /// Quick calls join the row above as they come, and a long call once it
    /// ended. While it runs it has a row of its own, which says what it
    /// does, and so does each of two that run at once. A row of calls that
    /// all run speaks in the present.
    #[test]
    fn running_calls() {
        let build = act(Command, "cargo build");
        assert_eq!(
            shown(&[ok(Command, "rg x"), Some((build.clone(), Running))]),
            ["Ran rg x".to_string(), doing(&build)]
        );
        let npm = act(Command, "npm test");
        assert_eq!(
            shown(&[
                ok(Command, "rg x"),
                Some((build.clone(), Running)),
                Some((npm.clone(), Running)),
            ]),
            ["Ran rg x".to_string(), doing(&build), doing(&npm)]
        );
        // Once both ended they are in the row before them, whichever ended
        // first.
        assert_eq!(
            shown(&[
                ok(Command, "rg x"),
                Some((build.clone(), Running)),
                Some((npm.clone(), Ok)),
            ])
            .len(),
            3
        );
        assert_eq!(
            shown(&[
                ok(Command, "rg x"),
                ok(Command, "cargo build"),
                ok(Command, "npm test")
            ])
            .len(),
            1
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

    /// A long call the reader opened on its row while it ran keeps that row
    /// once it ends, and the calls after it start rows of their own.
    #[test]
    fn an_opened_running_call_keeps_its_row() {
        let (rg, build, ls) = (
            act(Command, "rg x"),
            act(Command, "cargo build"),
            act(Command, "ls"),
        );
        let call = |id: u64, act, status, sealed| {
            Step::Call(Call {
                id,
                act: Some(act),
                status: Some(status),
                sealed,
                ..Call::default()
            })
        };
        let opened = [
            call(1, &rg, Ok, false),
            call(2, &build, Ok, true),
            call(3, &ls, Ok, false),
        ];
        let members: Vec<Vec<usize>> = rows(&opened)
            .into_iter()
            .filter_map(|piece| match piece {
                Piece::Row(row) => Some(row.members),
                Piece::Step(_) => None,
            })
            .collect();
        assert_eq!(members, [vec![0], vec![1], vec![2]]);
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
    /// its calls once anything shows after it. A long call that runs has a
    /// row of its own, and joins the row before it once it ends, however it
    /// ended.
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
            ["Ran rg x", doing(&build).as_str(), "Read a.rs"]
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
        let waiting = [call(1, &rg, Ok), call(2, &build, Ok), Step::Aside(3)];
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
    /// ones with more steps or later results. Rows only come at the end, and
    /// a row once shown keeps its key, but for the last row, of a long call
    /// that ran, which goes into the row before it once the call ended.
    fn assert_rows_only_grow(snapshots: &[Vec<Step>]) {
        fn state<'a>(steps: &[Step<'a>], id: u64) -> Option<(Option<CallStatus>, Option<&'a Act>)> {
            steps.iter().find_map(|step| match step {
                Step::Call(call) if call.id == id => Some((call.status, call.act)),
                _ => None,
            })
        }
        for pair in snapshots.windows(2) {
            let (before, after) = (keys(&pair[0]), keys(&pair[1]));
            // (The first row whose long call ran and has ended since.)
            let ended = before.iter().position(|key| {
                let id = key.strip_prefix('q').and_then(|id| id.parse().ok());
                let Some(id) = id else {
                    return false;
                };
                let ran = matches!(
                    state(&pair[0], id),
                    Some((Some(Running), Some(act)))
                        if !act.alone && matches!(act.kind, Command | Wait | JobWait | ActKind::Mcp | ActKind::Check)
                );
                ran && !matches!(state(&pair[1], id), Some((Some(Running), _)))
            });
            // (Only the last row goes, with the thinking before it, into the
            // row it joins.)
            if let Some(at) = ended
                && !after.contains(&before[at])
            {
                assert!(
                    !before[at + 1..].iter().any(|key| key.starts_with('q')),
                    "{before:?} then {after:?}"
                );
            }
            let kept = match ended {
                Some(at) => {
                    let joined = before[..at].iter().rposition(|key| key.starts_with('q'));
                    &before[..joined.map_or(at, |joined| joined + 1)]
                }
                None => &before[..],
            };
            assert!(
                after.starts_with(kept),
                "{before:?} then {after:?}\n{:#?}",
                said(&pair[1])
            );
        }
    }

    /// No row splits, goes or takes another's calls as calls start, run,
    /// end, fail, go on in the background or end out of order, but for the
    /// row of a long call that ran.
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
            vec![call(1, &rg, Ok), Step::Thinking(2)],
            vec![
                call(1, &rg, Ok),
                Step::Thinking(2),
                call(3, &build, Running),
            ],
            vec![call(1, &rg, Ok), Step::Thinking(2), call(3, &build, Ok)],
        ];
        assert_eq!(
            thinking.iter().map(|steps| keys(steps)).collect::<Vec<_>>(),
            [
                vec!["q1"],
                vec!["q1", "s1"],
                vec!["q1", "s1", "q3"],
                vec!["q1"]
            ]
        );
        assert_rows_only_grow(&thinking[2..]);
        // A long call that ran while more calls came below it stays where it
        // is once it ends, and the row below it keeps its calls.
        let ran = |id: u64, status, settled_at| {
            Step::Call(Call {
                id,
                act: Some(&build),
                status: Some(status),
                turn: Some(1),
                settled_at,
                ..Call::default()
            })
        };
        let below = [
            vec![call(1, &rg, Ok), ran(2, Running, None)],
            vec![call(1, &rg, Ok), ran(2, Running, None), call(3, &rg, Ok)],
            vec![
                call(1, &rg, Ok),
                ran(2, Running, None),
                call(3, &rg, Ok),
                call(4, &rg, Ok),
            ],
            vec![
                call(1, &rg, Ok),
                ran(2, Ok, Some(5)),
                call(3, &rg, Ok),
                call(4, &rg, Ok),
            ],
        ];
        assert_rows_only_grow(&below);
        let members = |steps: &[Step]| -> Vec<Vec<usize>> {
            rows(steps)
                .into_iter()
                .filter_map(|piece| match piece {
                    Piece::Row(row) => Some(row.members),
                    Piece::Step(_) => None,
                })
                .collect()
        };
        assert_eq!(members(&below[3]), [vec![0], vec![1], vec![2, 3]]);
        // A thinking or a note that came below it while it ran keeps it
        // where it is too, also once the note is read (by the ID it waited
        // as), and one that came after it ended doesn't.
        for (later, settled_at, rows) in [
            (Step::Thinking(3), 4, ["q1", "q2", "s2"].as_slice()),
            (Step::Aside(3), 4, &["q1", "q2", "s2"]),
            (Step::Other(3), 4, &["q1", "q2", "s2"]),
            (Step::Aside(3), 3, &["q1", "s2"]),
        ] {
            let steps = [call(1, &rg, Ok), ran(2, Ok, Some(settled_at)), later];
            assert_eq!(keys(&steps), rows, "{later:?} {settled_at}");
        }
        // One that ended while it was last joins the row before it, and the
        // calls after it join that row as they come.
        let last = [
            call(1, &rg, Ok),
            ran(2, Ok, Some(3)),
            call(3, &rg, Ok),
            call(4, &rg, Ok),
        ];
        assert_eq!(members(&last), [vec![0, 1, 2, 3]]);
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

    /// Back-to-back waits are one row, which says whom they waited for.
    /// What came to each is on its own line inside, and a wait alone says
    /// it on its row.
    #[test]
    fn a_wait_names_its_runs_once() {
        let runs = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        let came = |finished: &[&str], stopped: &[&str]| Came {
            finished: runs(finished),
            stopped: runs(stopped),
            ..Came::default()
        };
        // Every run the wait names finished or stopped, which its own words
        // already name, so what came names none of them again.
        for (finished, stopped, waited) in [
            (&["r4"][..], &[][..], &["r4"][..]),
            (&["r4", "r3"], &[], &["r3", "r4"]),
            (&["r2", "r3", "r4"], &[], &["r2", "r3", "r4"]),
            (&[], &["r4"], &["r4"]),
        ] {
            let words = came_words(&came(finished, stopped), &runs(waited));
            assert!(waited.iter().all(|run| !words.contains(run)), "{words}");
            assert_ne!(words, came_words(&Came::default(), &runs(waited)));
        }
        // Some of them, another run, or what came to a wait on any worker,
        // which its words don't name, are named.
        for (finished, waited, named) in [
            (&["r3"][..], &["r3", "r4"][..], "r3"),
            (&["r91"], &["r3"], "r91"),
            (&["r3"], &[], "r3"),
        ] {
            let words = came_words(&came(finished, &[]), &runs(waited));
            assert!(words.contains(named), "{words}");
        }
        // The row says each run once.
        let wait = Act {
            kind: Wait,
            target: "r4".into(),
            came: Some(came(&["r4"], &[])),
            ..Act::default()
        };
        let said = alone(&wait, CallStatus::Ok, None).said();
        assert_eq!(said.matches("r4").count(), 1, "{said}");
    }

    /// The mark on a call's line says how the call went, and for a look at
    /// a job or a wait on it, what it found of the job, which its Quiet row's
    /// ✗ says too.
    #[test]
    fn the_mark_follows_what_a_look_or_a_wait_found() {
        let found = |kind, found| Act {
            kind,
            target: "4d2c1a9b-0000".into(),
            found,
            ..Act::default()
        };
        use CallStatus::{Failed, Ok, Running};
        for (kind, finding, status, expected) in [
            (JobWait, None, Ok, Some(Ok)),
            (JobWait, Some(JobFound::Running), Ok, Some(Ok)),
            (JobWait, Some(JobFound::Finished), Ok, Some(Ok)),
            (JobWait, Some(JobFound::Failed), Ok, Some(Failed)),
            (ActKind::Check, Some(JobFound::Failed), Ok, Some(Failed)),
            (JobWait, Some(JobFound::Input), Ok, None),
            (JobWait, Some(JobFound::Stopped), Ok, None),
            (JobWait, Some(JobFound::Finished), Failed, Some(Failed)),
            (JobWait, None, Running, None),
            (Read, None, Failed, Some(Failed)),
        ] {
            let act = found(kind, finding);
            assert_eq!(
                mark(Some(status), Some(&act)),
                mark_of(status, finding),
                "{kind:?} {finding:?} {status:?}"
            );
            assert_eq!(
                mark(Some(status), Some(&act)),
                expected,
                "{kind:?} {finding:?}"
            );
            if status == Ok {
                let words = alone(&act, status, None);
                assert_eq!(
                    words.failed,
                    expected == Some(Failed),
                    "{kind:?} {finding:?}"
                );
            }
        }
        assert_eq!(mark(None, None), None);
    }

    /// What [`mark`] gives, as the table above reads.
    fn mark_of(status: CallStatus, found: Option<JobFound>) -> Option<CallStatus> {
        match (status, found) {
            (CallStatus::Running, _) => None,
            (CallStatus::Ok, Some(JobFound::Failed)) => Some(CallStatus::Failed),
            (CallStatus::Ok, Some(JobFound::Input | JobFound::Stopped)) => None,
            (status, _) => Some(status),
        }
    }

    /// A code run that failed says it couldn't do what its code is for, as
    /// any call that failed does, not what one that went well says, and one
    /// that a stop cut short says what it did.
    #[test]
    fn a_failed_code_run_says_it_couldnt() {
        let purpose = "rename the setting in every crate";
        let code = act(ActKind::Code, purpose);
        let done = alone(&code, CallStatus::Ok, None).text;
        let failed = alone(&code, CallStatus::Failed, Some(FAILED));
        assert!(failed.failed);
        assert!(failed.text.ends_with(purpose), "{}", failed.text);
        assert_ne!(failed.text, done);
        assert_eq!(
            failed.text,
            alone(&act(ActKind::Read, "a.rs"), CallStatus::Failed, None)
                .text
                .replace("read a.rs", &phrases_of(&code).verb)
        );
        let stopped = alone(&code, CallStatus::Failed, Some(STOPPED));
        assert_eq!(stopped.text, done);
        assert!(!stopped.failed);
    }

    /// A call's words after "Couldn’t", as its row would say them.
    fn phrases_of(act: &Act) -> Phrases {
        phrases(&Member {
            index: 0,
            call: Call {
                act: Some(act),
                ..Call::default()
            },
            act,
            name: Names::default().of(None, act),
            late: false,
        })
    }

    /// A line's words keep their start, the parts after them give way whole,
    /// the last first, each when the words would keep less than it lets
    /// them, and then the words are cut where a word ends.
    #[test]
    fn a_line_gives_way_its_parts_whole_then_its_words_at_a_word() {
        let part = |width, keep| Part { width, keep };
        let parts = [part(8, 24), part(6, 24)];
        // All of it fits.
        assert_eq!(
            fit(20, 4, &parts, 40),
            Fit {
                words: 22,
                parts: 2
            }
        );
        // The parts stay while the words keep 24, which take the rest.
        assert_eq!(
            fit(30, 0, &parts, 40),
            Fit {
                words: 26,
                parts: 2
            }
        );
        // Then the last part goes first, whole.
        assert_eq!(
            fit(30, 0, &parts, 37),
            Fit {
                words: 29,
                parts: 1
            }
        );
        assert_eq!(
            fit(30, 0, &parts, 31),
            Fit {
                words: 31,
                parts: 0
            }
        );
        // Words shorter than 24 keep only their own width.
        assert_eq!(
            fit(10, 0, &[part(8, 24)], 18),
            Fit {
                words: 10,
                parts: 1
            }
        );
        // What never gives way stays, and the words take what's left.
        assert_eq!(
            fit(30, 10, &[part(8, 24)], 20),
            Fit {
                words: 10,
                parts: 0
            }
        );
        // What happened (keep 12) stays after how it runs (keep 24) went,
        // until the words would keep less than 12.
        let parts = [part(16, WORDS_KEEP), part(8, WORDS_KEEP_RUNNING)];
        assert_eq!(
            fit(23, 0, &parts, 40),
            Fit {
                words: 24,
                parts: 1
            }
        );
        assert_eq!(
            fit(23, 0, &parts, 28),
            Fit {
                words: 12,
                parts: 1
            }
        );
        assert_eq!(
            fit(23, 0, &parts, 27),
            Fit {
                words: 27,
                parts: 0
            }
        );
        let cut = |text: &str, room: usize| {
            let clusters: Vec<(String, usize)> = text.chars().map(|c| (c.to_string(), 1)).collect();
            text.chars()
                .take(cut_at_word(&clusters, room, WORDS_KEEP))
                .collect::<String>()
        };
        // At the end of a word, without the space after it.
        assert_eq!(cut("Waiting for job c876812b", 9), "Waiting");
        // A word that fits the room the words keep is never cut inside, even
        // where the word before it keeps less than half the room.
        assert_eq!(cut("Started migrate in a terminal", 14), "Started");
        // A wider one is, a path, an ID or a long word.
        assert_eq!(
            cut("Ran ./target/debug/arko-tui-something", 30),
            "Ran ./target/debug/arko-tui-so"
        );
        assert_eq!(
            cut("Ran internationalization checks", 16),
            "Ran internationa"
        );
        assert_eq!(
            cut("Running cargo build --release -p arko", 22),
            "Running cargo build"
        );
        // A group in parentheses of up to 24 columns goes whole, even where
        // nothing comes before it.
        let heredoc = "$ cat > docs/release-notes.md <<'EOF'  (7 more lines)";
        assert_eq!(cut(heredoc, 48), "$ cat > docs/release-notes.md <<'EOF'");
        assert_eq!(cut("$ cat (7 more lines)", 13), "$ cat");
        assert_eq!(cut("(7 more lines)", 8), "");
        assert_eq!(cut("x (a (b c) d) y", 9), "x");
        // A wider one, a subshell, is cut at its words, though a short group
        // inside it still goes whole.
        let subshell = "Ran (git worktree list | grep -q row-m0-render || git worktree add -q --detach ../wt/row-m0-render 4f7a2c1e) && git worktree list | grep row-m0-render in demo_kit";
        assert_eq!(cut(subshell, 30), "Ran (git worktree list | grep");
        assert_eq!(
            cut(subshell, 100),
            "Ran (git worktree list | grep -q row-m0-render || git worktree add -q --detach ../wt/row-m0-render"
        );
        let nested = "Ran (cd web && (npm ci --silent) && npm run build -- --mode production)";
        assert_eq!(cut(nested, 25), "Ran (cd web &&");
        assert_eq!(cut(nested, 38), "Ran (cd web && (npm ci --silent) &&");
        // So the words keep as much as fits at any width, at most the room a
        // word takes short of it, and more as the room grows.
        let words: Vec<&str> = subshell.split(' ').collect();
        let widest = words.iter().map(|word| word.len()).max().unwrap();
        let mut last = 0;
        for room in 4..subshell.len() {
            let kept = cut(subshell, room).len();
            assert!(room - kept <= widest + 1, "{room}: {}", cut(subshell, room));
            assert!(kept >= last, "{room}: {}", cut(subshell, room));
            last = kept;
        }
        // A "(" or ")" without its pair (in quotes) is no group.
        let quoted = "Ran grep -c '(' notes.md && git status --short";
        assert_eq!(cut(quoted, 30), "Ran grep -c '(' notes.md &&");
        let quoted = "Ran grep -c ')' notes.md && git status --short";
        assert_eq!(cut(quoted, 30), "Ran grep -c ')' notes.md &&");
        // Inside a word only where the last that ends keeps under half.
        assert_eq!(
            cut("$ ./target/release/arko-something", 19),
            "$ ./target/release/"
        );
        assert_eq!(cut("abcdefghij", 4), "abcd");
        assert_eq!(cut("short", 9), "short");
    }

    /// What a call opens to holds its call as written where its detail or
    /// content holds what the call says past its verb and target, whatever
    /// its tool, and not where the call says more.
    #[test]
    fn what_a_call_opens_to_holds_its_call_as_written() {
        let body = "On activation reflow, apply the rule to every row kind.\n\nCheck it through every surface.";
        let cut = format!("message r5 “{}…”", &one_line(body)[..60]);
        let quoted =
            "Two things.\n\n1. The \"Ran\" row of C:\\Users\\ada\\notes.md cuts at “a word”.";
        let quoted_line = format!("message r5 “{}”", one_line(quoted));
        let held = [
            (cut.as_str(), body),
            (quoted_line.as_str(), quoted),
            (
                "start worker (opus) “Check the docs' links”",
                "Check the docs' links",
            ),
            (
                "index search “branched message index”",
                "branched message index",
            ),
            (
                "$ cd web && cargo build --release",
                "cd web && cargo build --release",
            ),
            (
                "(in src/work) $ printf one  (1 more line)",
                "printf one\nprintf two",
            ),
            // (A tool Arko doesn't know, by its line's form alone.)
            (
                "publish post “Release notes for 0.24”",
                "Release notes for 0.24\n",
            ),
            (
                "archive notes/2026/november.md",
                "Archived notes/2026/november.md",
            ),
        ];
        for (line, detail) in held {
            assert!(holds_call(line, &["", detail]), "{line}");
        }
        let not = [
            // A near match, part of it, or more than it.
            (
                cut.as_str(),
                "On activation reflow, apply the rule to every row.",
            ),
            ("grep “TODO” in src/remote", "TODO"),
            ("$ cargo build --release", "cargo build\nCompiling arko"),
            ("publish post “Release notes for 0.24”", "Release notes"),
            (
                "archive notes/2026/november.md --keep",
                "Archived notes/2026/november.md",
            ),
            // (A quote or a "$ " inside a path starts nothing.)
            ("archive notes/“draft”.md", "Archived other/“draft”.md"),
            ("archive notes/a$ b.md", "Archived other/a$ b.md"),
            (
                "create_issue (github) {\"title\":\"Rows\"}",
                "{\n  \"title\": \"Rows\"\n}",
            ),
            (
                "read /home/ada/notes/NOTES.md:120-139",
                "120\t- Quiet rows carry no ✓.",
            ),
            ("message r5 “Push it.”", ""),
            ("list", "list"),
        ];
        for (line, detail) in not {
            assert!(!holds_call(line, &[detail]), "{line}");
        }
    }

    /// An open row or call shows each thing its line left out once, its
    /// words whole where the line cut them and what happened where the line
    /// left it out, and nothing its line shows.
    #[test]
    fn what_a_line_left_out_shows_once() {
        let words = Words {
            text: "Waited for job 4d2c1a9b".into(),
            news: Some("1 failed".into()),
            note: Some("failed (exit 1)".into()),
            ..Words::default()
        };
        // Its words cut and every part left out.
        assert_eq!(
            words.left_out(Some(&words.text), 0).as_deref(),
            Some("Waited for job 4d2c1a9b (1 failed) · failed (exit 1)")
        );
        // Its words whole and its note left out, the note alone.
        assert_eq!(words.left_out(None, 1).as_deref(), Some("failed (exit 1)"));
        assert_eq!(
            words.left_out(None, 0).as_deref(),
            Some("1 failed · failed (exit 1)")
        );
        // Its words cut and every part shown, the words.
        assert_eq!(words.left_out(Some("x y"), 2).as_deref(), Some("x y"));
        assert_eq!(words.left_out(None, 2), None);
        // A call, by the same rule, never with its mark.
        assert_eq!(
            call_left_out(Some("$ cargo build --release"), Some("41.0 s")).as_deref(),
            Some("$ cargo build --release · 41.0 s")
        );
        assert_eq!(
            call_left_out(None, Some("failed (exit 1)")).as_deref(),
            Some("failed (exit 1)")
        );
        assert_eq!(call_left_out(Some("$ ls"), None).as_deref(), Some("$ ls"));
        assert_eq!(call_left_out(None, None), None);
        // What a call says once it ended. One that failed says so.
        assert_eq!(ended(Some(CallStatus::Failed), None), Some(FAILED));
        assert_eq!(
            ended(Some(CallStatus::Ok), Some(" 3 lines ")),
            Some("3 lines")
        );
        assert_eq!(ended(Some(CallStatus::Ok), Some(" ")), None);
        assert_eq!(ended(Some(CallStatus::Running), Some("x")), None);
    }

    /// A path's folders give way from their start, a whole folder at a time.
    #[test]
    fn folders_give_way_a_folder_at_a_time() {
        let clusters = |text: &str| -> Vec<(String, usize)> {
            text.chars().map(|c| (c.to_string(), 1)).collect()
        };
        let folders = "path/results/shortening/";
        let kept = |room| {
            folders
                .chars()
                .skip(folders_from(&clusters(folders), room))
                .collect::<String>()
        };
        assert_eq!(kept(24), folders);
        assert_eq!(kept(20), "results/shortening/");
        assert_eq!(kept(18), "shortening/");
        assert_eq!(kept(10), "");
        assert_eq!(
            "a\\b\\"
                .chars()
                .skip(folders_from(&clusters("a\\b\\"), 2))
                .collect::<String>(),
            "b\\"
        );
    }

    /// A file's line keeps its name whole. Its outcome's words show only
    /// beside the name and "…/", its folders give way next, and the name
    /// only after them.
    #[test]
    fn a_file_line_keeps_its_name_and_its_outcome_beside_it() {
        // Verb and mark 9, outcome 7, folders 24, name 31, "…/" 2.
        let parts = |room| file_fit(9, 7, 24, 31, 2, room);
        assert_eq!(
            parts(71),
            FileFit {
                outcome: true,
                folders: 24,
                name: 31
            }
        );
        // The folders give way first, down to "…/", the outcome stays.
        assert_eq!(
            parts(58),
            FileFit {
                outcome: true,
                folders: 11,
                name: 31
            }
        );
        assert_eq!(
            parts(49),
            FileFit {
                outcome: true,
                folders: 2,
                name: 31
            }
        );
        // Then the outcome goes whole, which leaves the folders more.
        assert_eq!(
            parts(48),
            FileFit {
                outcome: false,
                folders: 8,
                name: 31
            }
        );
        // And the name gives way last.
        assert_eq!(
            parts(30),
            FileFit {
                outcome: false,
                folders: 2,
                name: 19
            }
        );
        for room in 0..80 {
            let fit = parts(room);
            if room >= 9 + 2 + 31 {
                assert!(
                    9 + 7 * usize::from(fit.outcome) + fit.folders + fit.name <= room.max(9 + 2)
                );
                assert_eq!(fit.name, 31.max(fit.name), "{room}: {fit:?}");
            }
        }
    }

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
            ["Waited for workers"]
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
            ["Waited for workers"]
        );
        // One that waits has a row of its own, which says so.
        assert_eq!(
            shown(&[
                Some((wait("r3", update.clone()), Ok)),
                Some((act(Wait, "r3"), Running))
            ])[1],
            doing(&act(Wait, "r3"))
        );
    }

    /// A wait on a job merges with waits on the same job only, by its whole
    /// ID, and never with a wait for workers. Alone it says what it found,
    /// and a row of several says that only on each wait's own line.
    #[test]
    fn waits_on_a_job_merge_by_the_job() {
        let job = |id: &str, news: &str| Act {
            news: news.into(),
            ..act(JobWait, id)
        };
        let (a, b) = (
            "c876812b-5f3e-4a1d-9b7c-2e8f0a6d4c13",
            "c876812b-0000-4000-8000-000000000001",
        );
        let rows_of = |calls: &[(Act, CallStatus)]| -> Vec<Row> {
            let steps: Vec<Step> = calls
                .iter()
                .enumerate()
                .map(|(at, (act, status))| {
                    Step::Call(Call {
                        id: at as u64 + 1,
                        act: Some(act),
                        status: Some(*status),
                        ..Call::default()
                    })
                })
                .collect();
            rows(&steps)
                .into_iter()
                .filter_map(|piece| match piece {
                    Piece::Row(row) => Some(row),
                    Piece::Step(_) => None,
                })
                .collect()
        };
        let merged = rows_of(&[(job(a, "new output"), Ok), (job(a, "finished"), Ok)]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].members, [0, 1]);
        assert_eq!(
            (merged[0].words.news.clone(), merged[0].words.note.clone()),
            (None, None)
        );
        // A wait on it that runs has a row of its own until it ends.
        let running = rows_of(&[(job(a, "new output"), Ok), (job(a, ""), Running)]);
        assert_eq!(running.len(), 2);
        assert_eq!(running[0].words.note.as_deref(), Some("new output"));
        assert_eq!(running[1].words.text, doing(&job(a, "")));
        assert!(running[1].words.running);
        // Two jobs whose IDs begin alike are two rows, each with what its
        // wait found.
        let two = rows_of(&[(job(a, "new output"), Ok), (job(b, "nothing new"), Ok)]);
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].words.note.as_deref(), Some("new output"));
        assert_eq!(two[1].words.note.as_deref(), Some("nothing new"));
        // A wait for workers beside it is a row of its own.
        let mixed = rows_of(&[(job(a, "nothing new"), Ok), (act(Wait, "r4"), Running)]);
        assert_eq!(mixed.len(), 2);
        assert_eq!(mixed[1].words.text, doing(&act(Wait, "r4")));
        // It names the job by the start of its ID, as the terminal does.
        assert!(doing(&job(a, "")).ends_with(&job_name(a)));
        assert!(!doing(&job(a, "")).contains(a));
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
            Step::Thinking(2),
            call(3, &b),
            Step::Thinking(4),
            call(5, &c),
            Step::Thinking(6),
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
            pieces(&[Step::Thinking(1), call(2, &a), Step::Thinking(3)]),
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
        // While its job runs, its note says how the job runs, which gives way
        // before the words do and stands through the run.
        let runs = Act {
            job_runs: true,
            ..started.clone()
        };
        let steps = [Step::Call(Call {
            act: Some(&runs),
            status: Some(Ok),
            outcome: Some("running"),
            job: true,
            ..Call::default()
        })];
        let [Piece::Row(row)]: [Piece; 1] = rows(&steps).try_into().unwrap() else {
            panic!()
        };
        assert_eq!(
            (row.words.note.as_deref(), row.words.note_runs),
            (Some("running"), true)
        );
        assert_eq!(note_keep(&row.words), WORDS_KEEP_RUNNING);
        assert_eq!(outcome_keep(Some(&runs)), WORDS_KEEP_RUNNING);
        assert_eq!(outcome_keep(Some(&started)), WORDS_KEEP);
        // A long call that runs has a row of its own, which says what runs
        // with no note, so a narrow line keeps it and the row's buttons.
        let (rg, build) = (act(Command, "rg x"), act(Command, "cargo build"));
        let steps = [
            Step::Call(Call {
                id: 1,
                act: Some(&rg),
                status: Some(Ok),
                ..Call::default()
            }),
            Step::Call(Call {
                id: 2,
                act: Some(&build),
                status: Some(Running),
                ..Call::default()
            }),
        ];
        let [_, Piece::Row(row)]: [Piece; 2] = rows(&steps).try_into().unwrap() else {
            panic!()
        };
        assert_eq!(row.words.text, doing(&build));
        assert_eq!(
            (row.words.news.as_deref(), row.words.note.as_deref()),
            (None, None)
        );
        assert!(row.words.running);
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

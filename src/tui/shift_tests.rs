//! A meter of how the transcript's lines move while an agent works, on
//! termelon's emulator. A scripted session takes every live step of a turn
//! many times, in the Full, Quiet and Messages views, at several widths and
//! heights, with the reader following the end and with the reader scrolled
//! up. Every word of the session is a word no other has, so each line on the
//! screen is found again in the next frame by its first word. Following, a
//! line may move up, as what comes at the end pushes it, and never down.
//! Scrolled up, no line moves. A fold the reader opens or closes, by the
//! mouse or the keyboard, moves no line above it, up, down or along its row.
//! That goes for a long content's "N more lines" too, which opens the rest
//! below its preview and closes it again. Two moves are allowed. "Working…"
//! and the notes waiting at the end go down as what comes above them takes
//! their rows, into the blank rows below the end, and while the reader reads
//! on past the bottom row, and up as what stood above them goes. A note the
//! agent reads in the frame where output comes above it goes down by that
//! output's rows and no further. And rows that came after an open call go
//! down by the rows its content grows by above them as it comes, a command's
//! output in Full and a code run's calls and its result in any view where
//! the run is open. A worker's start keeps its words in their columns as the
//! worker's ID comes.
//!
//! `SHIFT_METER_SEEDS` plays that many sessions at more sizes, and
//! `SHIFT_METER_OUT` names a file for the tables of every step and view.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use termelon::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use termelon::testing::TestTerminal;

use crate::glyphs::{Symbols, tests::choose_symbols};
use crate::remote::{
    Act, ActKind, Activity, CallLine, CallStatus, Calls, ContentKind, Delivery, Item, Kind, Level,
    Update, content, quiet,
};
use crate::tui::app::{App, Info};
use crate::tui::input::{Editor, History};
use crate::tui::screen::draw;
use crate::tui::theme::{Catalog, Depth};
use crate::tui::transcript::{Target, View};

/// Numbers from a seed, so a session plays the same each time.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n.max(1)
    }

    /// True `percent` times in a hundred.
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// A step of a turn, which the frames it makes are counted under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Step {
    Asked,
    WorkingCall,
    WorkingThought,
    ThoughtGrows,
    ThoughtCall,
    MergeFile,
    MergeKind,
    Result,
    RunCall,
    RunCallEnds,
    CallReply,
    ThoughtReply,
    ReplyGrows,
    ReplyWorking,
    WorkingReply,
    JobEnds,
    Notice,
    Waits,
    Read,
    ResultRead,
    FoldOpens,
    FoldCloses,
    DoorOpens,
    DoorCloses,
    TurnEnds,
    WorkerStarts,
}

impl Step {
    const ALL: [Step; 26] = [
        Step::Asked,
        Step::WorkingCall,
        Step::WorkingThought,
        Step::ThoughtGrows,
        Step::ThoughtCall,
        Step::MergeFile,
        Step::MergeKind,
        Step::Result,
        Step::RunCall,
        Step::RunCallEnds,
        Step::CallReply,
        Step::ThoughtReply,
        Step::ReplyGrows,
        Step::ReplyWorking,
        Step::WorkingReply,
        Step::JobEnds,
        Step::Notice,
        Step::Waits,
        Step::Read,
        Step::ResultRead,
        Step::FoldOpens,
        Step::FoldCloses,
        Step::DoorOpens,
        Step::DoorCloses,
        Step::TurnEnds,
        Step::WorkerStarts,
    ];

    fn name(self) -> &'static str {
        match self {
            Step::Asked => "message → Working…",
            Step::WorkingCall => "Working… → call",
            Step::WorkingThought => "Working… → thought",
            Step::ThoughtGrows => "thought streams",
            Step::ThoughtCall => "thought → call",
            Step::MergeFile => "call joins a row of one file",
            Step::MergeKind => "call joins a row of its kind",
            Step::Result => "call → its result",
            Step::RunCall => "code run's call starts",
            Step::RunCallEnds => "code run's call ends",
            Step::CallReply => "result → reply",
            Step::ThoughtReply => "thought → reply",
            Step::ReplyGrows => "reply streams",
            Step::ReplyWorking => "reply pauses → Working…",
            Step::WorkingReply => "Working… → reply goes on",
            Step::JobEnds => "job running → finished",
            Step::Notice => "notice",
            Step::Waits => "message waits",
            Step::Read => "waiting message read",
            Step::ResultRead => "call → its result, a message read",
            Step::FoldOpens => "fold opens near the end",
            Step::FoldCloses => "fold closes near the end",
            Step::DoorOpens => "\"N more lines\" opens",
            Step::DoorCloses => "\"N more lines\" closes",
            Step::TurnEnds => "turn ends",
            Step::WorkerStarts => "worker start → its ID",
        }
    }
}

/// The frames of a step, how many moved a line, and by how many rows at
/// most and in all (each frame's largest move).
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    frames: u32,
    shifted: u32,
    max: u32,
    total: u32,
}

impl Tally {
    fn add(&mut self, moved: u32) {
        self.frames += 1;
        if moved > 0 {
            self.shifted += 1;
            self.max = self.max.max(moved);
            self.total += moved;
        }
    }

    fn cell(&self) -> String {
        if self.frames == 0 {
            "–".into()
        } else if self.shifted == 0 {
            format!("{} · 0", self.frames)
        } else {
            format!(
                "{} · **{}** · {} / {}",
                self.frames, self.shifted, self.max, self.total
            )
        }
    }
}

/// Where the reader is, for the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Reader {
    Following,
    Up,
}

/// How a frame moved its lines, which sorts the moves by cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum How {
    /// The reader toggled a fold, and a line above it moved.
    Toggled,
    /// Following, every line on the screen moved down (the end came down
    /// to the last row).
    AllDown,
    /// Following, lines below a change moved down, and the transcript was
    /// shorter than the screen, its start on the top row.
    Short,
    /// Following, lines below a change moved down, with blank rows below
    /// the end.
    Gap,
    /// Following, lines below a change moved down, with the end on the last
    /// row.
    AtEnd,
    /// Scrolled up, a line moved.
    Up,
}

/// The line that moved most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum What {
    /// A message waiting to be read, at the end.
    Note,
    /// The "Working…" line.
    Working,
    Other,
}

type Cause = (How, What);

/// A move the meter allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Allowed {
    /// "Working…" or a note waiting at the end went down into the blank
    /// rows below the end, as what came above them took those rows.
    Room,
    /// Rows that came after an open call went down by the rows its content
    /// grew by above them, as its content came. That is a command's output
    /// in Full, and a code run's calls and its result in any view where the
    /// run is open.
    Arrived,
}

/// What the sessions measured.
#[derive(Default)]
struct Meter {
    /// By step, view (its place in [`View::ALL`]) and where the reader was.
    tallies: BTreeMap<(Step, usize, Reader), Tally>,
    /// By the size of the screen, view and where the reader was.
    sizes: BTreeMap<((u16, u16), usize, Reader), Tally>,
    /// The frames that moved a line, by cause, step and view.
    causes: BTreeMap<(Cause, Step, usize), Tally>,
    /// The moves the meter allows, by step and view.
    allowed: BTreeMap<(Allowed, Step, usize), Tally>,
    /// The largest move of each cause in each step and view, the screens
    /// before and after.
    examples: BTreeMap<(Cause, Step, usize), (u32, String)>,
    /// The worker starts whose words moved along their row as the worker's
    /// ID came, with the rows before and after.
    sideways: Vec<String>,
    /// The folds toggled that moved a line above them along its row, with
    /// the rows before and after.
    aside: Vec<String>,
    /// The frames that toggled a fold by the keyboard, by step.
    keyed: BTreeMap<Step, u32>,
}

impl Meter {
    /// The frames that moved a line.
    fn shifted(&self) -> u32 {
        self.tallies.values().map(|tally| tally.shifted).sum()
    }

    fn frames(&self) -> u32 {
        self.tallies.values().map(|tally| tally.frames).sum()
    }

    /// The table, a row for each step and a column for each view and where
    /// the reader was, each cell "frames · shifted · max / total rows".
    fn table(&self) -> String {
        let mut out = String::new();
        let views = View::ALL.map(|view| view.name());
        let _ = writeln!(
            out,
            "| Step | {} |",
            views
                .iter()
                .flat_map(|view| [format!("{view}, following"), format!("{view}, scrolled up")])
                .collect::<Vec<_>>()
                .join(" | ")
        );
        let _ = writeln!(out, "|---|{}", "---|".repeat(views.len() * 2));
        let mut all = [Tally::default(); 6];
        for step in Step::ALL {
            let mut cells = Vec::new();
            for view in 0..views.len() {
                for (at, reader) in [Reader::Following, Reader::Up].into_iter().enumerate() {
                    let tally = self
                        .tallies
                        .get(&(step, view, reader))
                        .copied()
                        .unwrap_or_default();
                    let sum = &mut all[view * 2 + at];
                    sum.frames += tally.frames;
                    sum.shifted += tally.shifted;
                    sum.max = sum.max.max(tally.max);
                    sum.total += tally.total;
                    cells.push(tally.cell());
                }
            }
            let _ = writeln!(out, "| {} | {} |", step.name(), cells.join(" | "));
        }
        let cells: Vec<String> = all.iter().map(Tally::cell).collect();
        let _ = writeln!(out, "| All | {} |", cells.join(" | "));
        out
    }

    /// The table by the screen's size.
    fn size_table(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "| Size | View | Following | Scrolled up |");
        let _ = writeln!(out, "|---|---|---|---|");
        let sizes: Vec<(u16, u16)> = {
            let mut sizes: Vec<(u16, u16)> = self.sizes.keys().map(|(size, ..)| *size).collect();
            sizes.dedup();
            sizes
        };
        for size in sizes {
            for (view, name) in View::ALL.iter().enumerate() {
                let cell = |reader| {
                    self.sizes
                        .get(&(size, view, reader))
                        .copied()
                        .unwrap_or_default()
                        .cell()
                };
                let _ = writeln!(
                    out,
                    "| {}×{} | {} | {} | {} |",
                    size.0,
                    size.1,
                    name.name(),
                    cell(Reader::Following),
                    cell(Reader::Up)
                );
            }
        }
        out
    }

    /// The frames that moved a line by cause, step and view, each cell
    /// "frames that moved a line · the most rows / all rows".
    fn cause_table(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "| Cause | Step | full | quiet | messages |");
        let _ = writeln!(out, "|---|---|---|---|---|");
        let mut keys: Vec<(Cause, Step)> = self.causes.keys().map(|(c, s, _)| (*c, *s)).collect();
        keys.dedup();
        for (cause, step) in keys {
            let cells: Vec<String> = (0..View::ALL.len())
                .map(|view| match self.causes.get(&(cause, step, view)) {
                    Some(tally) => format!("{} · {} / {}", tally.shifted, tally.max, tally.total),
                    None => "–".into(),
                })
                .collect();
            let _ = writeln!(
                out,
                "| {:?}, {:?} | {} | {} |",
                cause.0,
                cause.1,
                step.name(),
                cells.join(" | ")
            );
        }
        out
    }

    /// The moves the meter allows, by kind, step and view, each cell
    /// "frames · the most rows / all rows".
    fn allowed_table(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "| Move | Step | full | quiet | messages |");
        let _ = writeln!(out, "|---|---|---|---|---|");
        let mut keys: Vec<(Allowed, Step)> = self
            .allowed
            .keys()
            .map(|(kind, step, _)| (*kind, *step))
            .collect();
        keys.dedup();
        for (kind, step) in keys {
            let cells: Vec<String> = (0..View::ALL.len())
                .map(|view| match self.allowed.get(&(kind, step, view)) {
                    Some(tally) => format!("{} · {} / {}", tally.frames, tally.max, tally.total),
                    None => "–".into(),
                })
                .collect();
            let kind = match kind {
                Allowed::Room => "the end makes room",
                Allowed::Arrived => "an open call's content comes above later rows",
            };
            let _ = writeln!(out, "| {kind} | {} | {} |", step.name(), cells.join(" | "));
        }
        out
    }

    fn examples(&self) -> String {
        let mut out = String::new();
        for ((cause, step, view), (_, example)) in &self.examples {
            let _ = writeln!(
                out,
                "### {:?} {:?}, {}, {}\n\n```text\n{example}```\n",
                cause.0,
                cause.1,
                step.name(),
                View::ALL[*view].name()
            );
        }
        out
    }
}

/// The words on `row` that the session made, `w` and a number, past a
/// word the line cut short.
fn words(row: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    let chars: Vec<(usize, char)> = row.char_indices().collect();
    for (at, &(index, c)) in chars.iter().enumerate() {
        let word = c.is_ascii_alphanumeric();
        match (start, word) {
            (None, true) => start = Some(index),
            (Some(from), false) => {
                if c != '…' {
                    out.push(&row[from..index]);
                }
                start = None;
            }
            _ => {}
        }
        if at + 1 == chars.len()
            && let Some(from) = start
        {
            out.push(&row[from..]);
        }
    }
    out.retain(|word| {
        word.len() > 1 && word.starts_with('w') && word[1..].bytes().all(|b| b.is_ascii_digit())
    });
    out
}

/// A row's text past the markers' column, where the spinner turns.
fn past_marker(row: &str) -> &str {
    let at = row.char_indices().nth(2).map_or(row.len(), |(at, _)| at);
    row[at..].trim()
}

/// Each line of `rows`, by what tells it from every other row there, which
/// is its first word that no other row has, else its text when no other row
/// has that. Blank rows and rows that share all they have are left out.
fn lines(rows: &[String]) -> HashMap<String, usize> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut texts: HashMap<&str, usize> = HashMap::new();
    let all: Vec<Vec<&str>> = rows.iter().map(|row| words(row)).collect();
    for (row, words) in rows.iter().zip(&all) {
        let mut mine = words.clone();
        mine.sort_unstable();
        mine.dedup();
        for word in mine {
            *seen.entry(word).or_default() += 1;
        }
        *texts.entry(past_marker(row)).or_default() += 1;
    }
    let mut out = HashMap::new();
    for (y, (row, words)) in rows.iter().zip(&all).enumerate() {
        let text = past_marker(row);
        if text.is_empty() {
            continue;
        }
        let id = match words.iter().find(|word| seen[*word] == 1) {
            Some(word) => format!("w:{word}"),
            None if texts[text] == 1 => format!("t:{text}"),
            None => continue,
        };
        out.insert(id, y);
    }
    out
}

/// The column each line of `rows` that [`lines`] finds by a word has that
/// word in.
fn columns(rows: &[String]) -> HashMap<String, usize> {
    lines(rows)
        .into_iter()
        .filter_map(|(id, y)| {
            let word = id.strip_prefix("w:")?;
            let at = place_of(&rows[y..=y], word)?.1;
            Some((id, at))
        })
        .collect()
}

/// The lines on the row `below` and above it that `before` and `after` both show,
/// whose words moved along their row, from which column to which.
fn moved_aside(before: &[String], after: &[String], below: usize) -> Vec<(String, usize, usize)> {
    let rows = lines(before);
    let now = columns(after);
    columns(before)
        .into_iter()
        .filter(|(id, _)| rows[id] <= below)
        .filter_map(|(id, from)| {
            let to = *now.get(&id)?;
            (to != from).then_some((id, from, to))
        })
        .collect()
}

/// How the lines of a frame moved from the frame before.
#[derive(Default)]
struct Moved {
    /// The most rows any line moved, down, and up too when the reader didn't
    /// follow in both frames or toggled a fold.
    rows: u32,
    /// The line that moved most, from where to where.
    line: Option<(String, usize, usize)>,
    /// The highest line both frames show moved down too, so every line did.
    all: bool,
    /// The most rows "Working…" or a note waiting at the end went down, as
    /// what came above them took their rows, or while the reader reads, up.
    room: u32,
    /// The most rows a line that came after an open call went down, by as
    /// many as the call's content grew above it ([`Allowed::Arrived`]).
    arrived: u32,
}

/// How the lines of `before` that `after` shows again moved, where a line
/// going up counts only when `following` is false. Rows of `before` below
/// `below` are left out (the fold a click toggled there moves them). From
/// `end` on, `after` shows "Working…", the notes waiting to be read and the
/// blank rows below the end, and from `was_end` on, `before` does. With
/// `call`, which rows of `before` and of `after` show the call whose content
/// came ([`Allowed::Arrived`]).
fn moves(
    before: &[String],
    after: &[String],
    following: bool,
    below: Option<usize>,
    (was_end, end): (usize, usize),
    call: Option<(&[bool], &[bool])>,
) -> Moved {
    let (was, now) = (lines(before), lines(after));
    // (The rows of lines `before` doesn't show, from where its end began.)
    let new: Vec<usize> = now
        .iter()
        .filter(|(id, y)| **y >= was_end && !was.contains_key(*id))
        .map(|(_, y)| *y)
        .collect();
    let mut moved = Moved::default();
    let mut highest: Option<(usize, usize)> = None;
    for (id, from) in was {
        if below.is_some_and(|row| from > row) {
            continue;
        }
        let Some(&to) = now.get(&id) else {
            continue;
        };
        let rows = if to > from || !following {
            to.abs_diff(from) as u32
        } else {
            0
        };
        // ("Working…" and the notes waiting go down as what comes above them
        // takes their rows, and while the reader reads, up as what stood
        // above them goes. A note the agent reads as output comes above it
        // goes down by that output's rows and no further.)
        let pushed = new.iter().filter(|y| **y < to).count();
        let read = from >= was_end && to > from && to - from <= pushed;
        if (to >= end || read) && (to > from || !following) {
            moved.room = moved.room.max(rows);
            continue;
        }
        // (A line below the call's rows, down by as many rows as those grew
        // above it, past what the whole screen moved, as the call's first
        // row did.)
        if let Some((was, now)) = call
            && to > from
            && !now.get(to).copied().unwrap_or(false)
            && let Some(first_was) = was.iter().position(|of| *of)
            && let Some(first_now) = now.iter().position(|of| *of)
            && first_was < from
            && first_now < to
        {
            let count =
                |of: &[bool], upto: usize| of.iter().take(upto).filter(|of| **of).count() as i64;
            let grew = count(now, to) - count(was, from);
            let by = (to as i64 - from as i64) - (first_now as i64 - first_was as i64);
            if grew > 0 && by == grew {
                moved.arrived = moved.arrived.max(rows);
                continue;
            }
        }
        if highest.is_none_or(|(at, _)| from < at) {
            highest = Some((from, to));
        }
        if rows > moved.rows {
            moved.rows = rows;
            moved.line = Some((id, from, to));
        }
    }
    moved.all = highest.is_some_and(|(from, to)| to > from);
    moved
}

/// A frame's transcript, as the emulator shows it, and how the screen
/// stood.
struct Shot {
    rows: Vec<String>,
    following: bool,
    /// The blank rows below the end.
    gap: u32,
    /// The transcript's start is on the top row.
    start: bool,
    /// The first row of "Working…", the notes waiting to be read and the
    /// blank rows below the end.
    end: usize,
    /// The key of the block each row shows.
    keys: Vec<Option<u64>>,
}

/// The Markdown of the reply that is open as it streams.
#[derive(Debug, Default)]
struct Open {
    bold: bool,
    code: bool,
}

impl Open {
    /// What closes it.
    fn close(&mut self) -> Option<String> {
        let close = match (self.bold, self.code) {
            (_, true) => "\n```\n",
            (true, false) => "**",
            (false, false) => return None,
        };
        *self = Open::default();
        Some(close.into())
    }
}

/// What is live at the end of the transcript, which names the next step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Live {
    Working,
    Thought,
    Reply,
    /// A call's result came, and Working… shows.
    Result,
}

/// A session, played into an app on an emulated terminal a change at a
/// time, each change drawn as a frame and measured.
struct Run<'m> {
    app: App,
    terminal: TestTerminal,
    rng: Rng,
    /// Numbers for the long contents and their doors.
    doors: Rng,
    /// Whether the agent now and then reads the message waiting in the
    /// frame of a call's result (the sessions of `SHIFT_METER_SEEDS`), and
    /// the numbers for it.
    together: bool,
    reads: Rng,
    /// Numbers for the code runs, which take the places of `rng` and
    /// `doors` while one plays, so the rest of the session makes the choices
    /// it would make without them.
    runs: Rng,
    run_doors: Rng,
    /// Whether its turns make code runs.
    code_runs: bool,
    id: u64,
    at: i64,
    word: u64,
    turn: u64,
    view: usize,
    size: (u16, u16),
    meter: &'m mut Meter,
    /// The last frame.
    last: Option<Shot>,
    /// The reader keeps scrolled up.
    scrolled: bool,
    live: Live,
    /// The turn's folder, and its calls since what was said last, with
    /// each one's kind, target and whether it stands alone.
    folder: String,
    calls: Vec<(ActKind, String, bool)>,
    /// A job that runs, by its call.
    job: Option<Item>,
    /// The fold the reader toggled, by its item (a count's key), to toggle
    /// back, and those toggled an odd number of times.
    opened: Option<(u64, bool)>,
    toggled: HashSet<(u64, bool)>,
    /// A message waiting to be read.
    waiting: Option<(u64, String)>,
    /// The message waiting was read in the frame of a call's result. Until
    /// the read it would have had, no other is typed, so the rest of the
    /// session plays as it would have.
    read_early: bool,
    clicks: u16,
    /// Whether its turns start workers, and how many they started.
    starts: bool,
    workers: u64,
    /// The call whose content comes in the next frame, and whether it is a
    /// code run.
    arriving: Option<(u64, bool)>,
}

/// The step a click on `row` makes when it's a content's door ("… 6 more
/// lines") or the line that closes what the door opened.
fn door_of(row: &str) -> Option<Step> {
    let text = row.trim_start_matches(['│', ' ']).trim_end();
    if text.starts_with("… ") && text.contains(" more ") {
        Some(Step::DoorOpens)
    } else if text == "↑ show the first lines only" {
        Some(Step::DoorCloses)
    } else {
        None
    }
}

/// The row and column of the first word `word` in `rows`.
fn place_of(rows: &[String], word: &str) -> Option<(usize, usize)> {
    rows.iter().enumerate().find_map(|(y, row)| {
        let at = row
            .find(&format!("{word} "))
            .or_else(|| row.ends_with(word).then(|| row.len() - word.len()))?;
        Some((y, row[..at].chars().count()))
    })
}

impl<'m> Run<'m> {
    fn new(seed: u64, view: usize, size: (u16, u16), scrolled: bool, meter: &'m mut Meter) -> Self {
        let mut app = App::new(
            crate::remote::Snapshot::default(),
            Editor::new(History::load(None)),
            Info::default(),
            Catalog::load(None),
            Depth::Rgb,
        );
        app.view = View::ALL[view];
        Self {
            app,
            terminal: crate::tui::golden::terminal(size.0, size.1),
            rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
            doors: Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1),
            reads: Rng(seed.wrapping_mul(0xA24B_AED4_963E_E407) | 1),
            together: false,
            runs: Rng(seed.wrapping_mul(0xA24B_AED4_963E_E407) | 1),
            run_doors: Rng(seed.wrapping_mul(0x9FB2_1C65_1E98_DF25) | 1),
            code_runs: true,
            id: 0,
            at: chrono::Utc::now().timestamp_millis(),
            word: 0,
            turn: 0,
            view,
            size,
            meter,
            last: None,
            scrolled,
            live: Live::Working,
            folder: String::new(),
            calls: Vec::new(),
            job: None,
            opened: None,
            toggled: HashSet::new(),
            waiting: None,
            read_early: false,
            clicks: 0,
            starts: false,
            workers: 0,
            arriving: None,
        }
    }

    /// `n` words no other text has.
    fn words(&mut self, n: u64) -> String {
        let words: Vec<String> = (0..n.max(1))
            .map(|_| {
                self.word += 1;
                format!("w{}", self.word)
            })
            .collect();
        words.join(" ")
    }

    /// A name several lines may have, a folder's (`d`) or a file's (`f`),
    /// which the meter doesn't find a line by.
    fn name(&mut self, kind: char) -> String {
        self.word += 1;
        format!("{kind}{}", self.word)
    }

    /// At least `least` words no other text has, and fewer than `more`
    /// besides.
    fn some(&mut self, least: u64, more: u64) -> String {
        let n = least + self.rng.below(more);
        self.words(n)
    }

    fn item(&mut self, kind: Kind, text: &str) -> Item {
        self.id += 1;
        self.at += 10;
        let mut item = Item::new(kind, text);
        item.id = self.id;
        item.at = self.at;
        item.turn = Some(self.turn);
        item
    }

    fn add(&mut self, item: Item) -> u64 {
        let id = item.id;
        self.app.update(Update::Item(item), Activity::default);
        id
    }

    fn update(&mut self, update: Update) {
        self.app.update(update, Activity::default);
    }

    fn works(&mut self, working: bool) {
        self.app.set_activity(Activity {
            reporting: false,
            working,
            since: working.then_some(self.at),
            question: None,
        });
    }

    /// The transcript's rows on the emulator's screen as the frame just
    /// drawn left them, and how the screen stood.
    fn shot(&mut self) -> Shot {
        let app = &mut self.app;
        self.terminal.draw(|frame| draw(frame, app)).unwrap();
        let area = self.app.transcript.drawn.area;
        let screen = self.terminal.screen().rows();
        Shot {
            rows: screen[area.y as usize..area.bottom() as usize].to_vec(),
            following: self.app.transcript.following(),
            gap: self.app.transcript.gap(),
            start: self.app.transcript.shows_start(),
            end: self.app.transcript.end_starts(),
            keys: self.app.transcript.row_keys(),
        }
    }

    /// Draws a frame and counts it under `step`, with how far its lines
    /// moved from the last frame's. Rows below `below` of the last frame
    /// aren't counted (a fold toggled on that row moves them), and every
    /// line above it counts that moves, up as well as down.
    fn frame_below(&mut self, step: Step, below: Option<usize>) {
        let shot = self.shot();
        // The call whose content came in this frame, where it is open: a
        // command's in Full and a code run's in any view.
        let arriving = self
            .arriving
            .take()
            .filter(|(_, code)| *code || View::ALL[self.view] == View::Full);
        let call = match (arriving, &self.last) {
            (Some((call, _)), Some(last)) => {
                let (filter, theme) = (self.app.filter(), self.app.theme.clone());
                let keys = self.app.transcript.keys_of_call(call, &filter, &theme);
                // (Its rows, and the blank rows right after them, the gap
                // the row after it has from it.)
                let of = |keyed: &[Option<u64>], rows: &[String]| -> Vec<bool> {
                    let mut of: Vec<bool> = keyed
                        .iter()
                        .map(|key| key.is_some_and(|key| keys.contains(&key)))
                        .collect();
                    for y in 1..of.len() {
                        if of[y - 1] && rows.get(y).is_some_and(|row| row.trim().is_empty()) {
                            of[y] = true;
                        }
                    }
                    of
                };
                Some((of(&last.keys, &last.rows), of(&shot.keys, &shot.rows)))
            }
            _ => None,
        };
        if let Some(last) = &self.last {
            let following = shot.following && last.following;
            // (Following, a line may go up as what comes at the end pushes
            // it, but not in the frame of a toggle.)
            let rises = following && below.is_none();
            let moved = moves(
                &last.rows,
                &shot.rows,
                rises,
                below,
                (last.end, shot.end),
                call.as_ref()
                    .map(|(was, now)| (was.as_slice(), now.as_slice())),
            );
            if let Some(row) = below {
                for (id, from, to) in moved_aside(&last.rows, &shot.rows, row) {
                    let rows: Vec<String> = last
                        .rows
                        .iter()
                        .zip(&shot.rows)
                        .map(|(a, b)| format!("{a:<width$} │ {b}", width = self.size.0 as usize))
                        .collect();
                    self.meter.aside.push(format!(
                        "{}×{} {:?}, {}: {id} went from column {from} to {to}\n{}",
                        self.size.0,
                        self.size.1,
                        View::ALL[self.view],
                        step.name(),
                        rows.join("\n")
                    ));
                }
            }
            if moved.room > 0 {
                self.meter
                    .allowed
                    .entry((Allowed::Room, step, self.view))
                    .or_default()
                    .add(moved.room);
            }
            if moved.arrived > 0 {
                self.meter
                    .allowed
                    .entry((Allowed::Arrived, step, self.view))
                    .or_default()
                    .add(moved.arrived);
            }
            let rows = moved.rows;
            let reader = if following {
                Reader::Following
            } else {
                Reader::Up
            };
            self.meter
                .tallies
                .entry((step, self.view, reader))
                .or_default()
                .add(rows);
            self.meter
                .sizes
                .entry((self.size, self.view, reader))
                .or_default()
                .add(rows);
            if let Some((id, from, to)) = moved.line.filter(|_| rows > 0) {
                let what = match &last.rows[from] {
                    row if row.ends_with("pending") => What::Note,
                    row if row.contains("Working…") => What::Working,
                    _ => What::Other,
                };
                let how = match () {
                    _ if below.is_some() => How::Toggled,
                    _ if !following => How::Up,
                    _ if moved.all => How::AllDown,
                    _ if last.start && shot.start => How::Short,
                    _ if last.gap > 0 => How::Gap,
                    _ => How::AtEnd,
                };
                let cause = (how, what);
                self.meter
                    .causes
                    .entry((cause, step, self.view))
                    .or_default()
                    .add(moved.rows);
                let key = (cause, step, self.view);
                if self
                    .meter
                    .examples
                    .get(&key)
                    .is_none_or(|(rows, _)| moved.rows > *rows)
                {
                    let mut example = format!(
                        "{}×{}: {id} went from row {from} to {to}, {} blank rows below the end, then {}{}\n",
                        self.size.0,
                        self.size.1,
                        last.gap,
                        shot.gap,
                        if last.start {
                            ", the start on the top row"
                        } else {
                            ""
                        }
                    );
                    for (y, (a, b)) in last.rows.iter().zip(&shot.rows).enumerate() {
                        let _ = writeln!(
                            example,
                            "{y:>2} {a:<width$} │ {b}",
                            width = self.size.0 as usize
                        );
                    }
                    self.meter.examples.insert(key, (moved.rows, example));
                }
            }
        }
        self.last = Some(shot);
    }

    fn frame(&mut self, step: Step) {
        self.frame_below(step, None);
    }

    /// A frame the reader's own scroll made, which nothing is counted for.
    fn moved(&mut self) {
        let shot = self.shot();
        self.last = Some(shot);
    }

    /// A reader who keeps scrolled up and follows again scrolls up a few
    /// rows.
    fn scroll_up(&mut self) {
        if self.scrolled && self.app.transcript.following() {
            let rows = 2 + self.rng.below(6) as i64;
            self.app.transcript.scroll(-rows);
            self.moved();
        }
    }

    /// Now and then, what may come at any time, which is a notice, a job's
    /// end, a message the user sends meanwhile, or the reader toggling a
    /// fold.
    /// Returns whether a notice came, after which the agent's text or
    /// thinking goes on in an item of its own.
    fn meanwhile(&mut self) -> bool {
        let notice = self.rng.chance(6);
        if notice {
            let text = self.some(3, 12);
            let mut notice = self.item(Kind::Notice, &text);
            notice.level = Some(if self.rng.chance(50) {
                Level::Warn
            } else {
                Level::Info
            });
            self.add(notice);
            self.frame(Step::Notice);
        }
        if self.job.is_some() && self.rng.chance(5) {
            let mut call = self.job.take().unwrap();
            call.outcome = Some("finished".into());
            self.update(Update::Change(call));
            self.frame(Step::JobEnds);
        }
        if self.waiting.is_none() && !self.read_early && self.rng.chance(1) {
            let text = format!("Also {}", self.some(2, 8));
            let mut note = self.item(Kind::User, &text);
            note.delivery = Some(Delivery::Note);
            note.pending = true;
            let id = self.add(note);
            self.waiting = Some((id, text));
            self.frame(Step::Waits);
        }
        if self.rng.chance(8) {
            self.toggle();
        }
        notice
    }

    /// The agent reads the message waiting, which comes again where it
    /// read it.
    fn read_waiting(&mut self) {
        self.read_early = false;
        if self.waiting.is_some() {
            self.read_note();
            self.frame(Step::Read);
        }
    }

    /// The agent reads the message waiting, as the feed sends it, in the
    /// frame drawn next.
    fn read_note(&mut self) {
        if let Some((id, text)) = self.waiting.take() {
            self.update(Update::Remove { id });
            let mut note = self.item(Kind::User, &text);
            note.delivery = Some(Delivery::Note);
            self.add(note);
        }
    }

    /// Clicks the screen's row `y` of the transcript, pressed and let go.
    fn click(&mut self, y: usize) {
        let area = self.app.transcript.drawn.area;
        // (Another column each time, so two clicks are never a double one.)
        self.clicks += 1;
        let x = area.x + 2 + self.clicks % 12;
        let y = area.y + y as u16;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            self.app
                .mouse(MouseEvent::new(kind, x, y, KeyModifiers::NONE));
        }
    }

    /// The fold on the transcript's row `y`, by its item, or a turn's
    /// count there (`true`).
    fn fold_on(&mut self, y: usize) -> Option<(u64, bool)> {
        let area = self.app.transcript.drawn.area;
        let (x, y) = (area.x + 3, area.y + y as u16);
        let key = self.app.transcript.drawn.key_at(x, y)?;
        if self.app.transcript.is_counter(key) {
            return Some((key, true));
        }
        let (filter, theme) = (self.app.filter(), self.app.theme.clone());
        self.app
            .transcript
            .fold_at(x, y, &filter, &theme)
            .map(|fold| (fold.item, false))
    }

    /// Toggles the lowest fold among the screen's last rows, or toggles
    /// back the one the reader toggled. (A command in the Full view is open
    /// from the start, so a first click may close it.)
    fn toggle(&mut self) {
        let Some(rows) = self.last.as_ref().map(|last| last.rows.clone()) else {
            return;
        };
        let end = rows.iter().rposition(|row| !row.trim().is_empty());
        let Some(end) = end else {
            return;
        };
        let at = match self.opened.take() {
            None => (end.saturating_sub(6)..=end)
                .rev()
                .find_map(|y| self.fold_on(y).map(|fold| (y, fold))),
            // (Out of sight, it stays as it is.)
            Some(fold) => (0..rows.len())
                .find(|y| self.fold_on(*y) == Some(fold))
                .map(|y| (y, fold)),
        };
        let Some((y, fold)) = at else {
            return;
        };
        // An open fold's line has ▾, and a thinking's has no marker. A
        // content's door is its last line.
        let marker = rows[y].trim_start_matches(['│', ' ']).chars().next();
        let step = match (door_of(&rows[y]), marker) {
            (Some(step), _) => step,
            (_, Some('▾')) => Step::FoldCloses,
            (_, Some('▸')) => Step::FoldOpens,
            _ if self.toggled.contains(&fold) => Step::FoldCloses,
            _ => Step::FoldOpens,
        };
        if !self.toggled.remove(&fold) {
            self.toggled.insert(fold);
            self.opened = Some(fold);
        }
        self.press(y, step);
    }

    /// A thought streams, from what was live, which names the step.
    fn think(&mut self) {
        let step = match self.live {
            Live::Reply => return,
            _ => Step::WorkingThought,
        };
        let text = self.some(2, 4);
        let thought = self.item(Kind::Thinking, &text);
        let mut id = Some(self.add(thought));
        self.live = Live::Thought;
        self.frame(step);
        for _ in 0..self.rng.below(5) {
            let n = 2 + self.rng.below(10);
            let more = match self.rng.below(6) {
                0 => format!("\n\n{}", self.words(n)),
                _ => format!(" {}", self.words(n)),
            };
            match id {
                Some(id) => self.update(Update::Append { id, text: more }),
                None => {
                    let thought = self.item(Kind::Thinking, &more);
                    id = Some(self.add(thought));
                }
            }
            self.frame(Step::ThoughtGrows);
            if self.meanwhile() {
                id = None;
            }
        }
    }

    /// A call, which comes after what is live, its result, and Working…
    /// after it.
    fn call(&mut self) {
        if self.code_runs && self.runs.chance(15) {
            let swap = |run: &mut Self| {
                std::mem::swap(&mut run.rng, &mut run.runs);
                std::mem::swap(&mut run.doors, &mut run.run_doors);
            };
            swap(self);
            self.code_run();
            swap(self);
        }
        // What it does, and whether it joins the row of the call before it
        // in the Quiet view.
        let last = self.calls.last().cloned();
        let name = self.name('f');
        let folder = self.folder.clone();
        let (kind, target, alone) = match self.rng.below(20) {
            // A read of another file of the folder.
            0..=6 => (ActKind::Read, format!("{folder}/{name}.rs"), false),
            // An edit of the file read last, or a read of it again.
            7..=11 => match last
                .as_ref()
                .filter(|(kind, ..)| matches!(kind, ActKind::Read | ActKind::Edit | ActKind::Write))
            {
                Some((_, target, _)) if self.rng.chance(75) => {
                    (ActKind::Edit, target.clone(), false)
                }
                Some((_, target, _)) => (ActKind::Read, target.clone(), false),
                None => (ActKind::Edit, format!("{folder}/{name}.rs"), false),
            },
            // A command, a test run among them, which stands alone.
            12..=16 => (
                ActKind::Command,
                format!("cargo {}", self.words(2)),
                self.rng.chance(30),
            ),
            // A job, once at a time.
            _ if self.job.is_none() => (ActKind::Other, format!("npm run {}", self.words(2)), true),
            _ => (ActKind::List, format!("{folder}/{name}"), false),
        };
        let file = |kind: ActKind| matches!(kind, ActKind::Read | ActKind::Edit | ActKind::Write);
        let joins = last.as_ref().is_some_and(|(was, at, was_alone)| {
            !alone
                && !was_alone
                && kind != ActKind::Other
                && (*was == kind || (file(*was) && file(kind) && *at == target))
        });
        let step = match (joins, last.as_ref()) {
            (true, Some((was, ..))) if *was == kind => Step::MergeKind,
            (true, _) => Step::MergeFile,
            _ if self.live == Live::Thought => Step::ThoughtCall,
            _ => Step::WorkingCall,
        };
        let text = match kind {
            ActKind::Read => format!("read {target}"),
            ActKind::Edit => format!("edit {target}"),
            ActKind::List => format!("list {target}"),
            ActKind::Command => format!("$ {target}"),
            _ => format!("start $ {target}"),
        };
        let mut call = self.item(Kind::Call, &text);
        call.status = Some(CallStatus::Running);
        call.act = Some(Act {
            kind,
            target: target.clone(),
            alone: alone && kind == ActKind::Command,
            ran: kind == ActKind::Command,
            doing: format!("Starting {target} in a terminal"),
            done: format!("Started {target} in a terminal"),
            ..Act::default()
        });
        let id = self.add(call.clone());
        self.calls.push((kind, target.clone(), alone));
        self.frame(step);
        self.meanwhile();
        // It ends, and its content comes after it. Now and then a long one,
        // whose preview leaves some out behind its door, which may cut a line
        // short, and a file's lines that cross into numbers a digit wider.
        let mut ended = call;
        ended.status = Some(CallStatus::Ok);
        // (A long one draws its numbers from the doors' own, so the rest of
        // the session makes the choices it would make without it.)
        let lines = 1 + self.rng.below(6);
        let long = matches!(kind, ActKind::Read | ActKind::Command | ActKind::List)
            && self.doors.chance(45);
        let lines = if long {
            content::PREVIEW_LINES as u64 + 1 + self.doors.below(20)
        } else {
            lines
        };
        // (The line that's longer than a preview shows.)
        let cut = (long && self.doors.chance(30))
            .then(|| self.doors.below(content::PREVIEW_LINES as u64));
        let start = if long && self.doors.chance(50) {
            85 + self.doors.below(14)
        } else {
            1
        };
        let words = |run: &mut Self, n: u64, least: u64, more: u64| {
            if cut == Some(n) {
                run.words(120)
            } else if long {
                let n = least + run.doors.below(more);
                run.words(n)
            } else if more == 0 {
                run.words(least)
            } else {
                run.some(least, more)
            }
        };
        let content = match kind {
            ActKind::Read => {
                ended.outcome = Some(format!("{lines} lines"));
                let text: Vec<String> = (0..lines)
                    .map(|n| format!("{}\t{}", start + n, words(self, n, 2, 6)))
                    .collect();
                Some((ContentKind::Lines, text.join("\n")))
            }
            ActKind::Edit => {
                ended.outcome = Some("1 edit".into());
                let text: Vec<String> = (0..lines.min(4))
                    .map(|n| {
                        let sign = if n % 2 == 0 { '-' } else { '+' };
                        format!("{sign}{}\t{}", n + 3, self.some(2, 5))
                    })
                    .collect();
                Some((ContentKind::Diff, text.join("\n")))
            }
            ActKind::Command => {
                ended.outcome = Some("exit 0".into());
                let text: Vec<String> = (0..lines)
                    .map(|n| format!("{} ok", words(self, n, 2, 5)))
                    .collect();
                Some((ContentKind::Plain, text.join("\n")))
            }
            ActKind::List => {
                ended.outcome = Some(format!("{lines} entries"));
                let text: Vec<String> = (0..lines)
                    .map(|n| format!("{}.rs", words(self, n, 1, 0)))
                    .collect();
                Some((ContentKind::Plain, text.join("\n")))
            }
            _ => {
                ended.outcome = Some("running".into());
                ended.job = Some(format!("job-{id}"));
                self.job = Some(ended.clone());
                None
            }
        };
        // (The feed sends the call's change and its content together.)
        self.update(Update::Change(ended));
        if let Some((kind, text)) = content {
            // (As the feed makes it, its preview and the door to the rest.)
            let (preview, more) = content::preview(kind, &text);
            let mut result = self.item(Kind::Result, &preview);
            result.call = Some(id);
            result.content = Some(kind);
            result.number_width = more
                .is_some()
                .then(|| content::number_width(kind, &text))
                .filter(|width| *width > 0);
            result.whole = more.is_some().then(|| text.into());
            result.more = more;
            self.add(result);
        }
        self.live = Live::Result;
        // (Now and then the agent reads the message waiting as the result
        // comes, in one frame, as when both reach the screen within a
        // frame's time.)
        if self.together && self.waiting.is_some() && self.reads.chance(50) {
            self.read_note();
            self.read_early = true;
            self.frame(Step::ResultRead);
        } else {
            self.arriving = Some((id, false));
            self.frame(Step::Result);
        }
        if long && self.doors.chance(90) {
            self.door();
        }
        self.meanwhile();
    }

    /// A code run. Its call and its code come together, then each call its
    /// code makes starts and ends on the run's row, a few of them refused,
    /// then its result comes. Now and then its code is long, behind a door.
    fn code_run(&mut self) {
        let purpose = self.words(3);
        let step = if self.live == Live::Thought {
            Step::ThoughtCall
        } else {
            Step::WorkingCall
        };
        let mut call = self.item(Kind::Call, &format!("run_code {purpose}"));
        call.status = Some(CallStatus::Running);
        call.act = Some(Act {
            kind: ActKind::Code,
            target: purpose.clone(),
            ..Act::default()
        });
        let id = self.add(call.clone());
        self.calls.push((ActKind::Code, purpose, true));
        let long = self.rng.chance(30);
        let lines = if long {
            content::PREVIEW_LINES as u64 + 1 + self.rng.below(10)
        } else {
            1 + self.rng.below(6)
        };
        let code: Vec<String> = (0..lines).map(|_| self.some(2, 4)).collect();
        let code = code.join("\n");
        let (preview, more) = content::preview(ContentKind::Code, &code);
        let mut block = self.item(Kind::Result, &preview);
        block.call = Some(id);
        block.content = Some(ContentKind::Code);
        block.language = Some("js".into());
        block.whole = more.is_some().then(|| code.into());
        block.more = more;
        self.add(block);
        self.frame(step);
        self.meanwhile();
        let mut calls = Calls::default();
        let mut done: Vec<Act> = Vec::new();
        for _ in 0..1 + self.rng.below(4) {
            let folder = self.folder.clone();
            let (kind, target, text) = match self.rng.below(3) {
                0 => {
                    let target = format!("{folder}/{}.rs", self.words(1));
                    (ActKind::Read, target.clone(), format!("read {target}"))
                }
                1 => {
                    let target = format!("{folder}/{}", self.words(1));
                    (ActKind::List, target.clone(), format!("list {target}"))
                }
                _ => {
                    let target = format!("cargo {}", self.words(2));
                    (ActKind::Command, target.clone(), format!("$ {target}"))
                }
            };
            let act = Act {
                kind,
                target,
                ..Act::default()
            };
            calls.count += 1;
            calls.running = quiet::doing(&act);
            calls.lines.push(CallLine {
                text,
                status: CallStatus::Running,
                outcome: String::new(),
                failed: String::new(),
            });
            call.act.as_mut().unwrap().calls = Some(calls.clone());
            self.update(Update::Change(call.clone()));
            self.arriving = Some((id, true));
            self.frame(Step::RunCall);
            self.meanwhile();
            let refused = self.rng.chance(15);
            let line = calls.lines.last_mut().unwrap();
            if refused {
                line.status = CallStatus::Failed;
                line.outcome = "refused".into();
                let said = quiet::alone(&act, CallStatus::Failed, Some("refused")).said();
                line.failed = format!("{said} · refused");
            } else {
                line.status = CallStatus::Ok;
                line.outcome = "2 lines".into();
                done.push(act);
            }
            calls.running.clear();
            calls.summary = quiet::called(&done);
            call.act.as_mut().unwrap().calls = Some(calls.clone());
            self.update(Update::Change(call.clone()));
            self.arriving = Some((id, true));
            self.frame(Step::RunCallEnds);
            self.meanwhile();
        }
        // It ends, and its result comes after it.
        let lines = 1 + self.rng.below(4);
        call.status = Some(CallStatus::Ok);
        call.outcome = Some(format!("{lines} lines"));
        self.update(Update::Change(call));
        let text: Vec<String> = (0..lines).map(|_| self.some(1, 4)).collect();
        let mut result = self.item(Kind::Result, &text.join("\n"));
        result.call = Some(id);
        result.content = Some(ContentKind::Muted);
        self.add(result);
        self.live = Live::Result;
        self.arriving = Some((id, true));
        self.frame(Step::Result);
        if long && self.rng.chance(90) {
            self.door();
        }
        self.meanwhile();
    }

    /// The reader opens the door of the lowest content among the screen's
    /// last rows, first opening the Quiet row or the call it's folded in
    /// where it doesn't show yet. It closes again as any fold toggled does.
    fn door(&mut self) {
        if self.opened.is_some() {
            return;
        }
        for _ in 0..3 {
            let Some(rows) = self.last.as_ref().map(|last| last.rows.clone()) else {
                return;
            };
            let door = rows
                .iter()
                .rposition(|row| door_of(row) == Some(Step::DoorOpens));
            if let Some(y) = door {
                let Some(fold) = self.fold_on(y) else {
                    return;
                };
                self.toggled.insert(fold);
                self.opened = Some(fold);
                self.press(y, Step::DoorOpens);
                // Now and then the reader scrolls down a little, its first
                // lines still showing or not, and closes it again.
                if self.doors.chance(60) {
                    let by = self.doors.below(u64::from(self.size.1)) as i64;
                    self.app.transcript.scroll(by);
                    self.moved();
                    let rows = self
                        .last
                        .as_ref()
                        .map_or_else(Vec::new, |last| last.rows.clone());
                    let close = rows
                        .iter()
                        .rposition(|row| door_of(row) == Some(Step::DoorCloses));
                    if let Some(y) = close
                        && self.fold_on(y) == Some(fold)
                    {
                        self.toggled.remove(&fold);
                        self.opened = None;
                        self.press(y, Step::DoorCloses);
                    }
                }
                return;
            }
            // (A closed fold that may hold it, which stays open, and the
            // reader's own scroll down to what it opened.)
            let Some(end) = rows.iter().rposition(|row| !row.trim().is_empty()) else {
                return;
            };
            let closed = (end.saturating_sub(8)..=end).rev().find(|y| {
                rows[*y].trim_start_matches(['│', ' ']).starts_with('▸')
                    && self.fold_on(*y).is_some()
            });
            let Some(y) = closed else {
                return;
            };
            if let Some(fold) = self.fold_on(y) {
                self.toggled.insert(fold);
            }
            self.press(y, Step::FoldOpens);
            self.app.transcript.scroll(i64::from(self.size.1 / 2));
            self.moved();
        }
    }

    /// The reader toggles the fold on the transcript's row `y` by a click,
    /// or a content's door now and then by the keyboard, and the frame is
    /// counted under `step`, every line above that row with it. (A door is
    /// its content's own block, or a part of the Quiet row it's in.)
    fn press(&mut self, y: usize, step: Step) {
        let area = self.app.transcript.drawn.area;
        let (filter, theme) = (self.app.filter(), self.app.theme.clone());
        let door = matches!(step, Step::DoorOpens | Step::DoorCloses);
        let fold = self
            .app
            .transcript
            .fold_at(area.x + 3, area.y + y as u16, &filter, &theme);
        match fold.filter(|_| door && self.doors.chance(40)) {
            Some(fold) => {
                self.app.transcript.focus = Some(if fold.block == fold.item {
                    Target::Block(fold.item)
                } else {
                    Target::Inner {
                        row: fold.block,
                        item: fold.item,
                    }
                });
                self.app
                    .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                *self.meter.keyed.entry(step).or_default() += 1;
                self.frame_below(step, Some(y));
                self.app.transcript.focus = None;
            }
            None => {
                self.click(y);
                self.frame_below(step, Some(y));
            }
        }
        self.scroll_up();
    }

    /// The reply streams, from what was live, with its paragraphs and bold
    /// words coming in pieces, and now and then a pause, after which a call
    /// may come before it goes on.
    fn reply(&mut self) {
        let step = match self.live {
            Live::Result => Step::CallReply,
            Live::Thought => Step::ThoughtReply,
            _ => Step::WorkingReply,
        };
        let first = format!(" {}", self.some(1, 4));
        let mut id = self.text(None, first);
        self.calls.clear();
        self.live = Live::Reply;
        self.frame(step);
        let mut open = Open::default();
        for _ in 0..6 + self.rng.below(24) {
            let piece = self.piece(&mut open);
            // (A piece comes in a few chunks, which may end inside a word or
            // a mark.)
            for chunk in self.chunks(&piece) {
                id = self.text(Some(id), chunk);
                self.frame(Step::ReplyGrows);
            }
            if self.meanwhile() {
                // (The text goes on in an item of its own.)
                let first = format!(" {}", self.some(1, 4));
                id = self.text(None, first);
                open = Open::default();
                self.frame(Step::ReplyGrows);
            }
            if self.rng.chance(8) {
                // It pauses, and Working… shows after a while.
                if let Some(close) = open.close() {
                    id = self.text(Some(id), close);
                }
                self.app.transcript.quiet_item(id);
                self.live = Live::Working;
                self.frame(Step::ReplyWorking);
                if self.rng.chance(50) {
                    // A call, and more text after it.
                    self.call();
                    let first = format!(" {}", self.some(1, 4));
                    id = self.text(None, first);
                    self.calls.clear();
                    self.live = Live::Reply;
                    self.frame(Step::CallReply);
                } else {
                    let more = format!(" {}", self.some(1, 4));
                    id = self.text(Some(id), more);
                    self.live = Live::Reply;
                    self.frame(Step::WorkingReply);
                }
            }
        }
        if let Some(close) = open.close() {
            self.text(Some(id), close);
            self.frame(Step::ReplyGrows);
        }
    }

    /// A piece of the reply's Markdown, which is words, a paragraph, bold
    /// words, a list's item, a heading, code or a table's row.
    fn piece(&mut self, open: &mut Open) -> String {
        let words = self.some(1, 6);
        if open.code {
            return match self.rng.below(5) {
                0 => {
                    open.code = false;
                    format!("\n```\n\n{words}")
                }
                _ => format!("\n{words}"),
            };
        }
        match self.rng.below(100) {
            0..=7 => format!("\n\n{words}"),
            8..=17 if open.bold => {
                open.bold = false;
                format!(" {words}**")
            }
            8..=17 => {
                open.bold = true;
                format!(" **{words}")
            }
            18..=23 if !open.bold => {
                let bullet = if self.rng.chance(50) {
                    "-".to_string()
                } else {
                    format!("{}.", 1 + self.rng.below(3))
                };
                format!("\n{bullet} {words}")
            }
            24..=26 if !open.bold => format!("\n\n## {words}\n\n"),
            27..=31 => format!(" `{words}`"),
            32..=35 if !open.bold => {
                open.code = true;
                format!("\n\n```\n{words}")
            }
            36..=37 if !open.bold => {
                let more = self.some(1, 3);
                format!("\n| {words} | {more} |")
            }
            _ => format!(" {words}"),
        }
    }

    /// `piece` in one to three chunks, cut anywhere but between two
    /// digits, so no chunk ends in another word than its own.
    fn chunks(&mut self, piece: &str) -> Vec<String> {
        let cuts: Vec<usize> = piece
            .char_indices()
            .skip(1)
            .filter(|(at, c)| !(c.is_ascii_digit() && piece.as_bytes()[at - 1].is_ascii_digit()))
            .map(|(at, _)| at)
            .collect();
        let mut at: Vec<usize> = (0..self.rng.below(3))
            .filter_map(|_| {
                cuts.get(self.rng.below(cuts.len() as u64) as usize)
                    .copied()
            })
            .collect();
        at.sort_unstable();
        at.dedup();
        let mut out = Vec::new();
        let mut from = 0;
        for cut in at {
            out.push(piece[from..cut].to_string());
            from = cut;
        }
        out.push(piece[from..].to_string());
        out
    }

    /// The agent's text goes on in the item `id`, or a new one, which is
    /// returned.
    fn text(&mut self, id: Option<u64>, text: String) -> u64 {
        match id {
            Some(id) => {
                self.update(Update::Append { id, text });
                id
            }
            None => {
                let item = self.item(Kind::Text, &text);
                self.add(item)
            }
        }
    }

    /// A turn, from the user's message to its end.
    /// A start of a worker, whose row says "starting" until the worker's
    /// first event and then the start's result, which names it, come. Its
    /// words keep their columns as its ID comes.
    fn start_worker(&mut self) {
        let title = self.some(2, 4);
        let first = title.split(' ').next().unwrap_or_default().to_string();
        let mut call = self.item(Kind::Call, &format!("start worker “{title}”"));
        call.status = Some(CallStatus::Running);
        call.tool = Some(crate::crew::START_AGENT.into());
        call.task_title = Some(title.clone());
        call.act = Some(Act {
            doing: "Starting a worker".into(),
            done: "Started a worker".into(),
            verb: "start a worker".into(),
            ..Act::default()
        });
        self.add(call.clone());
        self.calls.push((ActKind::Other, title.clone(), true));
        self.live = Live::Working;
        self.frame(Step::WorkingCall);
        self.workers += 1;
        let worker = format!("r{}", self.workers);
        // The worker's own event comes first, with the feed's line for it,
        // then the start's result.
        let mut line = self.item(Kind::WorkerLine, &title);
        line.worker = Some(worker.clone());
        let summary = crate::remote::WorkerSummary {
            title: title.clone(),
            state: crate::remote::WorkerState::Running,
            ..crate::remote::WorkerSummary::new(&worker)
        };
        self.worker_step(&first, |run| {
            run.add(line);
            run.update(Update::Worker(summary));
        });
        call.status = Some(CallStatus::Ok);
        call.outcome = Some(worker.clone());
        call.started = Some(worker.clone());
        call.sets = Some(worker);
        self.worker_step(&first, |run| run.update(Update::Change(call)));
    }

    /// A step of a worker's start, `step`, and its frame, noting where the
    /// start's first word `first` moved along its row.
    fn worker_step(&mut self, first: &str, step: impl FnOnce(&mut Self)) {
        let before = self
            .last
            .as_ref()
            .and_then(|last| place_of(&last.rows, first));
        step(self);
        self.frame(Step::WorkerStarts);
        let after = self
            .last
            .as_ref()
            .and_then(|last| place_of(&last.rows, first));
        if let (Some((_, from)), Some((_, to))) = (before, after)
            && from != to
        {
            let rows = self.last.as_ref().map(|last| last.rows.join("\n"));
            self.meter.sideways.push(format!(
                "{}×{} {:?}: {first} went from column {from} to {to}\n{}",
                self.size.0,
                self.size.1,
                View::ALL[self.view],
                rows.unwrap_or_default()
            ));
        }
    }

    fn turn(&mut self) {
        self.scroll_up();
        self.turn += 1;
        let n = 3 + self.rng.below(12);
        let text = format!("Please {}", self.words(n));
        let mut user = self.item(Kind::User, &text);
        user.delivery = Some(Delivery::Turn);
        self.add(user);
        self.works(true);
        self.live = Live::Working;
        self.folder = format!("src/{}", self.name('d'));
        self.calls.clear();
        self.frame(Step::Asked);
        self.meanwhile();
        if self.starts && self.rng.chance(40) {
            self.start_worker();
        }
        for _ in 0..1 + self.rng.below(3) {
            if self.rng.chance(60) {
                self.think();
            }
            for _ in 0..self.rng.below(6) {
                if !self.calls.is_empty() && self.rng.chance(40) {
                    self.think();
                }
                self.call();
            }
            if self.rng.chance(30) {
                self.read_waiting();
            }
            if self.rng.chance(20) {
                self.think();
            }
            self.reply();
        }
        self.read_waiting();
        let ended = self.words(1);
        let end = self.item(Kind::Turn, &format!("{ended} s · $0.01"));
        self.add(end);
        self.works(false);
        self.frame(Step::TurnEnds);
        if self.opened.is_some() {
            self.toggle();
        }
    }
}

/// The sizes of the screen the meter plays at, the first three in the
/// test's own run.
const SIZES: [(u16, u16); 6] = [
    (60, 24),
    (140, 40),
    (100, 30),
    (100, 24),
    (60, 40),
    (140, 24),
];

/// Plays sessions in every view, with the reader following and with the
/// reader scrolled up, the agent's thinking shown folded and shown whole.
/// `seeds` of them at every size, else one at each of the first three
/// sizes, its thinking folded or whole by turns.
fn measure(seeds: Option<u64>) -> Meter {
    measure_with(seeds, false)
}

/// [`measure`], with turns that start workers when `starts`.
fn measure_with(seeds: Option<u64>, starts: bool) -> Meter {
    let _full = choose_symbols(Symbols::Full);
    let mut meter = Meter::default();
    let (sizes, turns) = match seeds {
        Some(_) => (&SIZES[..], 5),
        None => (&SIZES[..3], 3),
    };
    for seed in 0..seeds.unwrap_or(1) {
        for (at, size) in sizes.iter().enumerate() {
            for view in 0..View::ALL.len() {
                let whole = match seeds {
                    Some(_) => vec![false, true],
                    None => vec![(at + view) % 2 == 1],
                };
                for thinking in whole {
                    for scrolled in [false, true] {
                        let seed = 1
                            + seed * 1000
                            + at as u64 * 20
                            + view as u64 * 4
                            + u64::from(thinking) * 2
                            + u64::from(scrolled);
                        let mut run = Run::new(seed, view, *size, scrolled, &mut meter);
                        run.starts = starts;
                        run.together = seeds.is_some();
                        // (The workers' sessions play as they did before
                        // code runs, which the main one plays.)
                        run.code_runs = !starts;
                        run.app.transcript.set_open_thinking(thinking);
                        for _ in 0..turns {
                            run.turn();
                        }
                    }
                }
            }
        }
    }
    meter
}

/// Following a session, no line on the screen moves down from one frame
/// to the next, and scrolled up none moves at all, through every live step
/// of a turn in every view, but for the moves the meter allows.
#[test]
fn no_line_moves_down_as_an_agent_works() {
    let seeds = std::env::var("SHIFT_METER_SEEDS")
        .ok()
        .and_then(|seeds| seeds.parse().ok());
    let meter = measure(seeds);
    let table = format!(
        "{} frames, {} moved a line. Each cell is frames · frames that moved a line · the most rows / all rows.\n\n{}\n{}\n{}",
        meter.frames(),
        meter.shifted(),
        meter.table(),
        meter.size_table(),
        meter.cause_table()
    );
    let table = format!(
        "{table}\nThe moves the test allows. \"Working…\" and a note waiting at the end go down as what comes above them takes their rows, into the blank rows below the end, and while the reader reads on past the bottom row, and up as what stood above them goes. Rows that came after an open call go down by the rows its content grows by above them as it comes, a command's output in Full and a code run's calls and its result in any view where the run is open. Each cell is frames · the most rows / all rows.\n\n{}",
        meter.allowed_table()
    );
    if let Ok(path) = std::env::var("SHIFT_METER_OUT") {
        let examples = meter.examples();
        std::fs::write(
            &path,
            format!("{table}\n## The largest move of each cause, step and view\n\n{examples}"),
        )
        .unwrap();
    }
    assert!(meter.frames() > 1500, "{} frames", meter.frames());
    assert_eq!(meter.shifted(), 0, "\n{table}\n{}", meter.examples());
    assert!(meter.aside.is_empty(), "{}", meter.aside.join("\n\n"));
    // Doors opened and closed, by the mouse and the keyboard, in Full and
    // inside open Quiet rows.
    for step in [Step::DoorOpens, Step::DoorCloses] {
        for view in [View::Full, View::Quiet] {
            let at = View::ALL.iter().position(|each| *each == view).unwrap();
            let frames: u32 = [Reader::Following, Reader::Up]
                .into_iter()
                .filter_map(|reader| meter.tallies.get(&(step, at, reader)))
                .map(|tally| tally.frames)
                .sum();
            assert!(frames > 0, "{} in {view:?}\n{table}", step.name());
        }
        assert!(
            meter.keyed.get(&step).is_some_and(|n| *n > 0),
            "{}",
            step.name()
        );
    }
}

/// A note the agent reads in the frame its call's result comes goes down by
/// the result's lines and no further, following and scrolled up. (The
/// frames are those of the Full view with blank rows below the end.)
#[test]
fn a_read_note_goes_down_by_the_output_above_it_and_no_further() {
    let rows = |rows: &[&str]| -> Vec<String> { rows.iter().map(|row| row.to_string()).collect() };
    let call = "▾ $ cargo w517 w518 · ✓ exit 0";
    let content = "│ w519 w520 w521 ok";
    let before = rows(&[
        call,
        content,
        "",
        "· $ cargo w522 w523 …",
        "",
        "▎ Also w524 w525 w526 w527 w528 w529 w530            pending",
        "",
        "",
        "",
        "",
        "",
        "",
    ]);
    let after = |blank: usize| {
        let mut after = rows(&[
            call,
            content,
            "",
            "▾ $ cargo w522 w523 · ✓ exit 0",
            "│ w531 w532 w533 ok",
            "│ w534 w535 w536 ok",
            "",
        ]);
        after.extend((0..blank).map(|_| String::new()));
        after.extend(rows(&[
            "▎ Also w524 w525 w526 w527 w528 w529 w530",
            "",
            "⣻ Working…",
        ]));
        after.resize(before.len(), String::new());
        after
    };
    for following in [true, false] {
        // The result's two lines push the note down two rows.
        let moved = moves(&before, &after(0), following, None, (4, 8), None);
        assert_eq!((moved.rows, moved.room), (0, 2), "following {following}");
        // Two blank rows more are a move of four.
        let moved = moves(&before, &after(2), following, None, (4, 10), None);
        assert_eq!(moved.rows, 4, "following {following}");
    }
}

/// A row that came after an open call goes down by the rows the call's
/// content grew by above it, and that is allowed, following and scrolled
/// up. A row down by a row more, a row of the call itself that moves, and
/// one above the call that does count as moves.
#[test]
fn a_row_after_an_open_call_goes_down_by_its_content_and_no_further() {
    let rows = |rows: &[&str]| -> Vec<String> { rows.iter().map(|row| row.to_string()).collect() };
    let before = rows(&[
        "  w601 w602",
        "▾ run_code w603 w604 …",
        "│  w605 w606",
        "│ · $ cargo w607 w608 · ✓ 2 lines",
        "",
        "! w609 w610",
        "",
        "",
        "",
    ]);
    let was = [false, true, true, true, false, false, false, false, false];
    let after = rows(&[
        "  w601 w602",
        "▾ run_code w603 w604 …",
        "│  w605 w606",
        "│ · $ cargo w607 w608 · ✓ 2 lines",
        "│ · $ cargo w611 w612 …",
        "",
        "! w609 w610",
        "",
        "",
    ]);
    let now = [false, true, true, true, true, false, false, false, false];
    for following in [true, false] {
        let moved = moves(&before, &after, following, None, (9, 9), Some((&was, &now)));
        assert_eq!((moved.rows, moved.arrived), (0, 1), "following {following}");
        // Without the call's rows, it's a move.
        let moved = moves(&before, &after, following, None, (9, 9), None);
        assert_eq!(moved.rows, 1, "following {following}");
        // A row more.
        let mut further = after.clone();
        further.insert(6, String::new());
        further.pop();
        let moved = moves(
            &before,
            &further,
            following,
            None,
            (9, 9),
            Some((&was, &now)),
        );
        assert_eq!(moved.rows, 2, "following {following}");
        // The call's code moves down inside it.
        let mut inside = after.clone();
        inside.insert(2, "│  w613".into());
        inside.pop();
        let grew = [false, true, true, true, true, true, false, false, false];
        let moved = moves(
            &before,
            &inside,
            following,
            None,
            (9, 9),
            Some((&was, &grew)),
        );
        assert!(moved.rows > 0, "following {following}");
        // A row above the call moves.
        let mut above = after.clone();
        above.insert(0, String::new());
        above.pop();
        let shifted = [false, false, true, true, true, true, false, false, false];
        let moved = moves(
            &before,
            &above,
            following,
            None,
            (9, 9),
            Some((&was, &shifted)),
        );
        assert!(moved.rows > 0, "following {following}");
    }
}

/// A message the agent reads in the frame its call's result comes, in Full
/// with blank rows below the end, goes down by the result's lines as it did
/// while it waited, which is the end making room, following and scrolled
/// up.
#[test]
fn a_message_read_as_its_calls_result_comes_moves_with_the_end() {
    let _full = choose_symbols(Symbols::Full);
    let full = View::ALL
        .iter()
        .position(|view| *view == View::Full)
        .unwrap();
    for scrolled in [false, true] {
        let mut meter = Meter::default();
        let mut run = Run::new(1, full, (60, 40), scrolled, &mut meter);
        run.turn = 1;
        run.works(true);
        for _ in 0..6 {
            let text = run.words(40);
            let said = run.item(Kind::Text, &text);
            run.add(said);
        }
        // A tall text that goes again, after the reader scrolls up, so
        // blank rows stand below the end.
        let text = run.words(200);
        let tall = run.item(Kind::Text, &text);
        let tall = run.add(tall);
        run.moved();
        if scrolled {
            run.app.transcript.scroll(-3);
            run.moved();
        }
        run.update(Update::Remove { id: tall });
        run.moved();
        let target = format!("cargo {}", run.words(2));
        let mut call = run.item(Kind::Call, &format!("$ {target}"));
        call.status = Some(CallStatus::Running);
        call.act = Some(Act {
            kind: ActKind::Command,
            target,
            ran: true,
            ..Act::default()
        });
        let id = run.add(call.clone());
        run.frame(Step::WorkingCall);
        let text = format!("Also {}", run.words(6));
        let mut note = run.item(Kind::User, &text);
        note.delivery = Some(Delivery::Note);
        note.pending = true;
        let note = run.add(note);
        run.waiting = Some((note, text));
        run.frame(Step::Waits);
        call.status = Some(CallStatus::Ok);
        call.outcome = Some("exit 0".into());
        run.update(Update::Change(call));
        let output = format!("{} ok\n{} ok", run.words(3), run.words(3));
        let mut result = run.item(Kind::Result, &output);
        result.call = Some(id);
        result.content = Some(ContentKind::Plain);
        run.add(result);
        run.read_note();
        run.frame(Step::ResultRead);
        let shown = run
            .last
            .as_ref()
            .map(|shot| shot.rows.join("\n"))
            .unwrap_or_default();
        let state = (
            run.app.transcript.end_shows(),
            run.app.transcript.gap(),
            run.app.transcript.following(),
        );
        assert!(
            state.0 && state.1 > 0,
            "scrolled {scrolled}: {state:?}\n{shown}"
        );
        assert_eq!(state.2, !scrolled, "{shown}");
        drop(run);
        assert_eq!(
            meter.shifted(),
            0,
            "\n{}\n{}",
            meter.table(),
            meter.examples()
        );
        let room: u32 = meter
            .allowed
            .iter()
            .filter(|((_, step, _), _)| *step == Step::ResultRead)
            .map(|(_, tally)| tally.max)
            .sum();
        assert!(room > 0, "scrolled {scrolled}\n{}", meter.allowed_table());
    }
}

/// A worker's start, which says "starting" until the worker's first event
/// and the start's result come, moves no line down as they come, and its
/// words keep their columns as the worker's ID comes, in every view, at
/// every size, following and scrolled up.
#[test]
fn a_worker_start_keeps_its_words_in_place() {
    let meter = measure_with(None, true);
    let starts: Vec<&Tally> = meter
        .tallies
        .iter()
        .filter(|((step, ..), _)| *step == Step::WorkerStarts)
        .map(|(_, tally)| tally)
        .collect();
    assert!(!starts.is_empty(), "no worker start");
    assert!(
        starts.iter().all(|tally| tally.shifted == 0),
        "\n{}\n{}",
        meter.table(),
        meter.examples()
    );
    assert!(meter.sideways.is_empty(), "{}", meter.sideways.join("\n\n"));
}

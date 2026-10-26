//! The transcript: the feed's items as the reader sees them. It keeps the
//! items (and the workers' summaries) as the feed sends them, and draws
//! them through a view: a filter and grouping over the items, so changing
//! the view changes no item.
//!
//! - **Full**: everything, the workers' calls inline.
//! - **Quiet**: the main agent's work; each worker is one line that
//!   changes in place.
//! - **Messages**: what was said (the user's messages, the agent's replies,
//!   messages and questions, warnings and errors) and the workers' lines;
//!   each turn's tool activity is one line that counts it.
//! - **A worker's own** (the worker view): its text, calls, results and
//!   report.
//!
//! The reading position is the reader's: while they are at the bottom the
//! transcript follows what comes; once they scroll up it stays where it is,
//! anchored on the item at the top, however much comes below, and a marker
//! counts what is new.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::markdown;
use super::motion;
use super::theme::Theme;
use super::wrap::{self, Join, Wrapped, width as text_width};
use crate::glyphs;
use crate::remote::{
    CallStatus, Delivery, Item, Kind, Level, MAIN_AGENT, Snapshot, Source, Update, WorkerState,
    WorkerSummary,
};

/// The items kept, as the feed keeps them: the main agent's and the
/// workers' apart.
const KEPT: usize = 3000;

/// How much of the work the transcript shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Full,
    Quiet,
    Messages,
}

impl View {
    pub const ALL: [View; 3] = [View::Full, View::Quiet, View::Messages];

    pub fn name(self) -> &'static str {
        match self {
            View::Full => "full",
            View::Quiet => "quiet",
            View::Messages => "messages",
        }
    }

    pub fn parse(name: &str) -> Option<View> {
        View::ALL
            .into_iter()
            .find(|view| view.name().eq_ignore_ascii_case(name.trim()))
    }

    /// The next view, as the key that cycles them goes.
    pub fn next(self) -> View {
        match self {
            View::Full => View::Quiet,
            View::Quiet => View::Messages,
            View::Messages => View::Full,
        }
    }
}

/// About this many bytes of commands' whole outputs are kept, the latest
/// (as the feed keeps them): an older command's block keeps its first lines.
const WHOLES_KEPT: usize = 8 * 1024 * 1024;

/// An item, with its lines as last drawn.
struct Entry {
    item: Item,
    cache: Option<Cached>,
    /// A command's block shows its whole output, or an agent's thinking
    /// shows whole (a click opened it).
    open: bool,
    /// When it came, or last changed (text streamed into it).
    changed: Instant,
}

impl Entry {
    fn new(item: Item) -> Self {
        Self {
            item,
            cache: None,
            open: false,
            changed: Instant::now(),
        }
    }
}

struct Cached {
    /// What the lines were drawn for.
    width: usize,
    style: u64,
    /// A running call's time.
    clock: Option<String>,
    lines: Vec<Wrapped>,
}

pub use crate::ui::Banner;

/// What a block of the drawn transcript is.
#[derive(Debug, Clone, PartialEq)]
enum Shown {
    /// The session's banner, first.
    Banner,
    /// An entry's lines.
    Entry(usize),
    /// A worker's summary, where it started (`worker_line`).
    Worker(String),
    /// A worker view's heading: the worker's task.
    Heading(String),
    /// A turn's tool activity, counted (the Messages view).
    Counter {
        calls: usize,
        failed: usize,
        running: usize,
    },
    /// "Working…", at the end, while the agent waits on its model and
    /// nothing else shows it.
    Working,
}

/// The key of the "Working…" line: after every item's.
const WORKING_KEY: u64 = u64::MAX;

/// Text that hasn't grown for this long, while its agent works, no longer
/// shows that the agent is under way (it may be writing a long call).
const QUIET: Duration = Duration::from_secs(2);

/// What an agent that works is under way with, as its latest items say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    /// It waits on its model, and nothing the model sent shows it yet: a
    /// turn began, a call's result went back, an answer came.
    Model,
    /// Its thinking streams (the item's ID).
    Thinking(u64),
    /// Its text streams, last grown at this moment.
    Text(Instant),
    /// A call of its runs, which shows it.
    Tool,
    /// It replied.
    Done,
}

/// What shows that a view's agent is under way, besides its calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Motion {
    /// Its thinking that streams, by ID: its label moves.
    thinking: Option<u64>,
    /// The "Working…" line, at the end.
    working: bool,
}

/// A block of lines in the drawn transcript.
struct Block {
    /// The ID of its (first) item: where the reading position anchors.
    key: u64,
    shown: Shown,
    /// A blank line before it.
    gap: bool,
    lines: usize,
    /// A blank line after it (the "Working…" line has one, at the end).
    tail: bool,
}

impl Block {
    fn height(&self) -> usize {
        self.lines + usize::from(self.gap) + usize::from(self.tail)
    }
}

/// Where the reader is: the block at the top of the screen, by its key,
/// and how many of its lines are above the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anchor {
    key: u64,
    /// Lines of the block above the top, as a share of its height (so a
    /// re-wrap at another width keeps about the same place).
    offset: usize,
    height: usize,
}

/// A row of the screen: the block it shows, and which of its lines (the
/// blank line before a block, when it has one, is line 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowRef {
    key: u64,
    line: usize,
    /// The blank line before the block.
    gap: bool,
}

/// A place in the transcript's text: a block (by its key), a line of it,
/// and a column. It stays on its text as the screen scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub key: u64,
    pub line: usize,
    pub col: usize,
}

/// Text the reader highlighted with the mouse: from where the drag began
/// (`anchor`) to where it is (`head`), in the view and at the width it was
/// made in.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub anchor: Point,
    pub head: Point,
    filter: Filter,
    width: usize,
}

/// What was drawn where, for the mouse.
#[derive(Debug, Clone, Default)]
pub struct Drawn {
    /// The transcript's area.
    pub area: Rect,
    /// What each row of the area shows.
    rows: Vec<Option<RowRef>>,
    /// Where the screen's top was: what holding the view still keeps.
    top: Option<Anchor>,
    /// The "new" marker's area, while it shows.
    pub marker: Option<Rect>,
    /// The keys of the counted turns' lines.
    counters: Vec<u64>,
    /// Something on the screen moves: it is drawn again at the next frame.
    pub moving: bool,
}

impl Drawn {
    /// The block on the screen's row `y` (not the blank line before it).
    pub fn key_at(&self, x: u16, y: u16) -> Option<u64> {
        self.row_at(x, y).map(|row| row.key)
    }

    /// The block's line on the screen's row `y` (not the blank line
    /// before it).
    fn row_at(&self, x: u16, y: u16) -> Option<RowRef> {
        if !self.area.contains(ratatui::layout::Position { x, y }) {
            return None;
        }
        self.rows
            .get((y - self.area.y) as usize)
            .copied()
            .flatten()
            .filter(|row| !row.gap)
    }
}

/// The transcript.
pub struct Transcript {
    entries: VecDeque<Entry>,
    workers: Vec<WorkerSummary>,
    /// `None` while following the bottom.
    anchor: Option<Anchor>,
    /// The view's height in lines when following stopped (and the view it
    /// was): the marker counts the lines that came since.
    baseline: Option<(Filter, usize)>,
    /// The session's banner, which opens the main transcript.
    pub banner: Option<Banner>,
    /// The text highlighted with the mouse.
    pub selection: Option<Selection>,
    /// The view holds still (a drag selects text): it doesn't follow new
    /// output, even at the bottom.
    hold: bool,
    /// The lines drawn last, by row.
    pub drawn: Drawn,
    /// Lines a scroll moved while the screen was not drawn yet.
    pending_scroll: i64,
    /// A fold was opened or closed: the next draw keeps its block's last
    /// line (the one clicked) on this row of the area.
    pin: Option<(u64, usize)>,
    /// The agents' thinking shows whole from the start (`--show-thinking`),
    /// instead of folded to its first line.
    open_thinking: bool,
    /// Since when the main agent works (Unix milliseconds), while it isn't
    /// waiting for the user's answer.
    pub working: Option<i64>,
    /// What showed that the agent was under way, as the blocks were last
    /// made.
    motion: Motion,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            workers: Vec::new(),
            anchor: None,
            baseline: None,
            banner: None,
            selection: None,
            hold: false,
            drawn: Drawn::default(),
            pending_scroll: 0,
            pin: None,
            open_thinking: false,
            working: None,
            motion: Motion::default(),
        }
    }

    /// Shows the agents' thinking whole (`open`) or folded, from now on and
    /// in the items already there.
    pub fn set_open_thinking(&mut self, open: bool) {
        self.open_thinking = open;
        for entry in &mut self.entries {
            if entry.item.kind == Kind::Thinking {
                entry.open = open;
                entry.cache = None;
            }
        }
    }

    /// An entry for `item`: an agent's thinking opens as `open_thinking`
    /// says.
    fn entry(&self, item: Item) -> Entry {
        let item = for_terminal(item);
        let open = self.open_thinking && item.kind == Kind::Thinking;
        Entry {
            open,
            ..Entry::new(item)
        }
    }

    /// Starts over from the feed's `snapshot` (when a client starts, or
    /// fell behind): the reading position and selection stay on their
    /// items.
    pub fn reset(&mut self, snapshot: Snapshot) {
        self.entries = snapshot
            .items
            .into_iter()
            .map(|item| self.entry(item))
            .collect();
        self.workers = snapshot.workers;
        self.trim();
    }

    /// Takes the feed's `update`.
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Item(item) => {
                let entry = self.entry(item);
                self.entries.push_back(entry);
                self.trim();
            }
            // (Text added at an item's end leaves what is highlighted
            // where it is: a drag over streaming text still copies it.)
            Update::Append { id, text } => {
                if let Some(entry) = self.find_mut(id) {
                    entry.item.text.push_str(&text);
                    entry.cache = None;
                    entry.changed = Instant::now();
                }
            }
            Update::Change(item) => {
                self.changed(item.id);
                if let Some(entry) = self.find_mut(item.id) {
                    entry.item = for_terminal(item);
                    entry.cache = None;
                    entry.changed = Instant::now();
                }
            }
            Update::Worker(summary) => match self.workers.iter_mut().find(|w| w.id == summary.id) {
                Some(worker) => *worker = summary,
                None => self.workers.push(summary),
            },
            Update::WorkerEnded { .. } => {}
            Update::Activity => {}
            // The main agent's items from `from` on are gone.
            Update::Truncate { from } => {
                self.entries
                    .retain(|entry| entry.item.id < from || !entry.item.is_main());
                self.selection = None;
            }
            Update::Remove { id } => {
                self.changed(id);
                self.entries.retain(|entry| entry.item.id != id);
            }
            // A moved call's line follows its job through its changes.
            Update::Job(_) => {}
        }
    }

    /// The item `id` changes (a call's outcome came): a selection over it
    /// may no longer be on the text it highlighted, so it goes.
    fn changed(&mut self, id: u64) {
        if let Some(selection) = &self.selection {
            let (a, b) = (selection.anchor.key, selection.head.key);
            if a.min(b) <= id && id <= a.max(b) {
                self.selection = None;
            }
        }
    }

    /// Keeps as many items as the feed does, the main agent's and the
    /// workers' each, and the latest whole outputs.
    fn trim(&mut self) {
        let mut wholes = 0;
        for entry in self.entries.iter_mut().rev() {
            wholes += entry.item.whole_size();
            if wholes > WHOLES_KEPT && entry.item.whole.take().is_some() {
                entry.open = false;
                entry.cache = None;
            }
        }
        let main = self.entries.iter().filter(|e| e.item.is_main()).count();
        let mut drop_main = main.saturating_sub(KEPT);
        let mut drop_workers = (self.entries.len() - main).saturating_sub(KEPT);
        if drop_main + drop_workers == 0 {
            return;
        }
        self.entries.retain(|entry| {
            let drop = if entry.item.is_main() {
                &mut drop_main
            } else {
                &mut drop_workers
            };
            if *drop > 0 {
                *drop -= 1;
                false
            } else {
                true
            }
        });
    }

    fn index(&self, id: u64) -> Option<usize> {
        self.entries.binary_search_by_key(&id, |e| e.item.id).ok()
    }

    fn find_mut(&mut self, id: u64) -> Option<&mut Entry> {
        let index = self.index(id)?;
        self.entries.get_mut(index)
    }

    /// The item `id`, while it is kept.
    pub fn item(&self, id: u64) -> Option<&Item> {
        self.index(id).map(|index| &self.entries[index].item)
    }

    pub fn workers(&self) -> &[WorkerSummary] {
        &self.workers
    }

    pub fn worker(&self, id: &str) -> Option<&WorkerSummary> {
        self.workers.iter().find(|worker| worker.id == id)
    }

    /// The main agent's last reply.
    pub fn last_reply(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.item.is_main() && e.item.kind == Kind::Reply)
            .map(|e| e.item.text.as_str())
    }

    /// Whether the transcript follows the bottom.
    pub fn following(&self) -> bool {
        self.anchor.is_none()
    }

    /// Follows the bottom again.
    pub fn follow(&mut self) {
        self.anchor = None;
        self.pending_scroll = 0;
    }

    /// Scrolls by `lines` (up when negative) at the next draw.
    pub fn scroll(&mut self, lines: i64) {
        self.pending_scroll += lines;
    }

    /// Shows the item `id` at the top (a counted turn's first call, when
    /// the Full view opens on it).
    pub fn show(&mut self, id: u64) {
        self.anchor = Some(Anchor {
            key: id,
            offset: 0,
            height: 0,
        });
        self.pending_scroll = 0;
    }

    /// Whether the block `key` (as drawn last) is a turn's counted calls.
    pub fn is_counter(&self, key: u64) -> bool {
        self.drawn.counters.contains(&key)
    }

    /// Makes the latest item's text look as if it stopped growing a while
    /// ago (tests).
    #[cfg(test)]
    pub(super) fn quiet_text(&mut self) {
        if let Some(entry) = self.entries.back_mut() {
            entry.changed -= QUIET;
        }
    }

    /// Scrolls to the top at the next draw.
    pub fn top(&mut self) {
        self.pending_scroll = i64::MIN / 2;
    }

    /// What the agent `agent` is under way with while it works (its turn
    /// began at `since`, in Unix milliseconds), as its latest items say.
    fn busy(&self, agent: &str, since: i64) -> Busy {
        // The calls of one response may run side by side: one that ended
        // doesn't mean the others did.
        let mut after_call = false;
        for entry in self.entries.iter().rev() {
            let item = &entry.item;
            if item.agent != agent || item.pending {
                continue;
            }
            match item.kind {
                Kind::Call if item.status == Some(CallStatus::Running) => return Busy::Tool,
                Kind::Call | Kind::Result => after_call = true,
                // Arko's lines, a command's output, the workers' places and
                // what the user typed beside the agent's work.
                Kind::Notice | Kind::Output | Kind::WorkerLine => {}
                Kind::User
                    if matches!(item.delivery, Some(Delivery::Command | Delivery::Queued)) => {}
                _ if after_call => return Busy::Model,
                Kind::Thinking => return Busy::Thinking(item.id),
                Kind::Text => return Busy::Text(entry.changed),
                Kind::Reply => return Busy::Done,
                // The end of a turn before this one began (a turn without a
                // message, when the workers' news came), or of this one.
                Kind::Turn if item.at < since => return Busy::Model,
                Kind::Turn => return Busy::Done,
                _ => return Busy::Model,
            }
        }
        Busy::Model
    }

    /// What shows that the agent of `filter`'s view is under way, besides
    /// its calls: its thinking as it streams, or else "Working…" while it
    /// waits on its model with nothing else to show.
    fn motion(&self, filter: &Filter) -> Motion {
        let (agent, since) = match filter {
            Filter::Main(_) => (MAIN_AGENT, self.working),
            Filter::Worker(id) => (
                id.as_str(),
                self.worker(id)
                    .filter(|worker| worker.state == WorkerState::Running)
                    .map(|_| i64::MIN),
            ),
        };
        let Some(since) = since else {
            return Motion::default();
        };
        // (The Messages view leaves the agent's thinking out.)
        let thinking_shown = !matches!(filter, Filter::Main(View::Messages));
        let working = |working| Motion {
            working,
            ..Motion::default()
        };
        match self.busy(agent, since) {
            Busy::Model => working(true),
            Busy::Text(grown) => working(grown.elapsed() >= QUIET),
            Busy::Thinking(id) if thinking_shown => Motion {
                thinking: Some(id),
                working: false,
            },
            Busy::Thinking(_) => working(true),
            Busy::Tool | Busy::Done => Motion::default(),
        }
    }

    /// The blocks of `filter`'s view of the items, at `width`.
    fn blocks(&mut self, filter: &Filter, theme: &Theme, width: usize) -> Vec<Block> {
        // A worker's own view doesn't name it on every line.
        let own = matches!(filter, Filter::Worker(_));
        let style = theme_key(theme) ^ u64::from(own);
        let mut blocks: Vec<Block> = Vec::new();
        // (Key 0: before every item. It scrolls away with them.)
        if let (Filter::Main(_), Some(banner)) = (filter, &self.banner) {
            blocks.push(Block {
                key: 0,
                shown: Shown::Banner,
                gap: false,
                lines: banner_lines(banner, theme, width).len(),
                tail: false,
            });
        }
        if let Filter::Worker(id) = filter
            && let Some(worker) = self.worker(id)
        {
            blocks.push(Block {
                key: 0,
                shown: Shown::Heading(worker.id.clone()),
                gap: false,
                lines: heading_lines(worker, theme, width).len(),
                tail: false,
            });
        }
        // The Messages view counts each turn's tool activity on one line,
        // where the activity began.
        let mut counters: HashMap<Option<u64>, usize> = HashMap::new();
        let mut last_group: Option<Group> = blocks.first().map(|_| Group::Prose);
        let now = chrono::Utc::now().timestamp_millis();
        for index in self.order() {
            let item = &self.entries[index].item;
            let shown = match filter.show(item) {
                Show::Hidden => continue,
                Show::Count => {
                    let turn = item.turn;
                    let failed = usize::from(item.status == Some(CallStatus::Failed));
                    let running = usize::from(item.status == Some(CallStatus::Running));
                    match counters.get(&turn) {
                        Some(&at) => {
                            if let Shown::Counter {
                                calls,
                                failed: failures,
                                running: runs,
                            } = &mut blocks[at].shown
                            {
                                *calls += 1;
                                *failures += failed;
                                *runs += running;
                            }
                            continue;
                        }
                        None => {
                            counters.insert(turn, blocks.len());
                            Shown::Counter {
                                calls: 1,
                                failed,
                                running,
                            }
                        }
                    }
                }
                Show::Worker(id) => Shown::Worker(id),
                Show::Item => Shown::Entry(index),
            };
            let group = match &shown {
                Shown::Entry(_) if item.kind == Kind::Output => Group::Output,
                Shown::Entry(_) => group(item),
                _ => Group::Activity,
            };
            // A command's output that comes in lines reads as one block.
            let output = matches!(&shown, Shown::Entry(_)) && item.kind == Kind::Output;
            let gap = match last_group {
                None => false,
                Some(Group::Output) if output => false,
                Some(last) => !(last == Group::Activity && group == Group::Activity),
            };
            last_group = Some(group);
            let lines = match &shown {
                Shown::Entry(index) => {
                    // A worker's notice names the worker that started it.
                    let under = match self.entries[*index].item.kind {
                        Kind::Notice => self
                            .worker(&self.entries[*index].item.agent)
                            .and_then(|worker| worker.parent.clone()),
                        _ => None,
                    };
                    let entry = &mut self.entries[*index];
                    let clock = running_time(&entry.item, now);
                    let fresh = entry
                        .cache
                        .as_ref()
                        .is_some_and(|c| c.width == width && c.style == style && c.clock == clock);
                    if !fresh {
                        entry.cache = Some(Cached {
                            width,
                            style,
                            lines: entry_lines(
                                &entry.item,
                                theme,
                                width,
                                own,
                                under.as_deref(),
                                entry.open,
                                clock.as_deref(),
                            ),
                            clock,
                        });
                    }
                    entry.cache.as_ref().map_or(0, |c| c.lines.len())
                }
                _ => 1,
            };
            blocks.push(Block {
                key: self.entries[index].item.id,
                shown,
                gap,
                lines,
                tail: false,
            });
        }
        self.motion = self.motion(filter);
        if self.motion.working {
            // Where the agent's output comes next: above the notes it hasn't
            // read, which wait at the end.
            let mut at = blocks.len();
            while at > 0
                && matches!(blocks[at - 1].shown, Shown::Entry(index) if self.entries[index].item.pending)
            {
                at -= 1;
            }
            // It has as many blank lines below as above, whatever started the
            // turn. Among the agent's own calls (a call, its result, its
            // thinking, a worker's line) it is one more row of that list,
            // which runs together and stands against the status line's rule,
            // none on either side. After anything else (the user's message,
            // the agent's text, a message, a notice, a command's output) it
            // is a paragraph of its own, a blank line on each side. The notes
            // it stands above (they wait at the end) set it apart below with
            // their own blank line, so it has one above too.
            let notes_below = at < blocks.len();
            let among_calls = at
                .checked_sub(1)
                .is_some_and(|last| self.is_call_run(&blocks[last]));
            let apart = at > 0 && (notes_below || !among_calls);
            blocks.insert(
                at,
                Block {
                    key: WORKING_KEY,
                    shown: Shown::Working,
                    gap: apart,
                    lines: 1,
                    tail: apart && !notes_below,
                },
            );
        }
        blocks
    }

    /// Whether `block` is part of the agent's run of calls: a call, its
    /// result, its thinking, a worker's line or a turn's count of calls.
    fn is_call_run(&self, block: &Block) -> bool {
        match &block.shown {
            Shown::Entry(index) => matches!(
                self.entries[*index].item.kind,
                Kind::Call | Kind::Result | Kind::Thinking
            ),
            Shown::Worker(_) | Shown::Counter { .. } => true,
            _ => false,
        }
    }

    /// The entries in the order they are drawn: by ID, except that a
    /// result's block comes right under its call (other items, such as a
    /// worker's, may have come between them), and a note the agent hasn't
    /// read waits at the end of its conversation, below the agent's output.
    fn order(&self) -> Vec<usize> {
        let mut under: HashMap<u64, Vec<usize>> = HashMap::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.item.kind == Kind::Result
                && let Some(call) = entry.item.call
                && self.index(call).is_some()
            {
                under.entry(call).or_default().push(index);
            }
        }
        let mut order = Vec::with_capacity(self.entries.len());
        let mut waiting = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let placed = entry.item.kind == Kind::Result
                && entry
                    .item
                    .call
                    .is_some_and(|call| under.contains_key(&call));
            if placed {
                continue;
            }
            if entry.item.pending {
                waiting.push(index);
                continue;
            }
            if entry.item.kind == Kind::Conversation {
                order.append(&mut waiting);
            }
            order.push(index);
            if let Some(results) = under.get(&entry.item.id) {
                order.extend(results);
            }
        }
        order.append(&mut waiting);
        order
    }

    /// Draws `filter`'s view into `area` of `buffer`.
    pub fn draw(&mut self, buffer: &mut Buffer, area: Rect, filter: &Filter, theme: &Theme) {
        let width = area.width as usize;
        let height = area.height as usize;
        // A selection made in another view, or at another width (its lines
        // wrapped otherwise), is gone.
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.filter != *filter || selection.width != width)
        {
            self.selection = None;
        }
        let blocks = self.blocks(filter, theme, width);
        let total: usize = blocks.iter().map(Block::height).sum();
        // The lines of text: "Working…" comes and goes without being new.
        let text_lines = total
            - blocks
                .iter()
                .filter(|block| block.shown == Shown::Working)
                .map(Block::height)
                .sum::<usize>();
        let bottom = total.saturating_sub(height);
        // Where each block starts.
        let mut starts = Vec::with_capacity(blocks.len());
        let mut at = 0;
        for block in &blocks {
            starts.push(at);
            at += block.height();
        }
        let mut top = match self.anchor {
            None => bottom,
            Some(anchor) => {
                let index = blocks
                    .iter()
                    .position(|block| block.key == anchor.key)
                    .or_else(|| blocks.iter().position(|block| block.key > anchor.key));
                match index {
                    Some(index) => {
                        let block = &blocks[index];
                        // The same share of the block, if it was re-wrapped.
                        let offset = if anchor.height == 0 || block.height() == anchor.height {
                            anchor.offset
                        } else {
                            anchor.offset * block.height() / anchor.height
                        };
                        starts[index] + offset.min(block.height().saturating_sub(1))
                    }
                    None => bottom,
                }
            }
        };
        // A fold opened or closed: the line clicked stays under the mouse.
        if let Some((key, row)) = self.pin.take()
            && let Some(index) = blocks.iter().position(|block| block.key == key)
        {
            top = (starts[index] + blocks[index].height()).saturating_sub(row + 1);
        }
        if self.pending_scroll != 0 {
            top = (top as i64 + self.pending_scroll).clamp(0, bottom as i64) as usize;
            self.pending_scroll = 0;
        }
        top = top.min(bottom);
        // The block at the top, and how far into it.
        let here = (!blocks.is_empty()).then(|| {
            let index = starts
                .partition_point(|start| *start <= top)
                .saturating_sub(1);
            Anchor {
                key: blocks[index].key,
                offset: top - starts[index],
                height: blocks[index].height(),
            }
        });
        match here {
            // At the bottom: following again (unless a drag holds it).
            _ if top >= bottom && !self.hold => self.anchor = None,
            Some(here) => {
                // Following stopped (or the view changed): what comes from
                // here on is new.
                if self
                    .baseline
                    .as_ref()
                    .is_none_or(|(view, _)| view != filter)
                {
                    self.baseline = Some((filter.clone(), text_lines));
                }
                self.anchor = Some(here);
            }
            None => self.anchor = None,
        }
        if self.anchor.is_none() {
            self.baseline = None;
        }

        // The selection's ends, in the order of the text.
        let range = self.selection.as_ref().and_then(|selection| {
            let place = |point: Point| {
                blocks
                    .iter()
                    .position(|block| block.key == point.key)
                    .map(|index| (index, point.line, point.col))
            };
            let (a, b) = (place(selection.anchor)?, place(selection.head)?);
            Some(if a <= b { (a, b) } else { (b, a) })
        });
        if self.selection.is_some() && range.is_none() {
            // Its text left the view (or the history).
            self.selection = None;
        }
        let highlight = highlight(theme);

        // The lines on the screen.
        buffer.set_style(area, theme.base());
        let mut rows: Vec<Option<RowRef>> = vec![None; height];
        let first = starts
            .partition_point(|start| *start <= top)
            .saturating_sub(1);
        let mut y = 0usize;
        let mut skip = top.saturating_sub(starts.get(first).copied().unwrap_or(0));
        let mut moving = false;
        let ms = motion::clock();
        for (index, block) in blocks.iter().enumerate().skip(first) {
            if y >= height {
                break;
            }
            // The user's messages sit on a band the width of the screen.
            let band = self.is_user(block).then(|| Style::new().bg(theme.user));
            for (line, wrapped) in self
                .block_rows(block, theme, width)
                .iter()
                .enumerate()
                .skip(skip)
            {
                if y >= height {
                    break;
                }
                let row = area.y + y as u16;
                if let Some(band) = band.filter(|_| !(block.gap && line == 0)) {
                    buffer.set_style(Rect::new(area.x, row, area.width, 1), band);
                }
                buffer.set_line(area.x, row, &Line::from(wrapped.spans.clone()), area.width);
                if let Some((from, to)) =
                    range.and_then(|range| selected_columns(range, (index, line), wrapped))
                {
                    for x in from..to.min(width) {
                        buffer[(area.x + x as u16, row)].set_style(highlight);
                    }
                }
                let gap = block.gap && line == 0;
                // The thinking that streams: its label, on its first line.
                if let Shown::Entry(entry) = block.shown
                    && self.motion.thinking == Some(self.entries[entry].item.id)
                    && line == usize::from(block.gap)
                {
                    motion::paint(buffer, (area.x, row), text_width(THINKING), theme, ms);
                    moving = true;
                }
                if block.shown == Shown::Working {
                    // (Not text: the mouse finds the line above it.)
                    if line == usize::from(block.gap) {
                        let label = text_width(&glyphs::render(WORKING));
                        let x = area.x + WORKING_SPINNER as u16;
                        motion::paint(buffer, (x, row), label, theme, ms);
                        moving = true;
                    }
                } else {
                    rows[y] = Some(RowRef {
                        key: block.key,
                        line,
                        gap,
                    });
                }
                y += 1;
            }
            skip = 0;
        }

        // The marker of what is new below: the lines that came since.
        let new = self
            .baseline
            .as_ref()
            .map_or(0, |(_, then)| text_lines.saturating_sub(*then));
        self.drawn = Drawn {
            area,
            rows,
            top: here,
            marker: None,
            counters: blocks
                .iter()
                .filter(|block| matches!(block.shown, Shown::Counter { .. }))
                .map(|block| block.key)
                .collect(),
            moving,
        };
        if self.anchor.is_some() && area.height > 0 && top < bottom {
            let text = if new > 0 {
                format!(
                    " {} {new} new line{} ",
                    glyphs::render("↓"),
                    if new == 1 { "" } else { "s" }
                )
            } else {
                format!(" {} end ", glyphs::render("↓"))
            };
            let w = text_width(&text) as u16;
            if w + 1 < area.width {
                let rect = Rect::new(area.right() - w - 1, area.bottom() - 1, w, 1);
                let style = if new > 0 { theme.chosen() } else { theme.key() };
                buffer.set_line(rect.x, rect.y, &Line::styled(text, style), w);
                self.drawn.marker = Some(rect);
            }
        }
    }

    /// A block's lines, with the blank line before it when it has one.
    /// Whether `block` is one of the user's messages.
    fn is_user(&self, block: &Block) -> bool {
        matches!(block.shown, Shown::Entry(index) if self.entries[index].item.kind == Kind::User)
    }

    fn block_rows(&self, block: &Block, theme: &Theme, width: usize) -> Vec<Wrapped> {
        let mut rows = Vec::with_capacity(block.height());
        if block.gap {
            rows.push(Wrapped::line(Vec::new()));
        }
        rows.extend(self.block_lines(block, theme, width));
        if block.tail {
            // (Not text: a copy leaves it out.)
            let mut blank = Wrapped::line(Vec::new());
            blank.bare = true;
            rows.push(blank);
        }
        rows
    }

    /// A block's lines (from the cache for an entry).
    fn block_lines(&self, block: &Block, theme: &Theme, width: usize) -> Vec<Wrapped> {
        let margin = |line: Line<'static>| {
            let mut line = Wrapped::line(line.spans);
            line.skip = MARGIN;
            line
        };
        match &block.shown {
            Shown::Banner => self
                .banner
                .as_ref()
                .map(|banner| banner_lines(banner, theme, width))
                .unwrap_or_default(),
            Shown::Entry(index) => self.entries[*index]
                .cache
                .as_ref()
                .map(|c| c.lines.clone())
                .unwrap_or_default(),
            Shown::Worker(id) => vec![margin(worker_line(self.worker(id), id, theme, width))],
            Shown::Heading(id) => self
                .worker(id)
                .map(|worker| heading_lines(worker, theme, width))
                .unwrap_or_default(),
            Shown::Counter {
                calls,
                failed,
                running,
            } => {
                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled(
                        format!(
                            "{} {calls} tool call{}",
                            glyphs::render("▸"),
                            if *calls == 1 { "" } else { "s" }
                        ),
                        theme.dim(),
                    ),
                ];
                if *failed > 0 {
                    spans.push(Span::styled(
                        rendered(format!(" · {failed} failed")),
                        theme.error(),
                    ));
                }
                if *running > 0 {
                    spans.push(Span::styled(rendered(" · running"), theme.dim()));
                }
                vec![margin(Line::from(spans))]
            }
            // (The spinner goes in the margin as it is drawn.)
            Shown::Working => {
                let label = glyphs::render(WORKING).into_owned();
                // The spinner stands in the column of the rail a command's
                // output hangs from, under it, and the word after it.
                let mut line = Wrapped::line(vec![
                    Span::raw(" ".repeat(WORKING_SPINNER + 2)),
                    Span::styled(label, theme.dim().add_modifier(Modifier::BOLD)),
                ]);
                line.bare = true;
                vec![line]
            }
        }
    }

    // ----- selecting text with the mouse -----

    /// The place in the text at the screen's `x`, `y`, as last drawn: above
    /// the transcript is its first line's start, below it (or past its
    /// text) its last line's end. `None` when nothing was drawn.
    pub fn point_at(&self, x: u16, y: u16) -> Option<Point> {
        let area = self.drawn.area;
        let drawn: Vec<(usize, RowRef)> = self
            .drawn
            .rows
            .iter()
            .enumerate()
            .filter_map(|(row, at)| at.map(|at| (row, at)))
            .collect();
        let (first, last) = (drawn.first()?, drawn.last()?);
        if y < area.y {
            return Some(Point {
                key: first.1.key,
                line: first.1.line,
                col: 0,
            });
        }
        let row = (y - area.y) as usize;
        let at = drawn.iter().rev().find(|(r, _)| *r <= row).unwrap_or(first);
        let past = y >= area.bottom() || row > last.0;
        Some(Point {
            key: at.1.key,
            line: at.1.line,
            col: if past {
                usize::MAX / 2
            } else {
                x.saturating_sub(area.x) as usize
            },
        })
    }

    /// Highlights from `anchor` to `head`, in `filter`'s view.
    pub fn select(&mut self, anchor: Point, head: Point, filter: &Filter) {
        self.selection = Some(Selection {
            anchor,
            head,
            filter: filter.clone(),
            width: self.drawn.area.width as usize,
        });
    }

    /// Holds the view still while a drag selects text: new output doesn't
    /// move the text under the mouse (it comes below, counted by the
    /// marker), as when the reader scrolls up.
    pub fn hold(&mut self) {
        if self.anchor.is_none() {
            self.anchor = self.drawn.top;
        }
        self.hold = true;
    }

    /// The drag ended: at the bottom, the view follows again.
    pub fn release(&mut self) {
        self.hold = false;
    }

    /// The block whose fold line is on the screen's `x`, `y`: its last
    /// line, which opens or closes a command's whole output or an agent's
    /// thinking.
    pub fn fold_at(&mut self, x: u16, y: u16, filter: &Filter, theme: &Theme) -> Option<u64> {
        let row = self.drawn.row_at(x, y)?;
        if !folds(self.item(row.key)?) {
            return None;
        }
        let rows = self.rows_of(row.key, filter, theme)?;
        (row.line + 1 == rows.len()).then_some(row.key)
    }

    /// Opens or closes the fold of the block `key`, its fold line held on
    /// the screen's row `y` at the next draw. Returns that line.
    pub fn toggle(&mut self, key: u64, y: u16, filter: &Filter, theme: &Theme) -> Option<usize> {
        let entry = self.find_mut(key)?;
        if !folds(&entry.item) {
            return None;
        }
        entry.open = !entry.open;
        entry.cache = None;
        self.pin = Some((key, y.saturating_sub(self.drawn.area.y) as usize));
        Some(self.rows_of(key, filter, theme)?.len() - 1)
    }

    /// The lines of the block `key` in `filter`'s view.
    fn rows_of(&mut self, key: u64, filter: &Filter, theme: &Theme) -> Option<Vec<Wrapped>> {
        let width = self.drawn.area.width as usize;
        let blocks = self.blocks(filter, theme, width);
        let block = blocks.iter().find(|block| block.key == key)?;
        Some(self.block_rows(block, theme, width))
    }

    /// The word at `point`: what a double click selects.
    pub fn word_at(
        &mut self,
        point: Point,
        filter: &Filter,
        theme: &Theme,
    ) -> Option<(Point, Point)> {
        let rows = self.rows_of(point.key, filter, theme)?;
        let (start, end) = word_columns(rows.get(point.line)?, point.col)?;
        Some((
            Point {
                col: start,
                ..point
            },
            Point { col: end, ..point },
        ))
    }

    /// The line at `point`, as the text has it (the display lines it was
    /// wrapped into): what a triple click selects.
    pub fn line_at(
        &mut self,
        point: Point,
        filter: &Filter,
        theme: &Theme,
    ) -> Option<(Point, Point)> {
        let rows = self.rows_of(point.key, filter, theme)?;
        let (first, last) = text_line(&rows, point.line)?;
        Some((
            Point {
                line: first,
                col: 0,
                ..point
            },
            Point {
                line: last,
                col: usize::MAX / 2,
                ..point
            },
        ))
    }

    /// The highlighted text, as written: without the view's margins,
    /// gutters and indents, and with the lines wrapped only for the screen
    /// joined again.
    pub fn selected_text(&mut self, filter: &Filter, theme: &Theme) -> Option<String> {
        let selection = self.selection.clone()?;
        let width = self.drawn.area.width as usize;
        let blocks = self.blocks(filter, theme, width);
        let place = |point: Point| {
            blocks
                .iter()
                .position(|block| block.key == point.key)
                .map(|index| (index, point.line, point.col))
        };
        let (a, b) = (place(selection.anchor)?, place(selection.head)?);
        let range = if a <= b { (a, b) } else { (b, a) };
        let rows: Vec<((usize, usize), Wrapped)> = (range.0.0..=range.1.0)
            .flat_map(|index| {
                self.block_rows(&blocks[index], theme, width)
                    .into_iter()
                    .enumerate()
                    .map(move |(line, wrapped)| ((index, line), wrapped))
            })
            .collect();
        copied(rows.iter().map(|(at, wrapped)| (*at, wrapped)), range)
    }
}

/// The columns of the word at column `col` of `line` (its first and last
/// characters'): what a double click selects.
pub(super) fn word_columns(line: &Wrapped, col: usize) -> Option<(usize, usize)> {
    let cells = cells(line);
    let at = cells.iter().position(|(_, start, w)| col < start + w)?;
    if at < line_skip(line, &cells) || cells[at].0.is_whitespace() {
        return None;
    }
    let word = |c: char| !c.is_whitespace();
    let mut start = at;
    while start > 0 && word(cells[start - 1].0) && cells[start - 1].1 >= line.skip {
        start -= 1;
    }
    let mut end = at;
    while end + 1 < cells.len() && word(cells[end + 1].0) {
        end += 1;
    }
    Some((cells[start].1, cells[end].1))
}

/// The first and last of `rows` that the line of text on row `row` was
/// wrapped into: what a triple click selects.
pub(super) fn text_line(rows: &[Wrapped], row: usize) -> Option<(usize, usize)> {
    rows.get(row)?;
    let mut first = row;
    while first > 0 && rows[first].join != Join::Break {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < rows.len() && rows[last + 1].join != Join::Break {
        last += 1;
    }
    Some((first, last))
}

/// What a copy of `range` takes of `lines` (each with its place, as
/// (block, line)), as written: without the margins, gutters and indents,
/// and with the lines wrapped only for the screen joined again.
pub(super) fn copied<'a>(
    lines: impl IntoIterator<Item = ((usize, usize), &'a Wrapped)>,
    range: Range,
) -> Option<String> {
    let mut text = String::new();
    let mut started = false;
    for (at, wrapped) in lines {
        let Some((from, to)) = selected_columns(range, at, wrapped) else {
            // A blank line inside the selection is still a line of it (a
            // border isn't).
            let inside = at > (range.0.0, range.0.1) && at < (range.1.0, range.1.1);
            if inside && started && !wrapped.bare {
                text.push('\n');
            }
            continue;
        };
        if started {
            text.push_str(match wrapped.join {
                Join::Break => "\n",
                Join::Space => " ",
                Join::Direct => "",
            });
        }
        started = true;
        // A line shown shortened, selected whole, is copied whole.
        match &wrapped.whole {
            Some(whole) if from <= wrapped.skip && to >= text_end(wrapped) => text.push_str(whole),
            _ => text.push_str(&columns(wrapped, from, to)),
        }
    }
    let text = text.trim_end().to_string();
    (!text.is_empty()).then_some(text)
}

/// The look of text highlighted with the mouse.
pub(super) fn highlight(theme: &Theme) -> Style {
    if theme.is_terminal() {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().bg(theme.selection)
    }
}

/// A line's characters, with the column each starts at and its width.
fn cells(line: &Wrapped) -> Vec<(char, usize, usize)> {
    let mut cells = Vec::new();
    let mut col = 0;
    for span in &line.spans {
        for c in span.content.chars() {
            let w = wrap::char_width(c);
            cells.push((c, col, w));
            col += w;
        }
    }
    cells
}

/// The index of a line's first cell of text (after its decoration).
fn line_skip(line: &Wrapped, cells: &[(char, usize, usize)]) -> usize {
    cells
        .iter()
        .position(|(_, col, _)| *col >= line.skip)
        .unwrap_or(cells.len())
}

/// What is highlighted, from its start to its end in the order of the
/// text, each as (block, line, column).
pub(super) type Range = ((usize, usize, usize), (usize, usize, usize));

/// The columns of `line` (the `at`'th line of a block, as (block, line))
/// that `range` covers, within its text: `None` when it covers none.
pub(super) fn selected_columns(
    range: Range,
    at: (usize, usize),
    line: &Wrapped,
) -> Option<(usize, usize)> {
    let ((start_block, start_line, start_col), (end_block, end_line, end_col)) = range;
    if line.bare || at < (start_block, start_line) || at > (end_block, end_line) {
        return None;
    }
    let end = text_end(line);
    let from = if at == (start_block, start_line) {
        start_col.max(line.skip)
    } else {
        line.skip
    };
    let to = if at == (end_block, end_line) {
        end_col.saturating_add(1).min(end)
    } else {
        end
    };
    (from < to).then_some((from, to))
}

/// The column after a line's text (its tail not counted).
fn text_end(line: &Wrapped) -> usize {
    cells(line)
        .iter()
        .rev()
        .find(|(c, _, _)| !c.is_whitespace())
        .map_or(0, |(_, col, w)| col + w)
        .saturating_sub(line.tail)
}

/// The text of `line` in columns `from` to `to`.
fn columns(line: &Wrapped, from: usize, to: usize) -> String {
    cells(line)
        .into_iter()
        .filter(|(_, col, _)| *col >= from && *col < to)
        .map(|(c, _, _)| c)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// `item` without what the terminal doesn't show (a call's parameters and
/// whole result, for apps), which would stay in memory with it.
fn for_terminal(mut item: Item) -> Item {
    item.parameters = None;
    item.output = None;
    item
}

/// Whether `item`'s block folds: a command's whole output, kept, or an
/// agent's thinking.
fn folds(item: &Item) -> bool {
    item.whole.is_some() || item.kind == Kind::Thinking
}

/// What a view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// The main transcript, in a view.
    Main(View),
    /// A worker's own transcript.
    Worker(String),
}

/// What a view does with an item.
enum Show {
    Hidden,
    Item,
    /// Counted in its turn's tool activity.
    Count,
    /// A worker's line.
    Worker(String),
}

impl Filter {
    fn show(&self, item: &Item) -> Show {
        let worker_line = || item.worker.clone().map_or(Show::Hidden, Show::Worker);
        match self {
            Filter::Worker(id) => match (&item.agent == id, item.kind) {
                // The lines of the workers it started.
                (true, Kind::WorkerLine) => worker_line(),
                (true, _) => Show::Item,
                (false, _) => Show::Hidden,
            },
            Filter::Main(view) => {
                if item.kind == Kind::WorkerLine {
                    // (Those of a worker's workers are in its own.)
                    return if item.is_main() {
                        worker_line()
                    } else {
                        Show::Hidden
                    };
                }
                if !item.is_main() {
                    // A worker's calls and what went wrong show in Full, and
                    // so does its stop (an Info notice). Its text, its report
                    // and what the user wrote to it are its own view's.
                    return match (view, item.kind) {
                        (View::Full, Kind::Call) => Show::Item,
                        (View::Full, Kind::Notice)
                            if item.level != Some(Level::Info) || item.stop =>
                        {
                            Show::Item
                        }
                        _ => Show::Hidden,
                    };
                }
                if item.kind == Kind::Turn {
                    // The status line shows how long the last turn ran, and
                    // the context and cost as they change. A line after each
                    // turn would repeat it.
                    return Show::Hidden;
                }
                match view {
                    View::Full | View::Quiet => Show::Item,
                    View::Messages => match item.kind {
                        Kind::User
                        | Kind::Text
                        | Kind::Message
                        | Kind::Question
                        | Kind::Reply
                        | Kind::Output
                        | Kind::Conversation => Show::Item,
                        Kind::Notice if item.level != Some(Level::Info) => Show::Item,
                        // Tool activity is one line a turn that counts the
                        // calls (as the web page shows it).
                        Kind::Call => Show::Count,
                        // (The agent's thinking is part of its work, not of
                        // what it says.)
                        Kind::Result | Kind::Notice | Kind::Turn | Kind::Thinking => Show::Hidden,
                        Kind::WorkerLine => worker_line(),
                    },
                }
            }
        }
    }
}

/// What kind of block an item makes, for the spacing: tool activity runs
/// together, and a blank line sets apart what is said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Activity,
    Prose,
    /// A command's output.
    Output,
}

fn group(item: &Item) -> Group {
    match item.kind {
        Kind::Call | Kind::Result | Kind::WorkerLine => Group::Activity,
        Kind::Notice | Kind::Thinking => Group::Activity,
        _ => Group::Prose,
    }
}

/// A number that changes when the theme does: the lines are drawn again.
fn theme_key(theme: &Theme) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    theme.name.hash(&mut hasher);
    format!("{:?}{:?}", theme.text, theme.dim).hash(&mut hasher);
    hasher.finish()
}

/// The left margin every line has: two columns, which the user's bar and
/// the tool lines' markers use.
const MARGIN: usize = 2;

/// The last line of a command's block that shows its whole output: a click
/// on it shows the first lines again.
const FOLD_CLOSE: &str = "↑ show the first lines only";

/// The label of an agent's thinking.
const THINKING: &str = "Thinking";

/// The line that says the agent works, while it waits on its model.
const WORKING: &str = "Working…";
/// The column of the "Working…" line's spinner: the column of the rail a
/// command's output hangs from (`gutter`), so the rail runs into it.
const WORKING_SPINNER: usize = MARGIN;

/// The last line of an agent's thinking shown whole: a click on it folds
/// it to its first line again.
const THINKING_CLOSE: &str = "↑ show the first line only";

/// An item's lines at `width` (margins included), with what a copy of each
/// needs. `own`: in its worker's own view, a worker's lines don't name it.
/// `under`: the worker that started the item's worker, which a notice names.
/// `open`: a command's block shows its whole output. `clock`: how long a
/// running call has run.
fn entry_lines(
    item: &Item,
    theme: &Theme,
    width: usize,
    own: bool,
    under: Option<&str>,
    open: bool,
    clock: Option<&str>,
) -> Vec<Wrapped> {
    let inner = width.saturating_sub(MARGIN).max(1);
    let blank = || Span::raw(" ".repeat(MARGIN));
    let mark = |symbol: &str, style: Style| {
        let symbol = glyphs::render(symbol).into_owned();
        let pad = MARGIN.saturating_sub(text_width(&symbol));
        Span::styled(format!("{symbol}{}", " ".repeat(pad)), style)
    };
    let worker_prefix = |item: &Item| -> Vec<Span<'static>> {
        if item.is_main() || own {
            Vec::new()
        } else {
            vec![Span::styled(
                format!("{} ", item.agent),
                theme.dim().add_modifier(Modifier::BOLD),
            )]
        }
    };
    match item.kind {
        Kind::User => {
            // A note the agent hasn't read yet is muted.
            let (style, bar) = if item.pending {
                (theme.dim(), theme.dim())
            } else {
                (theme.text(), theme.accent())
            };
            let mut lines = plain(&item.text, style, inner);
            if let Some(tag) = user_tag(item) {
                // (Not the user's text: a copy leaves it out.)
                let tag = Span::styled(format!("  {tag}"), theme.dim());
                let tag_width = text_width(&tag.content);
                match lines.last_mut() {
                    Some(last) if spans_width(&last.spans) + tag_width <= inner => {
                        last.spans.push(tag);
                        last.tail += tag_width;
                    }
                    _ => {
                        let mut line = Wrapped::line(vec![tag]);
                        line.skip = tag_width;
                        lines.push(line);
                    }
                }
            }
            wrap::prefixed(lines, mark("▎", bar), mark("▎", bar))
        }
        Kind::Text | Kind::Reply | Kind::Output => {
            // A command's warning or error keeps its colour.
            let style = match (item.kind, item.level) {
                (Kind::Output, Some(Level::Warn)) => theme.warning(),
                (Kind::Output, Some(Level::Error)) => theme.error(),
                _ => theme.text(),
            };
            let lines = if item.kind == Kind::Output {
                plain(&item.text, style, inner)
            } else {
                markdown::render(&item.text, style, theme, inner)
            };
            let lines = if lines.is_empty() {
                vec![Wrapped::line(Vec::new())]
            } else {
                lines
            };
            wrap::prefixed(lines, blank(), blank())
        }
        Kind::Message => {
            let mut lines = Vec::new();
            // A worker's message says whose it is.
            if let Some(from) = &item.from {
                let from = if from == MAIN_AGENT { "the lead" } else { from };
                lines.push(Wrapped::line(vec![Span::styled(
                    format!("Message from {from}"),
                    theme.dim().add_modifier(Modifier::BOLD),
                )]));
            }
            lines.extend(markdown::render(&item.text, theme.text(), theme, inner));
            wrap::prefixed(lines, mark("✉", theme.dim()), blank())
        }
        Kind::Question => {
            let mut lines = vec![Wrapped::line(vec![Span::styled(
                "The agent asks".to_string(),
                theme.accent().add_modifier(Modifier::BOLD),
            )])];
            match &item.questions {
                // Each question, with what became of it.
                Some(questions) => {
                    for question in questions {
                        let text = if crate::questions::is_agent_id(&question.id) {
                            format!("{} {}", question.id, question.question)
                        } else {
                            question.question.clone()
                        };
                        lines.extend(markdown::render(&text, theme.text(), theme, inner));
                        if let Some(state) = question_state(&question.state) {
                            lines.extend(
                                wrap::wrap(&[Span::styled(rendered(state), theme.dim())], inner, 0)
                                    .into_iter()
                                    .map(Wrapped::line),
                            );
                        }
                    }
                }
                None => lines.extend(markdown::render(&item.text, theme.text(), theme, inner)),
            }
            wrap::prefixed(lines, mark("?", theme.accent()), blank())
        }
        Kind::Call => {
            let mut spans = worker_prefix(item);
            let failed = item.status == Some(CallStatus::Failed);
            spans.push(Span::styled(rendered(&item.text), theme.dim()));
            match item.status {
                Some(CallStatus::Ok) => {
                    spans.push(Span::styled(rendered(" · "), theme.dim()));
                    spans.push(Span::styled(
                        glyphs::render("✓").into_owned(),
                        theme.success(),
                    ));
                    if let Some(outcome) = &item.outcome {
                        spans.push(Span::styled(rendered(format!(" {outcome}")), theme.dim()));
                    }
                }
                Some(CallStatus::Failed) => {
                    spans.push(Span::styled(rendered(" · "), theme.dim()));
                    let outcome = item.outcome.as_deref().unwrap_or("failed");
                    spans.push(Span::styled(
                        rendered(format!("✗ {outcome}")),
                        theme.error(),
                    ));
                }
                _ => {
                    if let Some(clock) = clock {
                        spans.push(Span::styled(rendered(format!(" · {clock}")), theme.dim()));
                    }
                    spans.push(Span::styled(
                        format!(" {}", glyphs::render("…")),
                        theme.dim(),
                    ));
                }
            }
            let marker_style = if failed { theme.error() } else { theme.dim() };
            wrap::prefixed(
                wrap::wrap_lines(&spans, inner, 2),
                mark("▸", marker_style),
                blank(),
            )
        }
        Kind::Result => {
            let style = match item.level {
                Some(Level::Error) => theme.error(),
                Some(Level::Warn) => theme.warning(),
                _ => theme.dim(),
            };
            let whole = item.whole.as_deref().filter(|_| open);
            let text = match (&item.detail, whole) {
                (_, Some(whole)) => whole.to_string(),
                (Some(detail), None) if item.level == Some(Level::Error) => {
                    format!("{}\n{detail}", item.text)
                }
                (Some(detail), None) => detail.clone(),
                (None, None) => item.text.clone(),
            };
            let mut lines = gutter(&text, style, theme, inner);
            // An open fold ends with the line that closes it, which a copy
            // leaves out (it isn't the output's).
            if whole.is_some() {
                lines.extend(
                    gutter(FOLD_CLOSE, style, theme, inner)
                        .into_iter()
                        .map(|line| Wrapped { bare: true, ..line }),
                );
            }
            lines
        }
        Kind::Notice => {
            let (symbol, style) = match item.level {
                Some(Level::Error) => ("✗", theme.error()),
                Some(Level::Warn) => ("!", theme.warning()),
                _ => ("·", theme.dim()),
            };
            let mut spans = worker_prefix(item);
            if let (Some(under), false) = (under, item.is_main() || own) {
                spans.push(Span::styled(
                    format!("(under {under}) "),
                    theme.dim().add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(rendered(&item.text), style));
            wrap::prefixed(
                wrap::wrap_lines(&spans, inner, 0),
                mark(symbol, style),
                blank(),
            )
        }
        Kind::Turn | Kind::Conversation => {
            let rule = glyphs::render("─").into_owned();
            let text = rendered(match &item.detail {
                Some(plan) => format!("{rule}{rule} {} · {plan} ", item.text),
                None => format!("{rule}{rule} {} ", item.text),
            });
            let fill = inner.saturating_sub(text_width(&text));
            let mut spans = vec![Span::styled(text, theme.dim())];
            spans.push(Span::styled(rule.repeat(fill), theme.dim()));
            wrap::prefixed(wrap::wrap_lines(&spans, inner, 0), blank(), blank())
        }
        Kind::Thinking => {
            // (Upright: in italic, some terminals draw an arrow over the
            // letter after it.)
            let style = theme.dim();
            let mut spans = worker_prefix(item);
            spans.push(Span::styled(
                THINKING.to_string(),
                theme.dim().add_modifier(Modifier::BOLD),
            ));
            // (The label isn't the agent's text: a copy leaves it out.)
            let label = spans_width(&spans);
            let text = wrap::clean(&item.text);
            let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
            if !open {
                // One line: the label and the first line, which a click
                // opens.
                let first = lines.next().unwrap_or_default().replace("**", "");
                let more = lines.next().is_some();
                let room = inner.saturating_sub(label + 3);
                let cut = text_width(&first) > room;
                let mut shown = cut_end(&first, room);
                if more && !cut && text_width(&shown) + 2 <= room {
                    shown.push_str(" …");
                }
                let separator = if shown.is_empty() {
                    String::new()
                } else {
                    rendered(" · ")
                };
                let separator_width = text_width(&separator);
                spans.push(Span::styled(separator, theme.dim()));
                spans.push(Span::styled(rendered(&shown), style));
                let mut line = Wrapped::line(spans);
                line.skip = label + separator_width;
                return wrap::prefixed(vec![line], blank(), blank());
            }
            let mut heading = Wrapped::line(spans);
            heading.bare = true;
            let mut out = vec![heading];
            out.extend(markdown::render(&item.text, style, theme, inner));
            out.extend(
                plain(THINKING_CLOSE, theme.dim(), inner)
                    .into_iter()
                    .map(|line| Wrapped { bare: true, ..line }),
            );
            wrap::prefixed(out, blank(), blank())
        }
        // Drawn from the worker's summary.
        Kind::WorkerLine => Vec::new(),
    }
}

/// The banner's labels' column.
const BANNER_LABELS: usize = 11;

/// The banner, boxed: the title, a row for each fact (its label and its
/// value), and the notes under a rule. A copy takes the values and notes,
/// not the box or the labels.
fn banner_lines(banner: &Banner, theme: &Theme, width: usize) -> Vec<Wrapped> {
    let set = theme.borders();
    let (tee_left, tee_right) = match glyphs::symbols() {
        glyphs::Symbols::Ascii => ("+".to_string(), "+".to_string()),
        _ => (
            glyphs::render("├").into_owned(),
            glyphs::render("┤").into_owned(),
        ),
    };
    let border = if theme.is_terminal() {
        theme.dim()
    } else {
        Style::new().fg(theme.border)
    };
    // As wide as the transcript, with the same margin on the right as on
    // the left.
    let outer = width.saturating_sub(2 * MARGIN).max(24);
    let inner = outer - 4;
    let edge = |left: &str, right: &str, title: Option<&str>| {
        let mut spans = vec![
            Span::raw(" ".repeat(MARGIN)),
            Span::styled(left.to_string(), border),
        ];
        let mut used = 2;
        if let Some(title) = title {
            spans.push(Span::styled(
                format!(" {title} "),
                theme.text().add_modifier(Modifier::BOLD),
            ));
            used += text_width(title) + 2;
        }
        spans.push(Span::styled(
            set.horizontal_top.repeat(outer.saturating_sub(used)),
            border,
        ));
        spans.push(Span::styled(right.to_string(), border));
        let mut line = Wrapped::line(spans);
        line.bare = true;
        line
    };
    // A line in the box: `lead` (a label, or nothing) and the text, with
    // the box's sides; the copy starts after the lead.
    let boxed = |lead: Span<'static>, text: Vec<Span<'static>>, join: Join| {
        let lead_width = text_width(&lead.content);
        let used = lead_width
            + text
                .iter()
                .map(|span| text_width(&span.content))
                .sum::<usize>();
        let pad = inner.saturating_sub(used);
        let mut spans = vec![
            Span::raw(" ".repeat(MARGIN)),
            Span::styled(format!("{} ", set.vertical_left), border),
            lead,
        ];
        spans.extend(text);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(format!(" {}", set.vertical_right), border));
        Wrapped {
            spans,
            join,
            skip: MARGIN + 2 + lead_width,
            tail: pad + 2,
            bare: false,
            whole: None,
        }
    };
    let value_width = inner.saturating_sub(BANNER_LABELS).max(1);
    // A row's value, shortened to fit: a copy of the row takes it whole.
    let row = |label: &str, value: &str, shown: String| {
        let mut line = boxed(
            Span::styled(format!("{label:<BANNER_LABELS$}"), theme.dim()),
            vec![Span::styled(shown.clone(), theme.text())],
            Join::Break,
        );
        if shown != value {
            line.whole = Some(value.to_string());
        }
        line
    };
    // A blank line above the box: one row reads as wide as the two
    // columns of margin at its sides.
    let mut above = Wrapped::line(Vec::new());
    above.bare = true;
    let mut lines = vec![above, edge(set.top_left, set.top_right, Some("Arko"))];
    lines.push(row(
        "Model",
        &banner.model,
        cut_end(&banner.model, value_width),
    ));
    lines.push(row(
        "Workers",
        &banner.workers,
        cut_end(&banner.workers, value_width),
    ));
    lines.push(row(
        "Workspace",
        &banner.workspace,
        cut_middle(&banner.workspace, value_width),
    ));
    lines.push(row(
        "Session",
        &banner.session,
        cut_end(&banner.session, value_width),
    ));
    if let Some(project) = &banner.project {
        lines.push(row("Project", project, cut_end(project, value_width)));
    }
    if !banner.tools.is_empty() {
        let tools = rendered(banner.tools.join(" · "));
        let spans = [Span::styled(tools, theme.text())];
        // Tools may take more than a line: under their value.
        for (n, line) in wrap::wrap_lines(&spans, value_width, 0)
            .into_iter()
            .enumerate()
        {
            let label = if n == 0 { "Tools" } else { "" };
            lines.push(boxed(
                Span::styled(format!("{label:<BANNER_LABELS$}"), theme.dim()),
                line.spans,
                line.join,
            ));
        }
    }
    if banner.model_note.is_some() || !banner.notes.is_empty() {
        lines.push(edge(&tee_left, &tee_right, None));
        let model_note = banner.model_note.iter().map(|note| (note.clone(), None));
        for (note, level) in model_note.chain(banner.notes.iter().cloned()) {
            let style = match level {
                Some(Level::Warn) => theme.warning(),
                Some(Level::Error) => theme.error(),
                _ => theme.dim(),
            };
            let spans = [Span::styled(note, style)];
            for line in wrap::wrap_lines(&spans, inner, 0) {
                lines.push(boxed(Span::raw(""), line.spans, line.join));
            }
        }
    }
    lines.push(edge(set.bottom_left, set.bottom_right, None));
    lines
}

/// `text` in `width` columns, cut at its end with an ellipsis.
pub(super) fn cut_end(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_string();
    }
    let mut cut = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = wrap::char_width(c);
        if used + w + 1 > width {
            break;
        }
        cut.push(c);
        used += w;
    }
    cut + &glyphs::render("…")
}

/// `text` in `width` columns, cut in its middle with an ellipsis: a path
/// keeps its start and its end (the folder that matters).
fn cut_middle(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let room = width.saturating_sub(1);
    let head = room / 2;
    let tail = room - head;
    let start: String = chars.iter().take(head).collect();
    let end: String = chars[chars.len().saturating_sub(tail)..].iter().collect();
    format!("{start}{}{end}", glyphs::render("…"))
}

/// What the user's text became, when it isn't the usual (a new turn). A
/// note the agent hasn't read is "pending", and once it has, it is a message
/// like any other. A command reads as one by itself.
fn user_tag(item: &Item) -> Option<String> {
    let delivery = match item.delivery {
        Some(Delivery::Note) if item.pending => Some("pending"),
        Some(Delivery::Answer) => Some("answer"),
        Some(Delivery::Queued) => Some("queued"),
        _ => None,
    };
    let web = (item.source == Some(Source::Web)).then_some("from the web page");
    match (delivery, web) {
        (Some(d), Some(w)) => Some(rendered(format!("{d} · {w}"))),
        (d, w) => d.or(w).map(str::to_string),
    }
}

/// `text` (Arko's words around the agent's) with its symbols as the
/// terminal shows them.
fn rendered(text: impl AsRef<str>) -> String {
    glyphs::render(text.as_ref()).into_owned()
}

/// `text` as it is, wrapped.
fn plain(text: &str, style: Style, width: usize) -> Vec<Wrapped> {
    let text = wrap::clean(&glyphs::render(text));
    let mut lines = Vec::new();
    for line in text.split('\n') {
        lines.extend(wrap::wrap_lines(
            &[Span::styled(line.to_string(), style)],
            width,
            0,
        ));
    }
    lines
}

/// `text` beside a gutter, under its call: a command's output, an error.
/// (The gutter isn't copied.)
fn gutter(text: &str, style: Style, theme: &Theme, width: usize) -> Vec<Wrapped> {
    let lines = plain(text, style, width.saturating_sub(2).max(1));
    let side = Span::styled(format!("  {}", glyphs::render("│ ")), theme.dim());
    lines
        .into_iter()
        .map(|line| line.decorated(side.clone()))
        .collect()
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| text_width(&span.content)).sum()
}

/// How long `ms` milliseconds read: `45s`, `2m`, `1h 5m`.
pub fn duration(ms: i64) -> String {
    let seconds = (ms / 1000).max(0);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// How long a running call has run, once that is a second or more: a
/// quick call doesn't flash a time.
fn running_time(item: &Item, now: i64) -> Option<String> {
    let ms = now - item.at;
    (item.status == Some(CallStatus::Running) && ms >= 1000).then(|| duration(ms))
}

/// A worker's state in a word, and its style.
pub fn state(worker: &WorkerSummary, theme: &Theme) -> (&'static str, Style) {
    match worker.state {
        WorkerState::Queued => ("queued", theme.dim()),
        WorkerState::Running => ("running", theme.accent()),
        WorkerState::Finished => ("finished", theme.success()),
        WorkerState::Failed => ("failed", theme.error()),
        WorkerState::Stopped => ("stopped", theme.warning()),
    }
}

/// How long the worker has run in all its runs, also those before the
/// session was opened again. `None` until it has run.
pub fn worker_time(worker: &WorkerSummary) -> Option<String> {
    run_time(worker, chrono::Utc::now().timestamp_millis()).map(duration)
}

/// The worker's run time at `now` in milliseconds (its runs that ended and
/// the current one so far), once it has run.
pub fn run_time(worker: &WorkerSummary, now: i64) -> Option<i64> {
    (worker.ran_ms > 0 || worker.running_since.is_some())
        .then(|| i64::try_from(worker.run_time_ms(now)).unwrap_or(i64::MAX))
}

/// A worker's line: `r2 Fix flaky test · 18 tool calls · 2m · running`.
fn worker_line(
    worker: Option<&WorkerSummary>,
    id: &str,
    theme: &Theme,
    width: usize,
) -> Line<'static> {
    let mut spans = vec![
        Span::raw(" ".repeat(MARGIN)),
        Span::styled(id.to_string(), theme.text().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
    ];
    if let Some(worker) = worker {
        let mut facts = vec![worker.title.clone()];
        facts.push(format!(
            "{} tool call{}",
            worker.tool_calls,
            if worker.tool_calls == 1 { "" } else { "s" }
        ));
        facts.extend(worker_time(worker));
        spans.push(Span::styled(
            rendered(format!("{} · ", facts.join(" · "))),
            theme.dim(),
        ));
        let (word, style) = state(worker, theme);
        spans.push(Span::styled(word.to_string(), style));
        if worker.state == WorkerState::Running
            && let Some(last) = &worker.last
        {
            spans.push(Span::styled(rendered(format!(" · {last}")), theme.dim()));
        }
    }
    // One line, cut at the width.
    let mut line = Vec::new();
    let mut used = 0;
    for span in spans {
        let w = text_width(&span.content);
        if used + w <= width {
            used += w;
            line.push(span);
        } else {
            let room = width.saturating_sub(used);
            let mut cut = String::new();
            let mut cut_width = 0;
            for c in span.content.chars() {
                let cw = wrap::char_width(c);
                if cut_width + cw + 1 > room {
                    break;
                }
                cut.push(c);
                cut_width += cw;
            }
            if room > 0 {
                cut.push_str(&glyphs::render("…"));
                line.push(Span::styled(cut, span.style));
            }
            break;
        }
    }
    Line::from(line)
}

/// A worker view's heading: the worker's ID, task and state.
fn heading_lines(worker: &WorkerSummary, theme: &Theme, width: usize) -> Vec<Wrapped> {
    let inner = width.saturating_sub(MARGIN).max(1);
    let (word, style) = state(worker, theme);
    let mut spans = vec![
        Span::styled(
            format!("{} ", worker.id),
            theme.text().add_modifier(Modifier::BOLD),
        ),
        Span::styled(worker.title.clone(), theme.text()),
        Span::styled(rendered(" · "), theme.dim()),
        Span::styled(word.to_string(), style),
    ];
    if let Some(time) = worker_time(worker) {
        spans.push(Span::styled(rendered(format!(" · {time}")), theme.dim()));
    }
    wrap::prefixed(
        wrap::wrap_lines(&spans, inner, 0),
        Span::raw("  "),
        Span::raw("  "),
    )
}

/// What became of a question, as its card says it under the question
/// (`None` while it is open).
fn question_state(state: &crate::questions::QuestionState) -> Option<String> {
    use crate::questions::QuestionState;
    Some(match state {
        QuestionState::Open => return None,
        QuestionState::Answered { answer } => format!("answered · {answer}"),
        QuestionState::AnsweredInMessage => "answered in your message".to_string(),
        QuestionState::Closed { why: Some(why) } => format!("closed by the agent · {why}"),
        QuestionState::Closed { why: None } => "closed by the agent".to_string(),
        QuestionState::Dismissed => "dismissed".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u64, kind: Kind, text: &str) -> Item {
        let mut item = Item::new(kind, text);
        item.id = id;
        item.at = chrono::Utc::now().timestamp_millis();
        item
    }

    fn rows(transcript: &mut Transcript, filter: &Filter, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        transcript.draw(&mut buffer, area, filter, &Theme::terminal());
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn full() -> Filter {
        Filter::Main(View::Full)
    }

    /// A question card shows each question with what became of it, and
    /// changes as that does.
    #[test]
    fn a_question_card_shows_each_questions_state() {
        use crate::questions::QuestionState;
        let mut t = Transcript::new();
        let mut card = item(1, Kind::Question, "q1 Tabs or spaces?\nq2 Push now?");
        let mut questions = crate::tui::app::tests::asking().questions;
        questions[1].question = "Push now?".into();
        card.questions = Some(questions.clone());
        t.apply(Update::Item(card.clone()));
        assert_eq!(
            rows(&mut t, &full(), 40, 3),
            ["? The agent asks", "  q1 Tabs or spaces?", "  q2 Push now?"]
        );
        questions[0].state = QuestionState::Answered {
            answer: "Spaces".into(),
        };
        questions[1].state = QuestionState::Closed {
            why: Some("Found it".into()),
        };
        card.questions = Some(questions);
        t.apply(Update::Change(card));
        assert_eq!(
            rows(&mut t, &full(), 40, 5),
            [
                "? The agent asks",
                "  q1 Tabs or spaces?",
                "  answered · Spaces",
                "  q2 Push now?",
                "  closed by the agent · Found it"
            ]
        );
    }

    /// A message a worker got shows in its own view with whose it is, and
    /// not in the main transcript, where the lead's call says it.
    #[test]
    fn a_workers_message_shows_in_its_own_view() {
        let mut t = Transcript::new();
        let mut call = item(1, Kind::Call, "message r1: Also the docs.");
        call.status = Some(CallStatus::Ok);
        call.outcome = Some("sent".into());
        t.apply(Update::Item(call));
        let mut message = item(2, Kind::Message, "Also the docs.");
        message.agent = "r1".into();
        message.from = Some(MAIN_AGENT.into());
        t.apply(Update::Item(message));
        assert_eq!(
            rows(&mut t, &full(), 40, 2),
            ["▸ message r1: Also the docs. · ✓ sent", ""]
        );
        assert_eq!(
            rows(&mut t, &Filter::Worker("r1".into()), 40, 2),
            ["✉ Message from the lead", "  Also the docs."]
        );
    }

    /// What the user wrote to a worker shows in that worker's own view, as
    /// the user's message. No view of the main transcript has it, and no
    /// other worker's view.
    #[test]
    fn the_users_message_to_a_worker_shows_in_its_own_view_only() {
        let mut t = Transcript::new();
        t.apply(Update::Item(item(1, Kind::User, "Fix it")));
        let mut message = item(2, Kind::User, "Use tabs.").agent("r1");
        message.source = Some(Source::Web);
        t.apply(Update::Item(message));
        t.apply(Update::Item(item(3, Kind::Text, "Testing.").agent("r2")));
        for view in View::ALL {
            assert_eq!(
                rows(&mut t, &Filter::Main(view), 50, 2),
                ["▎ Fix it", ""],
                "{view:?}"
            );
        }
        assert_eq!(
            rows(&mut t, &Filter::Worker("r1".into()), 50, 2),
            ["▎ Use tabs.  from the web page", ""]
        );
        assert_eq!(
            rows(&mut t, &Filter::Worker("r2".into()), 50, 2),
            ["  Testing.", ""]
        );
    }

    /// A running call shows how long it has run once that is a second,
    /// and the time goes on with nothing new from the feed; its outcome
    /// takes its place.
    #[test]
    fn a_running_call_shows_its_time() {
        let mut t = Transcript::new();
        let mut call = item(1, Kind::Call, "$ sleep 30");
        call.status = Some(CallStatus::Running);
        call.agent = "r1".into();
        call.at -= 900;
        t.apply(Update::Item(call.clone()));
        let own = Filter::Worker("r1".into());
        assert_eq!(rows(&mut t, &own, 40, 1)[0], "▸ $ sleep 30 …");
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert_eq!(rows(&mut t, &own, 40, 1)[0], "▸ $ sleep 30 · 1s …");
        call.at -= 2 * 60_000;
        t.apply(Update::Change(call.clone()));
        assert_eq!(rows(&mut t, &full(), 40, 1)[0], "▸ r1 $ sleep 30 · 2m …");
        call.status = Some(CallStatus::Ok);
        call.outcome = Some("30.0 s".into());
        t.apply(Update::Change(call));
        assert_eq!(rows(&mut t, &own, 40, 1)[0], "▸ $ sleep 30 · ✓ 30.0 s");
    }

    /// A call's outcome is on its line; a command's output is a block
    /// under it; the user's message has the bar; the end of a turn draws
    /// nothing (the status line says how long it ran).
    #[test]
    fn items_draw_as_the_design_says() {
        let mut t = Transcript::new();
        let mut user = item(1, Kind::User, "Run the tests");
        user.delivery = Some(Delivery::Turn);
        t.apply(Update::Item(user));
        let mut call = item(2, Kind::Call, "$ cargo test");
        call.status = Some(CallStatus::Running);
        t.apply(Update::Item(call.clone()));
        assert_eq!(rows(&mut t, &full(), 40, 3)[2], "▸ $ cargo test …");
        call.status = Some(CallStatus::Ok);
        call.outcome = Some("12 passed".into());
        t.apply(Update::Change(call));
        let mut result = item(3, Kind::Result, "exit code 0, 1.0 s").detail("running 12 tests");
        result.call = Some(2);
        t.apply(Update::Item(result));
        t.apply(Update::Item(item(4, Kind::Reply, "All **12** pass.")));
        t.apply(Update::Item(item(5, Kind::Turn, "3 s · $0.010")));
        assert_eq!(
            rows(&mut t, &full(), 40, 8),
            [
                "▎ Run the tests",
                "",
                "▸ $ cargo test · ✓ 12 passed",
                "  │ running 12 tests",
                "",
                "  All 12 pass.",
                "",
                "",
            ]
        );
    }

    /// Where the conversation changes, a rule names it, in every view. The
    /// main agent's items cut from the conversation go, a worker's stay.
    #[test]
    fn a_conversation_is_a_rule_and_a_cut_drops_its_items() {
        let mut t = Transcript::new();
        t.apply(Update::Item(item(1, Kind::User, "Count the pears")));
        t.apply(Update::Item(item(
            2,
            Kind::Conversation,
            "New conversation",
        )));
        t.apply(Update::Item(item(3, Kind::User, "Now the apples")));
        t.apply(Update::Item(item(4, Kind::Text, "Five.").agent("r1")));
        let shown = rows(&mut t, &Filter::Main(View::Messages), 30, 8);
        let at = |text: &str| shown.iter().position(|row| row.starts_with(text));
        let (first, rule, next) = (
            at("▎ Count the pears"),
            at("  ── New conversation ──"),
            at("▎ Now the apples"),
        );
        assert!(first < rule && rule < next && first.is_some(), "{shown:?}");
        t.apply(Update::Truncate { from: 2 });
        let ids: Vec<u64> = t.entries.iter().map(|entry| entry.item.id).collect();
        assert_eq!(ids, [1, 4], "the worker's item stays");
    }

    /// An agent's thinking is one quiet line, its label and its first
    /// line, in Full and Quiet and in its worker's own view, and none in
    /// Messages. Its line opens it whole, ending with the line that folds it
    /// again. With `--show-thinking` it starts whole.
    #[test]
    fn thinking_is_one_line_that_opens_whole() {
        let theme = Theme::terminal();
        let mut t = Transcript::new();
        t.apply(Update::Item(item(
            1,
            Kind::Thinking,
            "**Planning**\nRead the tests first.",
        )));
        t.apply(Update::Item(item(2, Kind::Text, "Running them.")));
        t.apply(Update::Item(
            item(3, Kind::Thinking, "Where is the parser?").agent("r1"),
        ));
        let folded = ["  Thinking · Planning …", "", "  Running them."];
        assert_eq!(rows(&mut t, &full(), 40, 3), folded);
        assert_eq!(rows(&mut t, &Filter::Main(View::Quiet), 40, 3), folded);
        assert_eq!(
            rows(&mut t, &Filter::Main(View::Messages), 40, 1),
            ["  Running them."]
        );
        assert_eq!(
            rows(&mut t, &Filter::Worker("r1".into()), 40, 1),
            ["  Thinking · Where is the parser?"]
        );
        // Its line opens it, and the last line folds it again.
        rows(&mut t, &full(), 40, 8);
        assert_eq!(t.fold_at(4, 0, &full(), &theme), Some(1));
        t.toggle(1, 0, &full(), &theme);
        t.follow();
        let open = rows(&mut t, &full(), 40, 7);
        assert_eq!(
            open[..5],
            [
                "  Thinking",
                "  Planning",
                "  Read the tests first.",
                "  ↑ show the first line only",
                "",
            ]
        );
        assert_eq!(t.fold_at(4, 3, &full(), &theme), Some(1));
        assert_eq!(t.fold_at(4, 1, &full(), &theme), None, "not its text");
        t.toggle(1, 3, &full(), &theme);
        t.follow();
        assert_eq!(rows(&mut t, &full(), 40, 3), folded);

        let mut shown = Transcript::new();
        shown.set_open_thinking(true);
        shown.apply(Update::Item(item(1, Kind::Thinking, "All of it.")));
        assert_eq!(
            rows(&mut shown, &full(), 40, 3),
            ["  Thinking", "  All of it.", "  ↑ show the first line only"]
        );
    }

    /// What follows the spinner at the start of `row`, if it has one.
    fn after_spinner(row: &str) -> Option<&str> {
        let (frames, _) = glyphs::spinner_frames(glyphs::symbols());
        let first = row.chars().next()?;
        let spins = frames.contains(&first.to_string().as_str());
        spins.then(|| &row[first.len_utf8()..])
    }

    /// Whether `row` is the "Working…" line: its spinner in the column of
    /// the rail a command's output hangs from, then the word.
    fn is_working(row: &str) -> bool {
        row.strip_prefix(" ".repeat(WORKING_SPINNER).as_str())
            .and_then(after_spinner)
            == Some(" Working…")
    }

    /// While the main agent waits on its model with nothing on the screen
    /// to show it (a turn began, a call's result went back, its text went
    /// quiet), "Working…" stands at the end. Its thinking, its text as it
    /// streams and its calls running show it instead, and it goes once
    /// the agent replied or stopped working.
    #[test]
    fn working_shows_while_the_agent_waits_on_its_model() {
        let mut t = Transcript::new();
        let last = |t: &mut Transcript, view: View| {
            let shown = rows(t, &Filter::Main(view), 40, 12);
            shown
                .into_iter()
                .rfind(|row| !row.is_empty())
                .unwrap_or_default()
        };
        let mut user = item(1, Kind::User, "Fix the tests");
        user.delivery = Some(Delivery::Turn);
        t.apply(Update::Item(user));
        assert_eq!(last(&mut t, View::Full), "▎ Fix the tests", "idle");
        t.working = Some(0);
        // A blank line on each side of it, after what was said.
        let shown = rows(&mut t, &full(), 40, 4);
        assert_eq!(shown[..2], ["▎ Fix the tests", ""]);
        assert!(is_working(&shown[2]), "{shown:?}");
        assert_eq!(shown[3], "");

        // Its thinking streams: the thinking shows it, its label moving, but
        // not in Messages.
        t.apply(Update::Item(item(2, Kind::Thinking, "Where to start?")));
        let thinking = last(&mut t, View::Full);
        assert_eq!(
            after_spinner(&thinking),
            Some(" Thinking · Where to start?"),
            "{thinking:?}"
        );
        assert!(t.drawn.moving);
        assert!(is_working(&last(&mut t, View::Messages)));
        // A call runs, then ends: the model has its result.
        let mut call = item(3, Kind::Call, "$ cargo test");
        call.status = Some(CallStatus::Running);
        t.apply(Update::Item(call.clone()));
        assert_eq!(last(&mut t, View::Full), "▸ $ cargo test …");
        call.status = Some(CallStatus::Ok);
        t.apply(Update::Change(call));
        let shown = rows(&mut t, &full(), 40, 5);
        assert_eq!(
            shown[2], "  Thinking · Where to start?",
            "it no longer moves"
        );
        assert_eq!(shown[3], "▸ $ cargo test · ✓", "no blank line before it");
        assert!(is_working(&shown[4]), "{shown:?}");
        assert!(is_working(&last(&mut t, View::Quiet)));
        assert!(is_working(&last(&mut t, View::Messages)));
        // Under the output's rail, its spinner in the rail's column.
        t.apply(Update::Item(item(4, Kind::Result, "12 passed")));
        let shown = rows(&mut t, &full(), 40, 6);
        let rail = shown
            .iter()
            .find_map(|row| row.chars().position(|c| c == '│'));
        assert_eq!(rail, Some(WORKING_SPINNER), "{shown:?}");
        assert!(shown.last().is_some_and(|row| is_working(row)), "{shown:?}");

        // Two calls side by side: while one runs, it shows the agent works.
        let mut calls = [
            item(4, Kind::Call, "read a.rs"),
            item(5, Kind::Call, "read b.rs"),
        ];
        calls[0].status = Some(CallStatus::Running);
        calls[1].status = Some(CallStatus::Ok);
        for call in &calls {
            t.apply(Update::Item(call.clone()));
        }
        assert_eq!(last(&mut t, View::Full), "▸ read b.rs · ✓");
        calls[0].status = Some(CallStatus::Ok);
        t.apply(Update::Change(calls[0].clone()));
        assert!(is_working(&last(&mut t, View::Full)));

        // Its text streams, then goes quiet (it writes a long call).
        t.apply(Update::Item(item(6, Kind::Text, "Fixing the parser.")));
        assert_eq!(last(&mut t, View::Full), "  Fixing the parser.");
        let entry = t.entries.back_mut().unwrap();
        entry.changed -= QUIET;
        assert!(is_working(&last(&mut t, View::Full)));
        t.apply(Update::Append {
            id: 6,
            text: " Then the tests.".into(),
        });
        assert_eq!(
            last(&mut t, View::Full),
            "  Fixing the parser. Then the tests."
        );

        // Its reply ends the turn.
        t.apply(Update::Item(item(7, Kind::Reply, "All pass.")));
        assert_eq!(last(&mut t, View::Full), "  All pass.");
        let turn = item(8, Kind::Turn, "3 s");
        let ended = turn.at;
        t.apply(Update::Item(turn));
        assert_eq!(last(&mut t, View::Full), "  All pass.");
        t.working = None;
        assert_eq!(last(&mut t, View::Full), "  All pass.");
        // A turn that begins without a message (the workers' news came).
        t.apply(Update::Item(item(
            9,
            Kind::Notice,
            "the workers' news goes to the agent",
        )));
        t.working = Some(ended + 1);
        assert!(is_working(&last(&mut t, View::Full)));
    }

    /// A worker's thinking that streams moves in its own view, and open
    /// whole (`--show-thinking`) its heading moves. Thinking a later item
    /// followed is still, as is the thinking of a worker that ended.
    #[test]
    fn the_thinking_that_streams_moves() {
        use crate::tui::app::tests::worker;
        let mut t = Transcript::new();
        t.set_open_thinking(true);
        t.apply(Update::Worker(worker("r1", WorkerState::Running)));
        t.apply(Update::Item(item(1, Kind::Thinking, "Plan.").agent("r1")));
        let own = Filter::Worker("r1".into());
        let shown = rows(&mut t, &own, 40, 5);
        assert_eq!(after_spinner(&shown[2]), Some(" Thinking"), "{shown:?}");
        assert_eq!(shown[3..], ["  Plan.", "  ↑ show the first line only"]);
        assert!(t.drawn.moving);
        t.apply(Update::Item(
            item(2, Kind::Text, "Done planning.").agent("r1"),
        ));
        let shown = rows(&mut t, &own, 40, 7);
        assert_eq!(shown[2], "  Thinking", "{shown:?}");
        assert!(!t.drawn.moving);
        t.apply(Update::Item(item(3, Kind::Thinking, "More.").agent("r1")));
        rows(&mut t, &own, 40, 12);
        assert!(t.drawn.moving);
        t.apply(Update::Worker(worker("r1", WorkerState::Finished)));
        let shown = rows(&mut t, &own, 40, 12);
        assert!(!t.drawn.moving, "{shown:?}");
    }

    /// Text the agent writes beside a `wait_for_agents` call, and a message
    /// it shows with `message_user`, are drawn in the order they came in
    /// every view: not folded into the waiting row, and not dropped in the
    /// quiet view. The messages view counts the call.
    #[test]
    fn text_beside_a_wait_and_a_message_show_in_every_view() {
        let mut t = Transcript::new();
        t.apply(Update::Item(item(1, Kind::User, "Check the files.")));
        t.apply(Update::Item(item(2, Kind::Text, "Looking at it.")));
        t.apply(Update::Item(item(
            3,
            Kind::Message,
            "Halfway: two files to go.",
        )));
        t.apply(Update::Item(item(4, Kind::Text, "Waiting for the check.")));
        let mut waiting = item(5, Kind::Call, "waiting for r1");
        waiting.status = Some(CallStatus::Running);
        t.apply(Update::Item(waiting));
        for (filter, last) in [
            (full(), "waiting for r1"),
            (Filter::Main(View::Quiet), "waiting for r1"),
            (Filter::Main(View::Messages), "1 tool call"),
        ] {
            let shown = rows(&mut t, &filter, 50, 12);
            let at = |text: &str| {
                shown
                    .iter()
                    .position(|row| row.contains(text))
                    .unwrap_or_else(|| panic!("{text:?} not drawn: {shown:?}"))
            };
            let order = [
                "Check the files.",
                "Looking at it.",
                "Halfway: two files to go.",
                "Waiting for the check.",
                last,
            ];
            for pair in order.windows(2) {
                assert!(at(pair[0]) < at(pair[1]), "{pair:?} in {shown:?}");
            }
            // The text is its own row, not part of the waiting row.
            assert!(
                !shown[at(last)].contains("Waiting for the check."),
                "{shown:?}"
            );
        }
    }

    /// A user's words carry a tag only for what isn't the usual. A note the
    /// agent hasn't read is "pending", and once it has, it is a message like
    /// any other. An answer, a line waiting for its turn and a message from
    /// the web page say so, and a command or a refresh has none.
    #[test]
    fn a_user_message_is_tagged_only_for_what_is_unusual() {
        let tag = |delivery: Option<Delivery>, pending: bool, source: Option<Source>| {
            let mut user = item(1, Kind::User, "Hi.");
            user.delivery = delivery;
            user.pending = pending;
            user.source = source;
            user_tag(&user)
        };
        let said = |tag: Option<String>| tag.unwrap_or_default();
        assert_eq!(said(tag(None, false, None)), "");
        assert_eq!(said(tag(Some(Delivery::Turn), false, None)), "");
        assert_eq!(said(tag(Some(Delivery::Note), true, None)), "pending");
        assert_eq!(said(tag(Some(Delivery::Note), false, None)), "");
        assert_eq!(said(tag(Some(Delivery::Answer), false, None)), "answer");
        assert_eq!(said(tag(Some(Delivery::Queued), false, None)), "queued");
        assert_eq!(said(tag(Some(Delivery::Command), false, None)), "");
        assert_eq!(said(tag(Some(Delivery::Refresh), false, None)), "");
        let web = Some(Source::Web);
        assert_eq!(
            said(tag(Some(Delivery::Turn), false, web)),
            "from the web page"
        );
        assert_eq!(
            said(tag(Some(Delivery::Note), true, web)),
            "pending · from the web page"
        );
        assert_eq!(
            said(tag(Some(Delivery::Note), false, web)),
            "from the web page"
        );
        // Without the symbols' middle dot.
        let _ascii = crate::glyphs::tests::choose_symbols(glyphs::Symbols::Ascii);
        assert_eq!(
            said(tag(Some(Delivery::Queued), false, web)),
            "queued | from the web page"
        );
    }

    /// "Working…" stands where the agent's output comes next: above a
    /// note the agent hasn't read, which waits at the end.
    #[test]
    fn working_stands_above_a_note_waiting_to_be_read() {
        let mut t = Transcript::new();
        t.working = Some(0);
        let mut call = item(1, Kind::Call, "$ make");
        call.status = Some(CallStatus::Ok);
        t.apply(Update::Item(call));
        let mut note = item(2, Kind::User, "Use tabs.");
        note.delivery = Some(Delivery::Note);
        note.pending = true;
        t.apply(Update::Item(note));
        // The note's blank line sets it apart below, so it has one above too.
        let shown = rows(&mut t, &full(), 40, 5);
        assert_eq!(shown[..2], ["▸ $ make · ✓", ""]);
        assert!(is_working(&shown[2]), "{shown:?}");
        assert_eq!(shown[3..], ["", "▎ Use tabs.  pending"]);
    }

    /// In a worker's own view, "Working…" says the worker waits on its
    /// model, from its start. A copy leaves it out, and the marker of
    /// what is new doesn't count it.
    #[test]
    fn working_shows_in_a_workers_view_and_is_not_text() {
        let mut t = Transcript::new();
        let mut worker = WorkerSummary {
            title: "Test it".into(),
            state: WorkerState::Running,
            ..WorkerSummary::new("r1")
        };
        t.apply(Update::Worker(worker.clone()));
        let own = Filter::Worker("r1".into());
        let shown = rows(&mut t, &own, 40, 4);
        assert!(shown[0].starts_with("  r1 Test it"), "{shown:?}");
        assert_eq!(shown[1], "");
        assert!(is_working(&shown[2]), "{shown:?}");
        assert_eq!(shown[3], "");
        // The main agent doesn't work: its view has no such line.
        assert!(
            !rows(&mut t, &full(), 40, 3)
                .iter()
                .any(|row| is_working(row))
        );
        t.apply(Update::Item(item(1, Kind::Text, "Reading.").agent("r1")));
        t.apply(Update::Item(item(2, Kind::Result, "done").agent("r1")));
        let shown = rows(&mut t, &own, 40, 5);
        assert!(is_working(&shown[4]), "{shown:?}");
        // Selected to the end, it isn't copied.
        assert_eq!(shown[1], "  Reading.");
        let (from, to) = (t.point_at(0, 1).unwrap(), t.point_at(39, 4).unwrap());
        t.select(from, to, &own);
        assert_eq!(
            t.selected_text(&own, &Theme::terminal()).unwrap(),
            "Reading.\n\ndone"
        );
        assert_eq!(t.drawn.key_at(4, 4), None);
        // Scrolled up, it isn't counted as new.
        t.selection = None;
        rows(&mut t, &own, 40, 2);
        t.scroll(-1);
        let scrolled = rows(&mut t, &own, 40, 2);
        assert!(scrolled[1].ends_with("↓ end"), "{scrolled:?}");
        worker.state = WorkerState::Finished;
        t.apply(Update::Worker(worker));
        t.follow();
        let shown = rows(&mut t, &own, 40, 5);
        assert!(!shown.iter().any(|row| is_working(row)), "{shown:?}");
    }

    /// The user's messages sit on a band the width of the screen. The
    /// agent's lines and the blank lines between don't.
    #[test]
    fn the_users_messages_sit_on_a_band() {
        use crate::tui::theme::{Catalog, DEFAULT, Depth};
        let theme = Catalog::load(None).theme(DEFAULT, Depth::Rgb);
        assert_ne!(theme.user, theme.background);
        let mut t = Transcript::new();
        t.apply(Update::Item(item(1, Kind::Reply, "Hello.")));
        t.apply(Update::Item(item(
            2,
            Kind::User,
            "Run the tests, then fix what fails",
        )));
        t.apply(Update::Item(item(3, Kind::Reply, "All pass.")));
        let (width, height) = (24, 7);
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        t.draw(&mut buffer, area, &full(), &theme);
        let text = |y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        let on = |y, color| (0..width).all(|x| buffer[(x, y)].bg == color);
        let mut user_rows = 0;
        for y in 0..height {
            if text(y).contains('▎') {
                assert!(on(y, theme.user), "row {y}: {:?}", text(y));
                user_rows += 1;
            } else {
                assert!(on(y, theme.background), "row {y}: {:?}", text(y));
            }
        }
        // The message wraps, and every line of it is on the band.
        assert_eq!(user_rows, 2);
    }

    /// A note typed while the agent's reply streams stays below the reply,
    /// muted, and the reply's later chunks join its text, mid-word too.
    /// Once the agent reads the note, it is in the user's colours where it
    /// was, and what the agent says next comes below it. A note left
    /// waiting in a conversation the session went from stays at the end of
    /// that one.
    #[test]
    fn a_note_waits_muted_below_the_streaming_reply() {
        use crate::tui::theme::{Catalog, DEFAULT, Depth};
        use arko_agents::AgentEvent;
        let theme = Catalog::load(None).theme(DEFAULT, Depth::Rgb);
        let feed = crate::remote::Feed::new(MAIN_AGENT);
        let (snapshot, mut updates) = feed.subscribe();
        let mut t = Transcript::new();
        t.reset(snapshot);
        let event = |event: AgentEvent| arko_workflows::WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: uuid::Uuid::nil(),
            agent_name: MAIN_AGENT.into(),
            agent_run_id: uuid::Uuid::nil(),
            event,
        };
        let chunk = |text: &str| event(AgentEvent::AssistantResponseChunk(text.into()));
        let (width, height) = (60, 5);
        let mut draw = |t: &mut Transcript| {
            while let Ok(update) = updates.try_recv() {
                t.apply(update);
            }
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            t.draw(&mut buffer, area, &full(), &theme);
            let rows: Vec<String> = (0..height)
                .map(|y| {
                    let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                    row.trim_end().to_string()
                })
                .collect();
            // The colours of the note's bar and text.
            let note = rows
                .iter()
                .position(|row| row.contains("Keep the old name."));
            let colours = note.map(|y| (buffer[(0, y as u16)].fg, buffer[(2, y as u16)].fg));
            (rows, colours)
        };

        feed.event(&chunk("It turned out to be a kit f"), None);
        feed.note("Keep the old name.", Source::Terminal, 1, Vec::new());
        feed.event(&chunk("ix, so that went to the kit."), None);
        let (drawn, colours) = draw(&mut t);
        assert_eq!(
            drawn[..3],
            [
                "  It turned out to be a kit fix, so that went to the kit.",
                "",
                "▎ Keep the old name.  pending",
            ]
        );
        assert_eq!(colours, Some((theme.dim, theme.dim)));

        feed.event(
            &event(AgentEvent::ContextInjected {
                ids: vec!["note:1".into()],
                bytes: 100,
            }),
            None,
        );
        feed.event(&chunk("Renamed it back."), None);
        let (drawn, colours) = draw(&mut t);
        assert_eq!(
            drawn,
            [
                "  It turned out to be a kit fix, so that went to the kit.",
                "",
                "▎ Keep the old name.",
                "",
                "  Renamed it back.",
            ]
        );
        assert_eq!(colours, Some((theme.accent, theme.text)));

        // A note left waiting in the conversation the session went from
        // stays at the end of that one.
        let mut waiting = item(10, Kind::User, "Use tabs.");
        waiting.delivery = Some(Delivery::Note);
        waiting.pending = true;
        let mut t = Transcript::new();
        t.apply(Update::Item(item(9, Kind::Text, "Indenting.")));
        t.apply(Update::Item(waiting));
        t.apply(Update::Item(item(11, Kind::Text, "Still indenting.")));
        t.apply(Update::Item(item(
            12,
            Kind::Conversation,
            "New conversation",
        )));
        t.apply(Update::Item(item(13, Kind::Text, "Hello.")));
        let shown = rows(&mut t, &full(), 40, 9);
        let at = |text: &str| shown.iter().position(|row| row.contains(text));
        assert!(
            at("Still indenting.") < at("Use tabs.") && at("Use tabs.") < at("New conversation"),
            "{shown:?}"
        );
    }

    /// Claude Code's message about a request that failed mid-response shows
    /// once, as a line in the theme's warning colour (the terminal's yellow
    /// in the terminal theme), apart from the reply. The error that carries
    /// the same text adds no line.
    #[test]
    fn a_failed_requests_message_shows_once_in_the_warning_colour() {
        use crate::tui::theme::{Catalog, DEFAULT, Depth};
        use arko_agents::AgentEvent;
        let note = "API Error: Connection lost mid-response. The response above may be incomplete.";
        let event = |event: AgentEvent| arko_workflows::WorkflowEvent {
            timestamp: chrono::Utc::now(),
            workflow_run_id: uuid::Uuid::nil(),
            agent_name: MAIN_AGENT.into(),
            agent_run_id: uuid::Uuid::nil(),
            event,
        };
        for theme in [
            Catalog::load(None).theme(DEFAULT, Depth::Rgb),
            Theme::terminal(),
        ] {
            let feed = crate::remote::Feed::new(MAIN_AGENT);
            let (snapshot, mut updates) = feed.subscribe();
            let mut t = Transcript::new();
            t.reset(snapshot);
            feed.event(
                &event(AgentEvent::AssistantResponseChunk("The answer is".into())),
                None,
            );
            feed.event(
                &event(AgentEvent::ServiceWarning { text: note.into() }),
                None,
            );
            feed.event(
                &event(AgentEvent::ErrorEncountered {
                    error_message: note.into(),
                    requires_approval_to_retry: true,
                    tool_call_id: None,
                    error_kind: None,
                }),
                None,
            );
            while let Ok(update) = updates.try_recv() {
                t.apply(update);
            }
            let (width, height) = (100, 6);
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            t.draw(&mut buffer, area, &full(), &theme);
            let rows: Vec<String> = (0..height)
                .map(|y| {
                    let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                    row.trim_end().to_string()
                })
                .collect();
            let warned: Vec<usize> = (0..rows.len())
                .filter(|y| rows[*y].contains(note))
                .collect();
            assert_eq!(warned.len(), 1, "{rows:?}");
            let y = warned[0];
            let x = rows[y].split(note).next().unwrap().chars().count() as u16;
            assert_eq!(buffer[(x, y as u16)].fg, theme.warning, "{rows:?}");
            let reply = rows.iter().position(|row| row.trim() == "The answer is");
            let reply = reply.unwrap_or_else(|| panic!("the reply is alone in its row: {rows:?}"));
            assert_eq!(buffer[(2, reply as u16)].fg, theme.text, "{rows:?}");
        }
    }

    /// Scrolled up, the screen stays put as items come, and the marker
    /// counts them; at the bottom again, it follows.
    #[test]
    fn nothing_moves_under_the_reader() {
        let mut t = Transcript::new();
        for id in 1..=20 {
            t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
        }
        let filter = full();
        let before = rows(&mut t, &filter, 30, 6);
        assert_eq!(before[5], "  line 20");
        t.scroll(-10);
        let scrolled = rows(&mut t, &filter, 30, 6);
        assert!(!t.following());
        for id in 21..=25 {
            t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
        }
        let after = rows(&mut t, &filter, 30, 6);
        assert_eq!(after[..5], scrolled[..5]);
        assert!(after[5].contains("5 new lines"), "{after:?}");
        // Streaming text into an item below doesn't move it either.
        t.apply(Update::Item(item(26, Kind::Text, "more")));
        t.apply(Update::Append {
            id: 26,
            text: " and more".into(),
        });
        assert_eq!(rows(&mut t, &filter, 30, 6)[..5], scrolled[..5]);
        // A narrower screen keeps about the same place.
        let narrow = rows(&mut t, &filter, 20, 6);
        assert_eq!(narrow[0].trim(), scrolled[0].trim());
        t.scroll(1000);
        let bottom = rows(&mut t, &filter, 30, 6);
        assert!(t.following());
        assert!(bottom[5].contains("more and more"));
    }

    /// A selection copies what the screen shows as it is written: without
    /// the margins, the code's gutter and the output's prefix, and with a
    /// line the screen wrapped joined again.
    #[test]
    fn a_selection_copies_clean_text() {
        let mut t = Transcript::new();
        t.apply(Update::Item(item(
            1,
            Kind::Reply,
            "A long line that the screen wraps into two\n\n```\nfn main() {}\n```",
        )));
        let mut call = item(2, Kind::Call, "$ ls");
        call.status = Some(CallStatus::Ok);
        call.outcome = Some("2 files".into());
        t.apply(Update::Item(call));
        let mut result = item(3, Kind::Result, "exit code 0, 0.1 s").detail("a.rs\nb.rs");
        result.call = Some(2);
        t.apply(Update::Item(result));
        let filter = full();
        let screen = rows(&mut t, &filter, 30, 12);
        assert_eq!(screen[0], "  A long line that the screen");
        assert_eq!(screen[1], "  wraps into two");
        let (from, to) = (t.point_at(0, 0).unwrap(), t.point_at(29, 11).unwrap());
        t.select(from, to, &filter);
        assert_eq!(
            t.selected_text(&filter, &Theme::terminal()).unwrap(),
            "A long line that the screen wraps into two\n\nfn main() {}\n\n$ ls · ✓ 2 files\na.rs\nb.rs"
        );
        // Part of a line: from its column to the other's.
        let (from, to) = (t.point_at(4, 0).unwrap(), t.point_at(12, 0).unwrap());
        t.select(from, to, &filter);
        assert_eq!(
            t.selected_text(&filter, &Theme::terminal()).unwrap(),
            "long line"
        );
        // It is highlighted, from its first cell to its last.
        let area = Rect::new(0, 0, 30, 12);
        let mut buffer = Buffer::empty(area);
        t.draw(&mut buffer, area, &filter, &Theme::terminal());
        let reversed = |x: u16| buffer[(x, 0)].modifier.contains(Modifier::REVERSED);
        assert!(!reversed(3) && reversed(4) && reversed(12) && !reversed(13));
        // Text added to an item keeps it; another width (lines wrapped
        // otherwise) takes it away.
        t.apply(Update::Append {
            id: 1,
            text: " more".into(),
        });
        assert!(t.selection.is_some());
        rows(&mut t, &filter, 26, 12);
        assert!(t.selection.is_none());
    }

    /// The banner is a box that fits the screen, with a blank line above it
    /// and the same margin on both sides: its long path cut in the middle,
    /// its notes under a rule. A copy takes the values and notes, not the
    /// box or the labels.
    #[test]
    fn the_banner_is_a_box_whose_copy_is_its_values() {
        let mut t = Transcript::new();
        t.banner = Some(Banner {
            model: "Claude Opus 5.5 (xhigh)".into(),
            workers: "parallel workers".into(),
            workspace: format!("/home/someone/{}/project", "deep/".repeat(20)),
            session: "20261014-101010-abcd".into(),
            project: Some("AGENTS.md".into()),
            tools: vec!["1 skill".into()],
            model_note: None,
            notes: vec![("Outside access is on.".into(), Some(Level::Warn))],
        });
        let filter = full();
        let screen = rows(&mut t, &filter, 80, 14);
        assert_eq!(screen[0].trim(), "");
        assert_eq!(screen[1], format!("  ┌ Arko {}┐", "─".repeat(68)));
        assert_eq!(
            screen[2],
            format!("  │ Model      Claude Opus 5.5 (xhigh){} │", " ".repeat(38))
        );
        assert!(
            screen[4].starts_with("  │ Workspace  /home/someone/deep/"),
            "{screen:?}"
        );
        assert!(screen[4].contains("…") && screen[4].ends_with("/project │"));
        // Two columns of margin on the right, as on the left.
        assert!(screen.iter().all(|row| text_width(row) <= 78));
        assert_eq!(screen[8], format!("  ├{}┤", "─".repeat(74)));
        assert!(screen[9].starts_with("  │ Outside access is on."));
        assert_eq!(screen[10], format!("  └{}┘", "─".repeat(74)));
        let (from, to) = (t.point_at(0, 0).unwrap(), t.point_at(79, 10).unwrap());
        t.select(from, to, &filter);
        let copied = t.selected_text(&filter, &Theme::terminal()).unwrap();
        // (The path whole, as it is, not as the screen shortens it.)
        let path = format!("/home/someone/{}/project", "deep/".repeat(20));
        assert!(
            copied.starts_with(&format!(
                "Claude Opus 5.5 (xhigh)\nparallel workers\n{path}\n"
            )),
            "{copied}"
        );
        assert!(
            copied.ends_with("20261014-101010-abcd\nAGENTS.md\n1 skill\nOutside access is on."),
            "{copied}"
        );
        assert!(!copied.contains('│') && !copied.contains("Model"));
    }

    /// While a drag holds the view, output that comes doesn't move the
    /// text under the mouse, even at the bottom.
    #[test]
    fn a_drag_holds_the_text_still() {
        let mut t = Transcript::new();
        for id in 1..=5 {
            t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
        }
        let filter = full();
        let before = rows(&mut t, &filter, 20, 4);
        t.hold();
        t.apply(Update::Item(item(6, Kind::Output, "line 6")));
        let held = rows(&mut t, &filter, 20, 4);
        assert_eq!(held[..3], before[..3]);
        t.release();
        t.follow();
        assert_eq!(rows(&mut t, &filter, 20, 4)[3], "  line 6");
    }

    /// Old items leaving the history don't shift the screen.
    #[test]
    fn trimming_keeps_the_place() {
        let mut t = Transcript::new();
        for id in 1..=KEPT as u64 {
            t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
        }
        t.scroll(-50);
        let before = rows(&mut t, &full(), 30, 5);
        for id in KEPT as u64 + 1..=KEPT as u64 + 10 {
            t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
        }
        assert_eq!(rows(&mut t, &full(), 30, 5)[..4], before[..4]);
    }

    /// The views filter and group the same items.
    #[test]
    fn views_are_filters_over_the_items() {
        let mut t = Transcript::new();
        let mut user = item(1, Kind::User, "Fix it");
        user.turn = Some(1);
        t.apply(Update::Item(user));
        for (id, text) in [(2, "read a.rs"), (3, "edit a.rs")] {
            let mut call = item(id, Kind::Call, text);
            call.turn = Some(1);
            call.status = Some(if id == 3 {
                CallStatus::Failed
            } else {
                CallStatus::Ok
            });
            t.apply(Update::Item(call));
        }
        let mut line = item(4, Kind::WorkerLine, "Test it");
        line.worker = Some("r1".into());
        line.turn = Some(1);
        t.apply(Update::Item(line));
        let mut worker_call = item(5, Kind::Call, "cargo test").agent("r1");
        worker_call.status = Some(CallStatus::Running);
        t.apply(Update::Item(worker_call));
        t.apply(Update::Item(
            item(6, Kind::Text, "Thinking about it").agent("r1"),
        ));
        t.apply(Update::Worker(WorkerSummary {
            title: "Test it".into(),
            state: WorkerState::Running,
            tool_calls: 1,
            last: Some("cargo test".into()),
            ..WorkerSummary::new("r1")
        }));
        let mut reply = item(7, Kind::Reply, "Done.");
        reply.turn = Some(1);
        t.apply(Update::Item(reply));

        let full = rows(&mut t, &Filter::Main(View::Full), 60, 8);
        assert!(full.iter().any(|r| r.contains("r1 cargo test")), "{full:?}");
        assert!(!full.iter().any(|r| r.contains("Thinking about it")));
        let quiet = rows(&mut t, &Filter::Main(View::Quiet), 60, 8);
        assert!(quiet.iter().any(|r| r.contains("read a.rs")));
        assert!(!quiet.iter().any(|r| r.contains("r1 cargo test")));
        assert!(
            quiet
                .iter()
                .any(|r| r.contains("r1 Test it · 1 tool call · running · cargo test")),
            "{quiet:?}"
        );
        let messages = rows(&mut t, &Filter::Main(View::Messages), 60, 8);
        assert!(
            messages
                .iter()
                .any(|r| r.contains("▸ 2 tool calls · 1 failed")),
            "{messages:?}"
        );
        assert!(!messages.iter().any(|r| r.contains("read a.rs")));
        let worker = rows(&mut t, &Filter::Worker("r1".into()), 60, 8);
        assert!(worker[0].contains("r1 Test it · running"), "{worker:?}");
        assert!(worker.iter().any(|r| r.contains("Thinking about it")));
        assert!(!worker.iter().any(|r| r.contains("Fix it")));
        // Its own view doesn't name it on its calls.
        assert!(
            worker.iter().any(|r| r.starts_with("▸ cargo test")),
            "{worker:?}"
        );
        // A turn of text and notices only has no counter.
        let mut text = item(8, Kind::Text, "Thinking.");
        text.turn = Some(2);
        t.apply(Update::Item(text));
        let mut notice = item(9, Kind::Notice, "news");
        notice.level = Some(Level::Info);
        notice.turn = Some(2);
        t.apply(Update::Item(notice));
        let messages = rows(&mut t, &Filter::Main(View::Messages), 60, 12);
        assert!(
            !messages.iter().any(|r| r.contains("0 tool calls")),
            "{messages:?}"
        );
        // The agent's text stays, as on the web page.
        assert!(messages.iter().any(|r| r.contains("Thinking.")));
        assert_eq!(t.item(1).unwrap().agent, MAIN_AGENT);
    }

    /// The line of a worker that a worker started is in its parent's own
    /// transcript, where the parent started it, and not in the main one.
    #[test]
    fn a_workers_worker_has_its_line_in_the_parents_transcript() {
        let mut t = Transcript::new();
        let mut user = item(1, Kind::User, "Review it");
        user.turn = Some(1);
        t.apply(Update::Item(user));
        let mut first = item(2, Kind::WorkerLine, "Review the files");
        first.worker = Some("r1".into());
        first.turn = Some(1);
        t.apply(Update::Item(first));
        let mut second = item(3, Kind::WorkerLine, "Count the files").agent("r1");
        second.worker = Some("r2".into());
        t.apply(Update::Item(second));
        t.apply(Update::Worker(WorkerSummary {
            title: "Review the files".into(),
            state: WorkerState::Running,
            ..WorkerSummary::new("r1")
        }));
        t.apply(Update::Worker(WorkerSummary {
            title: "Count the files".into(),
            state: WorkerState::Running,
            parent: Some("r1".into()),
            depth: 2,
            ..WorkerSummary::new("r2")
        }));
        for view in [View::Full, View::Quiet, View::Messages] {
            let main = rows(&mut t, &Filter::Main(view), 70, 10);
            assert!(
                main.iter().any(|r| r.contains("r1 Review the files")),
                "{main:?}"
            );
            assert!(!main.iter().any(|r| r.contains("r2")), "{main:?}");
        }
        let own = rows(&mut t, &Filter::Worker("r1".into()), 70, 10);
        assert!(
            own.iter().any(|r| r.contains("r2 Count the files")),
            "{own:?}"
        );
    }

    /// A worker's stop or failure in the main view names the worker that
    /// started it, and the main agent's workers are named alone.
    #[test]
    fn a_workers_notice_names_the_worker_that_started_it() {
        let mut t = Transcript::new();
        let mut user = item(1, Kind::User, "Review it");
        user.turn = Some(1);
        t.apply(Update::Item(user));
        t.apply(Update::Worker(WorkerSummary {
            title: "Review the files".into(),
            state: WorkerState::Stopped,
            ..WorkerSummary::new("r1")
        }));
        t.apply(Update::Worker(WorkerSummary {
            title: "Count the files".into(),
            state: WorkerState::Stopped,
            parent: Some("r1".into()),
            depth: 2,
            ..WorkerSummary::new("r2")
        }));
        // (The feed's stop notices are Info.)
        let mut second = item(2, Kind::Notice, "stopped (its lead r1 was stopped)").agent("r2");
        second.level = Some(Level::Info);
        second.parent = Some("r1".into());
        second.stop = true;
        second.turn = Some(1);
        t.apply(Update::Item(second));
        let mut first = item(3, Kind::Notice, "stopped (the user stopped the worker)").agent("r1");
        first.level = Some(Level::Info);
        first.stop = true;
        first.turn = Some(1);
        t.apply(Update::Item(first));
        // Another Info notice of a worker stays out of the main view.
        let mut aside = item(4, Kind::Notice, "an aside").agent("r2");
        aside.level = Some(Level::Info);
        aside.turn = Some(1);
        t.apply(Update::Item(aside));
        let main = rows(&mut t, &Filter::Main(View::Full), 70, 10);
        assert!(
            main.iter()
                .any(|r| r.contains("r2 (under r1) stopped (its lead r1 was stopped)")),
            "{main:?}"
        );
        assert!(
            main.iter()
                .any(|r| r.contains("r1 stopped (the user stopped the worker)")),
            "{main:?}"
        );
        assert!(!main.iter().any(|r| r.contains("an aside")), "{main:?}");
        // In its own view a worker's lines don't name it or its starter.
        let own = rows(&mut t, &Filter::Worker("r2".into()), 70, 10);
        assert!(
            own.iter()
                .any(|r| r.contains("stopped (its lead r1 was stopped)"))
                && !own.iter().any(|r| r.contains("(under r1)")),
            "{own:?}"
        );
    }

    /// A worker's time is the total of its runs, also those before Arko
    /// quit and the session was resumed, and not only its latest run.
    #[test]
    fn a_worker_s_time_counts_all_its_runs() {
        let now = chrono::Utc::now().timestamp_millis();
        let summary = |state, ran_ms, running_since: Option<i64>| WorkerSummary {
            title: "Test it".into(),
            state,
            tool_calls: 2,
            // Its latest run began five minutes ago.
            started_at: Some(now - 300_000),
            ended_at: (state != WorkerState::Running).then_some(now),
            ran_ms,
            running_since,
            ..WorkerSummary::new("r1")
        };
        // Two hours in earlier runs, five minutes in the one going on.
        let running = summary(WorkerState::Running, 7_200_000, Some(now - 300_000));
        assert_eq!(run_time(&running, now), Some(7_500_000));
        assert_eq!(worker_time(&running).as_deref(), Some("2h 5m"));
        let finished = summary(WorkerState::Finished, 7_500_000, None);
        assert_eq!(run_time(&finished, now + 60_000), Some(7_500_000));
        // A worker waiting for its first run has no time yet.
        let queued = summary(WorkerState::Queued, 0, None);
        assert_eq!(worker_time(&queued), None);

        let mut t = Transcript::new();
        let mut line = item(1, Kind::WorkerLine, "Test it");
        line.worker = Some("r1".into());
        line.turn = Some(1);
        t.apply(Update::Item(line));
        t.apply(Update::Worker(finished));
        let quiet = rows(&mut t, &Filter::Main(View::Quiet), 60, 4);
        assert!(
            quiet
                .iter()
                .any(|r| r.contains("r1 Test it · 2 tool calls · 2h 5m · finished")),
            "{quiet:?}"
        );
        let own = rows(&mut t, &Filter::Worker("r1".into()), 60, 4);
        assert!(own[0].contains("r1 Test it · finished · 2h 5m"), "{own:?}");
    }
}

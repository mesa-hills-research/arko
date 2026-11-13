//! The transcript, the feed's items as the reader sees them. It keeps the
//! items (and the workers' summaries) as the feed sends them, and draws
//! them through a view, a filter and grouping over the items, so changing
//! the view changes no item.
//!
//! - **Full** shows everything, the workers' calls inline.
//! - **Quiet** shows the main agent's work. Each worker is one line that
//!   changes in place.
//! - **Messages** shows what was said (the user's messages, the agent's replies,
//!   messages and questions, warnings and errors) and the workers' lines.
//!   Each turn's tool activity is one line that counts it.
//! - **A worker's own** (the worker view) shows its text, calls, results and
//!   report.
//!
//! The reading position is the reader's. While they are at the bottom the
//! transcript follows what comes. Once they scroll up it stays where it is,
//! anchored on the item at the top, however much comes below, and a marker
//! counts what is new.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::markdown;
use super::motion;
use super::theme::{Theme, Tint};
use super::wrap::{self, Join, Wrapped, width as text_width};
use crate::glyphs;
use crate::remote::{
    ActKind, CallStatus, ContentKind, Delivery, Item, Kind, Level, MAIN_AGENT, Snapshot, Source,
    Update, WorkerState, WorkerSummary, content, quiet,
};

/// The items kept, as the feed keeps them, the main agent's and the
/// workers' apart.
const KEPT: usize = 3000;

/// How much of the work the transcript shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
/// (as the feed keeps them). An older command's block keeps its first lines.
const WHOLES_KEPT: usize = 8 * 1024 * 1024;

/// An item, with its lines as last drawn.
struct Entry {
    item: Item,
    cache: Option<Cached>,
    /// A call shows its content in the Full view, a content shows its
    /// whole, or an agent's thinking shows whole (a click opened it).
    open: bool,
    /// An agent's thinking, or a call in a Quiet row of several, is opened
    /// in the quiet view, which a click does and nothing else (`open` is
    /// for the full view).
    open_quiet: bool,
    /// When it came, or last changed (text streamed into it).
    changed: Instant,
    /// A number no other state of an entry had, new each time its lines
    /// change, by which a Quiet row that shows it knows to draw again.
    revision: u64,
}

impl Entry {
    fn new(item: Item) -> Self {
        Self {
            item,
            cache: None,
            open: false,
            open_quiet: false,
            changed: Instant::now(),
            revision: next_revision(),
        }
    }

    /// Its lines change, so they're drawn again, here and in a Quiet row.
    fn touch(&mut self) {
        self.cache = None;
        self.revision = next_revision();
    }
}

/// A revision no entry had yet.
fn next_revision() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

struct Cached {
    /// What the lines were drawn for.
    width: usize,
    style: u64,
    /// A running call's time.
    clock: Option<String>,
    open: bool,
    folds: bool,
    /// It was drawn as a worker's row in its lead's view.
    status: Option<quiet::WorkerRow>,
    lines: Vec<Wrapped>,
}

pub use crate::remote::protocol::duration;
pub use crate::ui::Banner;

/// The model and effort workers start with unless told otherwise. A worker's
/// line names its model or effort only when they are other ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usual {
    pub model: String,
    pub effort: String,
}

/// What a block of the drawn transcript is.
#[derive(Debug, Clone, PartialEq)]
enum Shown {
    /// The session's banner, first.
    Banner,
    /// An entry's lines.
    Entry(usize),
    /// A worker's summary, where it started (`worker_line`).
    Worker(String),
    /// A worker view's heading, the worker's task.
    Heading(String),
    /// A turn's tool activity, counted (the Messages view). Open, the
    /// turn's Quiet rows show under it, each where it came.
    Counter {
        calls: usize,
        failed: usize,
        running: usize,
        open: bool,
    },
    /// "Working…", at the end, while the agent waits on its model and
    /// nothing else shows it.
    Working,
    /// A row of the Quiet view, with calls in a few words, merged with their
    /// neighbours of a kind.
    Quiet(Arc<QuietRow>),
}

/// A row of the Quiet view.
#[derive(Debug, Clone, PartialEq)]
struct QuietRow {
    /// Its entries in order, which are its calls, the thinking after them and the
    /// calls' results, which show when it's open.
    members: Vec<usize>,
    words: quiet::Words,
    /// It holds several calls, each of which shows as its Full line.
    merged: bool,
    /// It has more to show than its words. One that hasn't has no fold.
    opens: bool,
}

/// A Quiet row's lines, with what they were drawn for.
struct QuietCached {
    /// Its words, and each item it shows with that item's revision.
    words: quiet::Words,
    members: Vec<(u64, u64)>,
    open: bool,
    width: usize,
    style: u64,
    clock: Option<String>,
    lines: Vec<Wrapped>,
    /// Where each item's lines are among them, while the row is open.
    spans: Vec<(u64, std::ops::Range<usize>)>,
}

/// A line held on a row of the screen at the next draw, so the line a click
/// opened or closed a fold at stays under the pointer.
#[derive(Debug, Clone, Copy)]
struct Pin {
    /// The block it's in, and its line there.
    key: u64,
    line: usize,
    /// The row of the area it stays on.
    row: usize,
}

/// What the keyboard is on (Ctrl+G): a block that opens (a Quiet row, a
/// call, a content, a thinking), by its key, or something inside an open
/// Quiet row (a call, a content, a thinking), by the row's key and its item.
/// A row's key is its first call's, so the row and that call are apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Block(u64),
    Inner { row: u64, item: u64 },
}

/// Where a fold opens and closes, with the item whose fold it is, the block it
/// shows in (an open Quiet row's, for an item inside it), and its line
/// there. A click, the keyboard and a selection all find a fold by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    pub item: u64,
    pub block: u64,
    pub line: usize,
}

/// What a view shows in order, before it's laid out in blocks.
#[derive(Clone)]
enum Placed {
    Entry(usize),
    Quiet(Arc<QuietRow>),
}

/// The key of the "Working…" line, after every item's.
const WORKING_KEY: u64 = u64::MAX;

/// The bit that sets a turn's count of calls (Messages) apart from the
/// Quiet row of its first call, which its key is otherwise.
const COUNT_KEY: u64 = 1 << 63;

/// Whether the block `key` is a turn's count of calls.
fn is_count(key: u64) -> bool {
    key != WORKING_KEY && key & COUNT_KEY != 0
}

/// A turn's calls as the Messages view counts them, on one line where the
/// first one came, or the thinking that led to it.
#[derive(Debug, Default)]
struct Counted {
    /// Its first call, by which it opens.
    first: u64,
    /// Where its line stands, the first call or the thinking before it.
    from: u64,
    calls: usize,
    failed: usize,
    running: usize,
    open: bool,
}

/// Text that hasn't grown for this long, while its agent works, no longer
/// shows that the agent is under way (it may be writing a long call).
const QUIET: Duration = Duration::from_secs(2);

/// What an agent that works is under way with, as its latest items say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    /// It waits on its model, and nothing the model sent shows it yet (a
    /// turn began, a call's result went back, an answer came).
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Motion {
    /// Its thinking that streams, by ID, the live line, whose spinner turns.
    thinking: Option<u64>,
    /// The "Working…" line, at the end.
    working: bool,
    /// What that line says instead of "Working…", in the Messages view
    /// while a call of the agent runs ("Waiting for r91…").
    label: Option<String>,
}

/// A block of lines in the drawn transcript.
struct Block {
    /// The ID of its (first) item, where the reading position anchors.
    key: u64,
    shown: Shown,
    /// A blank line before it.
    gap: bool,
    lines: usize,
    /// A blank line after it (the "Working…" line has one, at the end).
    tail: bool,
    /// It's open, so it stands apart from its neighbours by a blank line on
    /// each side.
    open: bool,
}

impl Block {
    fn height(&self) -> usize {
        self.lines + usize::from(self.gap) + usize::from(self.tail)
    }
}

/// Where the reader is, the block at the top of the screen, by its key,
/// and how many of its lines are above the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anchor {
    key: u64,
    /// Lines of the block above the top, as a share of its height (so a
    /// re-wrap at another width keeps about the same place).
    offset: usize,
    height: usize,
}

/// A row of the screen, with the block it shows, and which of its lines (the
/// blank line before a block, when it has one, is line 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowRef {
    key: u64,
    line: usize,
    /// The blank line before the block.
    gap: bool,
}

/// A place in the transcript's text, a block (by its key), a line of it,
/// and a column. It stays on its text as the screen scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub key: u64,
    pub line: usize,
    pub col: usize,
}

/// Text the reader highlighted with the mouse, from where the drag began
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
    /// Where the screen's top was, what holding the view still keeps.
    top: Option<Anchor>,
    /// The "new" marker's area, while it shows.
    pub marker: Option<Rect>,
    /// The keys of the counted turns' lines.
    counters: Vec<u64>,
    /// Something on the screen moves, so it is drawn again at the next frame.
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
    /// was). The marker counts the lines that came since.
    baseline: Option<(Filter, usize)>,
    /// The session's banner, which opens the main transcript.
    pub banner: Option<Banner>,
    /// What workers start with unless told otherwise.
    pub usual: Usual,
    /// The text highlighted with the mouse.
    pub selection: Option<Selection>,
    /// The view holds still (a drag selects text). It doesn't follow new
    /// output, even at the bottom.
    hold: bool,
    /// The lines drawn last, by row.
    pub drawn: Drawn,
    /// Lines a scroll moved while the screen was not drawn yet.
    pending_scroll: i64,
    /// A fold was opened or closed, and the next draw keeps the line clicked
    /// on the row of the area it was on.
    pin: Option<Pin>,
    /// The agents' thinking shows whole from the start (`--show-thinking`),
    /// instead of folded to its first line.
    open_thinking: bool,
    /// Since when the main agent works (Unix milliseconds), while it isn't
    /// waiting for the user's answer.
    pub working: Option<i64>,
    /// What showed that the agent was under way, as the blocks were last
    /// made.
    motion: Motion,
    /// The Quiet view's rows the reader opened, by their first call.
    quiet_open: HashSet<u64>,
    /// The Quiet view's rows of one call the reader opened, by that call,
    /// which keep their form ([`quiet::Call::sealed`]), and the thoughts the
    /// reader opened on lines of their own, which stay there.
    quiet_sealed: HashSet<u64>,
    /// The turns whose count of calls the reader opened in the Messages
    /// view, by their first call.
    counts_open: HashSet<u64>,
    /// The calls that have content, a block of their result.
    contents: HashSet<u64>,
    /// The Quiet view's rows' lines as last drawn, by their first call.
    quiet_cache: HashMap<u64, QuietCached>,
    /// One more each time an item comes, changes or goes.
    generation: u64,
    /// The Quiet view's order as last worked out, for a view and a
    /// generation. It's worked out again only when the items change.
    quiet_order: Option<(Filter, u64, Arc<Vec<Placed>>)>,
    /// The row the keyboard is on (Ctrl+G). It shows in the selection's
    /// colour, and the view keeps it in sight.
    pub focus: Option<Target>,
    /// The keyboard moved, so the next draw scrolls its row into sight.
    reveal: bool,
    /// The width the blocks were last made for, which decides what a line
    /// cuts and so what opens.
    width: usize,
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
            usual: Usual::default(),
            selection: None,
            hold: false,
            drawn: Drawn::default(),
            pending_scroll: 0,
            pin: None,
            open_thinking: false,
            working: None,
            motion: Motion::default(),
            quiet_open: HashSet::new(),
            quiet_sealed: HashSet::new(),
            counts_open: HashSet::new(),
            contents: HashSet::new(),
            quiet_cache: HashMap::new(),
            generation: 0,
            quiet_order: None,
            focus: None,
            reveal: false,
            width: 0,
        }
    }

    /// Shows the agents' thinking whole (`open`) or folded, from now on and
    /// in the items already there.
    pub fn set_open_thinking(&mut self, open: bool) {
        self.open_thinking = open;
        for entry in &mut self.entries {
            if entry.item.kind == Kind::Thinking {
                entry.open = open;
                entry.touch();
            }
        }
    }

    /// An entry for `item`, where an agent's thinking opens as `open_thinking`
    /// says, and a command shows its output in the Full view.
    fn entry(&self, item: Item) -> Entry {
        let item = for_terminal(item);
        let open = match item.kind {
            Kind::Thinking => self.open_thinking,
            Kind::Call => item
                .act
                .as_ref()
                .is_some_and(|act| act.kind == ActKind::Command),
            _ => false,
        };
        Entry {
            open,
            ..Entry::new(item)
        }
    }

    /// Notes the calls that have content, from the entries.
    fn note_contents(&mut self) {
        self.contents = self
            .entries
            .iter()
            .filter(|entry| entry.item.kind == Kind::Result)
            .filter_map(|entry| entry.item.call)
            .collect();
    }

    /// What `filter`'s view does with `item`. A view shows its agent's own
    /// items by its rule, and the agent's own workers as their reports and
    /// their messages to it, each a row that opens to its words, a
    /// message's content under it while it's open. The rest of a worker's,
    /// its text, its calls, its conversation with the user and its workers,
    /// is its own view's.
    fn shows(&self, filter: &Filter, item: &Item) -> Show {
        if filter.owns(item) {
            return filter.view().shows(item);
        }
        let status = |item: &Item| self.status_row(item, filter).is_some();
        let content = item.kind == Kind::Result
            && item
                .call
                .and_then(|call| self.item(call))
                .is_some_and(status);
        if status(item) || content {
            Show::Item
        } else {
            Show::Hidden
        }
    }

    /// The row of `item` in `filter`'s view when it is a report or a
    /// message to its lead of one of the view's agent's own workers
    /// ([`quiet::WorkerRow`]).
    fn status_row(&self, item: &Item, filter: &Filter) -> Option<quiet::WorkerRow> {
        let lead = filter.agent();
        if filter.owns(item) || self.lead_of(&item.agent) != lead {
            return None;
        }
        quiet::WorkerRow::of(
            item.kind,
            item.act.as_ref(),
            item.status,
            item.outcome.as_deref(),
            lead,
        )
    }

    /// The agent that started the worker `agent`, its lead, which is the
    /// main agent unless a worker started it.
    fn lead_of(&self, agent: &str) -> &str {
        self.worker(agent)
            .and_then(|worker| worker.parent.as_deref())
            .unwrap_or(MAIN_AGENT)
    }

    /// Whether `item`'s block opens and closes in `filter`'s view, as it
    /// does in every view ([`Transcript::folds`]), or as a worker's row
    /// there, a report that has words and a message whose content came.
    fn folds_in(&self, item: &Item, filter: &Filter) -> bool {
        match self.status_row(item, filter) {
            Some(quiet::WorkerRow::Reported) => !item.text.trim().is_empty(),
            Some(quiet::WorkerRow::Message(_)) => self.contents.contains(&item.id),
            None => self.folds(item),
        }
    }

    /// Whether `item`'s block opens and closes. A call does when it has a
    /// detail or content or its line cuts its words, a content when its
    /// preview leaves some out, and an agent's thinking when its first line
    /// leaves some out, at the width the blocks are made for.
    fn folds(&self, item: &Item) -> bool {
        match item.kind {
            Kind::Thinking => thought_has_more(&item.text, self.width),
            Kind::Call => {
                item.detail.is_some()
                    || self.contents.contains(&item.id)
                    || call_cut(item, self.width, false)
            }
            _ => item.whole.is_some(),
        }
    }

    /// [`Transcript::folds`] for `item` inside an open Quiet row, whose
    /// lines are a rail's width narrower.
    fn folds_inside(&self, item: &Item) -> bool {
        match item.kind {
            Kind::Thinking => thought_has_more(&item.text, self.width.saturating_sub(MARGIN)),
            Kind::Call => {
                item.detail.is_some()
                    || self.contents.contains(&item.id)
                    || call_cut(item, self.width.saturating_sub(MARGIN), true)
            }
            _ => item.whole.is_some(),
        }
    }

    /// Starts over from the feed's `snapshot` (when a client starts, or
    /// fell behind). The reading position and selection stay on their
    /// items.
    pub fn reset(&mut self, snapshot: Snapshot) {
        self.generation += 1;
        self.quiet_cache.clear();
        self.entries = snapshot
            .items
            .into_iter()
            .map(|item| self.entry(item))
            .collect();
        self.workers = snapshot.workers;
        self.trim();
        self.note_contents();
    }

    /// Takes the feed's `update`.
    pub fn apply(&mut self, update: Update) {
        // (Streamed text and summaries leave the items' order as it is.)
        if matches!(
            update,
            Update::Item(_) | Update::Change(_) | Update::Truncate { .. } | Update::Remove { .. }
        ) {
            self.generation += 1;
        }
        match update {
            Update::Item(item) => {
                // A call's content came, so its line opens.
                if item.kind == Kind::Result
                    && let Some(call) = item.call
                {
                    self.contents.insert(call);
                    if let Some(entry) = self.find_mut(call) {
                        entry.touch();
                    }
                }
                let entry = self.entry(item);
                self.entries.push_back(entry);
                self.trim();
            }
            // (Text added at an item's end leaves what is highlighted
            // where it is, so a drag over streaming text still copies it.)
            Update::Append { id, text } => {
                if let Some(entry) = self.find_mut(id) {
                    entry.item.text.push_str(&text);
                    entry.touch();
                    entry.changed = Instant::now();
                }
            }
            Update::Change(item) => {
                self.changed(item.id);
                if let Some(entry) = self.find_mut(item.id) {
                    entry.item = for_terminal(item);
                    entry.touch();
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
                self.note_contents();
            }
            Update::Remove { id } => {
                self.changed(id);
                self.entries.retain(|entry| entry.item.id != id);
                self.note_contents();
            }
            // A moved call's line follows its job through its changes.
            Update::Job(_) => {}
        }
    }

    /// The item `id` changes (a call's outcome came). A selection over it
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
                entry.touch();
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

    /// The key of the block at the top of the screen and how many of its
    /// lines are above it, unless the transcript follows the bottom.
    #[cfg(feature = "dbg-snapshot")]
    pub fn reading_position(&self) -> Option<(u64, usize)> {
        self.anchor.map(|anchor| (anchor.key, anchor.offset))
    }

    /// Follows the bottom again, also during a drag, whose release the
    /// terminal may never report.
    pub fn follow(&mut self) {
        self.anchor = None;
        self.pending_scroll = 0;
        self.hold = false;
    }

    /// Scrolls by `lines` (up when negative) at the next draw.
    pub fn scroll(&mut self, lines: i64) {
        self.pending_scroll += lines;
    }

    /// Whether the block `key` (as drawn last) is a turn's counted calls.
    pub fn is_counter(&self, key: u64) -> bool {
        self.drawn.counters.contains(&key)
    }

    /// Opens or closes the turn's count of calls `key` (Messages), its line
    /// held on the screen's row `y` at the next draw, so what it opens to
    /// comes below it.
    pub fn toggle_count(&mut self, key: u64, y: u16) {
        self.open_count(key, None);
        let row = y.saturating_sub(self.drawn.area.y) as usize;
        if let Some(at) = self.drawn.rows.get(row).copied().flatten()
            && at.key == key
        {
            self.pin = Some(Pin {
                key,
                line: at.line,
                row,
            });
        }
    }

    /// Opens (`Some(true)`), closes (`Some(false)`) or toggles (`None`) the
    /// turn's count of calls `key` (Messages).
    fn open_count(&mut self, key: u64, open: Option<bool>) {
        let first = key & !COUNT_KEY;
        let now = self.counts_open.contains(&first);
        if open.unwrap_or(!now) == now {
            return;
        }
        if now {
            self.counts_open.remove(&first);
        } else {
            self.counts_open.insert(first);
        }
        self.generation += 1;
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
        // The calls of one response may run side by side, so one that ended
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
    /// its calls, which is its thinking as it streams, or else "Working…" while it
    /// waits on its model with nothing else to show. `opened` holds the
    /// entries Messages shows as Quiet does, where a turn's count is open.
    fn motion(&self, filter: &Filter, opened: &HashSet<usize>) -> Motion {
        let Some((agent, since)) = self.under_way(filter) else {
            return Motion::default();
        };
        // (The Messages view leaves the agent's thinking out, but where its
        // turn's count is open.)
        let thinking_shown = |id: u64| {
            filter.view() != View::Messages || self.index(id).is_some_and(|at| opened.contains(&at))
        };
        let working = |working| Motion {
            working,
            ..Motion::default()
        };
        match self.busy(agent, since) {
            Busy::Model => working(true),
            Busy::Text(grown) => working(grown.elapsed() >= QUIET),
            // The thinking that streams is the live line itself. (It stands
            // on a line of its own until the next call comes.)
            Busy::Thinking(id) if thinking_shown(id) => Motion {
                thinking: Some(id),
                ..Motion::default()
            },
            Busy::Thinking(_) => working(true),
            // The Messages view counts calls, so the line says what the
            // agent's call does while it runs, unless its turn's count is
            // open, where its row shows that.
            Busy::Tool
                if filter.view() == View::Messages
                    && !self.running(agent).is_some_and(|at| opened.contains(&at)) =>
            {
                Motion {
                    working: true,
                    label: self.doing(agent),
                    ..Motion::default()
                }
            }
            Busy::Tool | Busy::Done => Motion::default(),
        }
    }

    /// What the latest call of `agent` that runs does ("Running cargo
    /// test…").
    fn doing(&self, agent: &str) -> Option<String> {
        self.running(agent)
            .and_then(|at| self.entries[at].item.act.as_ref())
            .map(|act| format!("{}…", quiet::doing(act)))
    }

    /// The entry of the latest call of `agent` that runs.
    fn running(&self, agent: &str) -> Option<usize> {
        self.entries.iter().rposition(|entry| {
            let item = &entry.item;
            item.agent == agent
                && item.kind == Kind::Call
                && item.status == Some(CallStatus::Running)
        })
    }

    /// The agent of `filter`'s view and when its turn began (Unix
    /// milliseconds), while it works.
    fn under_way<'a>(&self, filter: &'a Filter) -> Option<(&'a str, i64)> {
        match filter {
            Filter::Main(_) => self.working.map(|since| (MAIN_AGENT, since)),
            Filter::Worker(id, _) => self
                .worker(id)
                .filter(|worker| worker.state == WorkerState::Running)
                .map(|_| (id.as_str(), i64::MIN)),
        }
    }

    /// Whether `agent` made a call or thought in its turn that began at
    /// `since`, meaning its own run of calls has begun. Before that, the calls of
    /// its workers and their cards don't make one.
    fn calls_begun(&self, agent: &str, since: i64) -> bool {
        self.entries.iter().rev().any(|entry| {
            let item = &entry.item;
            item.agent == agent
                && item.at >= since
                && matches!(item.kind, Kind::Call | Kind::Result | Kind::Thinking)
        })
    }

    /// The blocks of `filter`'s view of the items, at `width`.
    fn blocks(&mut self, filter: &Filter, theme: &Theme, width: usize) -> Vec<Block> {
        self.width = width;
        // A worker's own view doesn't name it on every line.
        let own = matches!(filter, Filter::Worker(..));
        let style = theme_key(theme) ^ u64::from(own);
        let mut blocks: Vec<Block> = Vec::new();
        // (Key 0 is before every item. It scrolls away with them.)
        if let (Filter::Main(_), Some(banner)) = (filter, &self.banner) {
            blocks.push(Block {
                key: 0,
                shown: Shown::Banner,
                gap: false,
                lines: banner_lines(banner, theme, width).len(),
                tail: false,
                open: false,
            });
        }
        if let Filter::Worker(id, _) = filter
            && let Some(worker) = self.worker(id)
        {
            blocks.push(Block {
                key: 0,
                shown: Shown::Heading(worker.id.clone()),
                gap: false,
                lines: heading_lines(worker, theme, width).len(),
                tail: false,
                open: false,
            });
        }
        // The Messages view counts each turn's tool activity on one line,
        // where the activity began. A count that's open shows the turn's
        // Quiet rows, each where it came.
        let (counts, opened) = self.counted(filter);
        let as_quiet = filter.in_view(View::Quiet);
        // The group of the block before, and whether it's open. A blank line
        // stands between what is said and the work, and on both sides of
        // anything open, and lines of work stack.
        let mut last: Option<(Group, bool)> = blocks.first().map(|_| (Group::Prose, false));
        let now = chrono::Utc::now().timestamp_millis();
        let quiet = filter.view() == View::Quiet;
        let placed: Arc<Vec<Placed>> = if quiet || !opened.is_empty() {
            match &self.quiet_order {
                Some((seen, generation, placed))
                    if seen == filter && *generation == self.generation =>
                {
                    placed.clone()
                }
                _ => {
                    let placed = Arc::new(self.quiet_placed(filter, &counts, &opened));
                    self.quiet_order = Some((filter.clone(), self.generation, placed.clone()));
                    placed
                }
            }
        } else {
            Arc::new(self.order().into_iter().map(Placed::Entry).collect())
        };
        // (Before the lines, because the thinking that streams has the
        // spinner.)
        self.motion = self.motion(filter, &opened);
        for placed in placed.iter() {
            let (index, shown) = match placed {
                Placed::Quiet(row) => (row.members[0], Some(Shown::Quiet(row.clone()))),
                Placed::Entry(index) => (*index, None),
            };
            let item = &self.entries[index].item;
            let opens = opened.contains(&index);
            if let Some(count) = counts.get(&item.turn).filter(|count| count.from == item.id) {
                blocks.push(Block {
                    key: COUNT_KEY | count.first,
                    shown: Shown::Counter {
                        calls: count.calls,
                        failed: count.failed,
                        running: count.running,
                        open: count.open,
                    },
                    gap: last.is_some_and(|(group, open)| open || group != Group::Activity),
                    lines: 1,
                    tail: false,
                    open: false,
                });
                last = Some((Group::Activity, false));
            }
            let shown = match shown {
                Some(shown) => shown,
                None => match self.shows(if opens { &as_quiet } else { filter }, item) {
                    Show::Hidden | Show::Count => continue,
                    Show::Worker(id) => Shown::Worker(id),
                    // A call's content shows while the call is open.
                    Show::Item if self.closed_content(item) => continue,
                    Show::Item => Shown::Entry(index),
                },
            };
            let group = match &shown {
                Shown::Entry(_) if item.kind == Kind::Output => Group::Output,
                // (A worker's report or message runs with the calls.)
                Shown::Entry(_) if self.status_row(item, filter).is_some() => Group::Activity,
                Shown::Entry(_) => group(item),
                _ => Group::Activity,
            };
            // A command's output that comes in lines reads as one block, and
            // a call's content belongs to its call above it.
            let output = matches!(&shown, Shown::Entry(_)) && item.kind == Kind::Output;
            let content = matches!(&shown, Shown::Entry(_))
                && item.kind == Kind::Result
                && item.call.is_some_and(|call| {
                    blocks.last().is_some_and(|block| {
                        block.key == call
                            || matches!(block.shown, Shown::Entry(at)
                                if self.entries[at].item.call == Some(call))
                    })
                });
            let (lines, open) = match &shown {
                Shown::Entry(index) => {
                    let folds = self.folds_in(&self.entries[*index].item, filter);
                    let status = self.status_row(&self.entries[*index].item, filter);
                    let entry = &mut self.entries[*index];
                    // Only a click opens thinking in the quiet view.
                    let quiet = (quiet || opens) && entry.item.kind == Kind::Thinking;
                    let open = if quiet { entry.open_quiet } else { entry.open };
                    let clock = running_time(&entry.item, now);
                    let fresh = entry.cache.as_ref().is_some_and(|c| {
                        c.width == width
                            && c.style == style
                            && c.clock == clock
                            && c.open == open
                            && c.folds == folds
                            && c.status == status
                    });
                    if !fresh {
                        entry.cache = Some(Cached {
                            width,
                            style,
                            lines: entry_lines(
                                &entry.item,
                                theme,
                                width,
                                own,
                                Look {
                                    open,
                                    clock: clock.as_deref(),
                                    folds,
                                    status,
                                },
                            ),
                            clock,
                            open,
                            folds,
                            status,
                        });
                    }
                    let lines = entry.cache.as_ref().map_or(0, |c| c.lines.len());
                    // A call opens to what it holds, a content is what it
                    // opened to, and a thinking or a worker's row is open
                    // when it shows more than its line.
                    let open = match entry.item.kind {
                        Kind::Call if status.is_none() => folds && open,
                        Kind::Result => true,
                        _ => lines > 1,
                    };
                    (lines, open)
                }
                Shown::Quiet(row) => {
                    let key = self.entries[row.members[0]].item.id;
                    let open = self.quiet_open.contains(&key);
                    let clock = self.quiet_clock(row, now);
                    let members: Vec<(u64, u64)> = row
                        .members
                        .iter()
                        .map(|member| {
                            let entry = &self.entries[*member];
                            (entry.item.id, entry.revision)
                        })
                        .collect();
                    let fresh = self.quiet_cache.get(&key).is_some_and(|c| {
                        c.words == row.words
                            && c.members == members
                            && c.open == open
                            && c.width == width
                            && c.style == style
                            && c.clock == clock
                    });
                    if !fresh {
                        let (lines, spans) = self.quiet_lines(row, theme, width, now);
                        self.quiet_cache.insert(
                            key,
                            QuietCached {
                                words: row.words.clone(),
                                members,
                                open,
                                width,
                                style,
                                clock,
                                lines,
                                spans,
                            },
                        );
                    }
                    let lines = self.quiet_cache.get(&key).map_or(0, |c| c.lines.len());
                    (lines, lines > 1)
                }
                _ => (1, false),
            };
            let gap = match last {
                None => false,
                _ if content => false,
                Some((Group::Output, _)) if output => false,
                Some((last, was_open)) => {
                    was_open || open || !(last == Group::Activity && group == Group::Activity)
                }
            };
            last = Some((group, open));
            blocks.push(Block {
                key: self.entries[index].item.id,
                shown,
                gap,
                lines,
                tail: false,
                open,
            });
        }
        if quiet || !opened.is_empty() {
            let keys: HashSet<u64> = blocks.iter().map(|block| block.key).collect();
            self.quiet_cache.retain(|key, _| keys.contains(key));
        }
        if self.motion.working {
            // Where the agent's output comes next, above the notes it hasn't
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
            // is a paragraph of its own, a blank line on each side. So it is
            // before the agent's first call of the turn, under the calls of
            // its workers or the card of one that started from the queue,
            // as it is under the user's message. Anything open above sets it
            // apart as text does. The notes it stands above (they wait at
            // the end) set it apart below with their own blank line, so it has
            // one above too.
            let notes_below = at < blocks.len();
            let among_calls = at
                .checked_sub(1)
                .is_some_and(|last| self.is_call_run(&blocks[last]))
                && self
                    .under_way(filter)
                    .is_some_and(|(agent, since)| self.calls_begun(agent, since));
            let after_open = at.checked_sub(1).is_some_and(|last| blocks[last].open);
            let apart = at > 0 && (notes_below || !among_calls || after_open);
            blocks.insert(
                at,
                Block {
                    key: WORKING_KEY,
                    shown: Shown::Working,
                    gap: apart,
                    lines: 1,
                    tail: apart && !notes_below,
                    open: false,
                },
            );
        }
        // The notes the agent hasn't read wait at the end, with a blank line
        // below them, before the status line, as there is above them.
        if let Some(last) = blocks.last_mut()
            && matches!(last.shown, Shown::Entry(index) if self.entries[index].item.pending)
        {
            last.tail = true;
        }
        blocks
    }

    /// Whether `item` is a call's content whose call is closed in the Full
    /// view.
    fn closed_content(&self, item: &Item) -> bool {
        item.kind == Kind::Result
            && item
                .call
                .and_then(|call| self.index(call))
                .is_some_and(|call| !self.entries[call].open)
    }

    /// Whether `block` is part of the agent's run of calls, namely a call, its
    /// result, its thinking, a worker's line or a turn's count of calls.
    fn is_call_run(&self, block: &Block) -> bool {
        match &block.shown {
            Shown::Entry(index) => matches!(
                self.entries[*index].item.kind,
                Kind::Call | Kind::Result | Kind::Thinking
            ),
            Shown::Worker(_) | Shown::Counter { .. } | Shown::Quiet(_) => true,
            _ => false,
        }
    }

    /// What the Quiet view shows, in order, with each run of the agent's calls
    /// as rows ([`quiet::rows`]), and the rest as it is. A call's content
    /// goes in its row. In the Messages view, the entries of `opened` (of
    /// the turns whose count is open) show so, and the rest as Messages
    /// shows them, with each turn's first call of `counts`, where its
    /// count goes.
    fn quiet_placed(
        &self,
        filter: &Filter,
        counts: &HashMap<Option<u64>, Counted>,
        opened: &HashSet<usize>,
    ) -> Vec<Placed> {
        let messages = filter.view() == View::Messages;
        let as_quiet = filter.in_view(View::Quiet);
        // The entries shown, each with the results of its own, and whether
        // it stands as it is.
        let mut shown: Vec<(usize, Vec<usize>, bool)> = Vec::new();
        for index in self.order() {
            let item = &self.entries[index].item;
            let opens = opened.contains(&index);
            let first = counts
                .get(&item.turn)
                .is_some_and(|count| count.from == item.id);
            let hidden = matches!(
                self.shows(if opens { &as_quiet } else { filter }, item),
                Show::Hidden
            );
            if hidden && !(messages && first) {
                continue;
            }
            // (A result comes right under its call.)
            if item.kind == Kind::Result
                && let Some((call, results, _)) = shown.last_mut()
                && item.call == Some(self.entries[*call].item.id)
            {
                results.push(index);
                continue;
            }
            shown.push((index, Vec::new(), messages && (!opens || hidden)));
        }
        let steps: Vec<quiet::Step> = shown
            .iter()
            .map(|(index, results, as_is)| {
                let item = &self.entries[*index].item;
                match item.kind {
                    // (A note the agent hasn't read waits at the end.)
                    _ if item.pending => quiet::Step::Aside,
                    _ if *as_is => quiet::Step::Other(item.id),
                    Kind::Call if filter.owns(item) => quiet::Step::Call(quiet::Call {
                        id: item.id,
                        act: item.act.as_ref(),
                        status: item.status,
                        outcome: item.outcome.as_deref(),
                        job: item.job.is_some(),
                        turn: item.turn,
                        content: !results.is_empty(),
                        ended: item.ended,
                        sealed: self.quiet_sealed.contains(&item.id),
                    }),
                    // (One the reader opened on its own line stays there.)
                    Kind::Thinking if self.quiet_sealed.contains(&item.id) => {
                        quiet::Step::Other(item.id)
                    }
                    Kind::Thinking => quiet::Step::Thinking,
                    _ => quiet::Step::Other(item.id),
                }
            })
            .collect();
        let mut placed = Vec::with_capacity(shown.len());
        let pieces = quiet::rows(&steps);
        for piece in pieces {
            match piece {
                quiet::Piece::Step(at) => {
                    let (index, results, _) = &shown[at];
                    placed.push(Placed::Entry(*index));
                    placed.extend(results.iter().copied().map(Placed::Entry));
                }
                quiet::Piece::Row(row) => {
                    let mut members = Vec::new();
                    for at in &row.members {
                        let (index, results, _) = &shown[*at];
                        members.push(*index);
                        members.extend(results.iter().copied());
                    }
                    placed.push(Placed::Quiet(Arc::new(QuietRow {
                        members,
                        words: row.words,
                        merged: row.merged,
                        opens: row.opens,
                    })));
                }
            }
        }
        placed
    }

    /// The turns' counts of calls in `filter`'s view when it's Messages, by
    /// turn, and the entries that show as the Quiet view shows them because
    /// their turn's count is open. A count's line stands at the turn's first
    /// call, or at the thinking that led to it with nothing Messages shows
    /// between them, and open it shows that and what came after it in the
    /// turn, as Quiet does.
    fn counted(&self, filter: &Filter) -> (HashMap<Option<u64>, Counted>, HashSet<usize>) {
        let mut counts: HashMap<Option<u64>, Counted> = HashMap::new();
        let mut opened = HashSet::new();
        if filter.view() != View::Messages {
            return (counts, opened);
        }
        let order = self.order();
        // The thinking since the last thing Messages showed.
        let mut thought = None;
        for index in &order {
            let item = &self.entries[*index].item;
            match self.shows(filter, item) {
                Show::Count => {}
                Show::Hidden if filter.owns(item) && item.kind == Kind::Thinking => {
                    thought.get_or_insert(item.id);
                    continue;
                }
                Show::Hidden => continue,
                Show::Item | Show::Worker(_) => {
                    thought = None;
                    continue;
                }
            }
            let from = thought.take().unwrap_or(item.id);
            let count = counts.entry(item.turn).or_insert_with(|| Counted {
                first: item.id,
                from,
                open: self.counts_open.contains(&item.id),
                ..Counted::default()
            });
            count.calls += 1;
            count.failed += usize::from(item.status == Some(CallStatus::Failed));
            count.running += usize::from(item.status == Some(CallStatus::Running));
        }
        for index in order {
            let item = &self.entries[index].item;
            let open = counts
                .get(&item.turn)
                .is_some_and(|count| count.open && item.id >= count.from);
            if filter.owns(item) && open {
                opened.insert(index);
            }
        }
        (counts, opened)
    }

    /// Whether `filter`'s view shows the item `id` as the Quiet view does,
    /// as Messages does where its turn's count is open.
    fn shown_quiet(&self, id: u64, filter: &Filter) -> bool {
        match filter.view() {
            View::Quiet => true,
            View::Messages => self
                .index(id)
                .is_some_and(|at| self.counted(filter).1.contains(&at)),
            View::Full => false,
        }
    }

    /// Opens what the renderings show open (`remote::scenario`). In Quiet
    /// that's every row, in a row of several its first call with something
    /// to show and every call that failed, and a worker's reports and
    /// messages. In Full it's the first read and the first edit, besides
    /// the commands. In both, every thinking is open. In Messages it's each
    /// turn's count.
    #[cfg(test)]
    pub(crate) fn open_for_renderings(&mut self, view: View) {
        if view != View::Messages {
            for entry in &mut self.entries {
                if entry.item.kind == Kind::Thinking {
                    entry.open = true;
                    entry.open_quiet = true;
                    entry.touch();
                }
            }
        }
        if view == View::Messages {
            let (counts, _) = self.counted(&Filter::Main(view));
            self.counts_open
                .extend(counts.values().map(|count| count.first));
            self.generation += 1;
            return;
        }
        if view == View::Quiet {
            let placed = self.quiet_placed(&Filter::Main(view), &HashMap::new(), &HashSet::new());
            for row in placed.iter().filter_map(|placed| match placed {
                Placed::Quiet(row) => Some(row),
                Placed::Entry(_) => None,
            }) {
                let key = self.entries[row.members[0]].item.id;
                self.quiet_open.insert(key);
                if !row.merged {
                    self.quiet_sealed.insert(key);
                    continue;
                }
                let mut first = true;
                for member in &row.members {
                    let item = &self.entries[*member].item;
                    if item.kind != Kind::Call || !self.folds_inside(item) {
                        continue;
                    }
                    let failed = item.status == Some(CallStatus::Failed);
                    if first || failed {
                        self.entries[*member].open_quiet = true;
                        self.entries[*member].touch();
                    }
                    first = false;
                }
            }
            let filter = Filter::Main(view);
            let rows: Vec<usize> = (0..self.entries.len())
                .filter(|at| self.status_row(&self.entries[*at].item, &filter).is_some())
                .collect();
            for at in rows {
                self.entries[at].open = true;
                self.entries[at].touch();
            }
            return;
        }
        for kind in [ActKind::Read, ActKind::Edit] {
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| entry.item.act.as_ref().is_some_and(|act| act.kind == kind))
            {
                entry.open = true;
                entry.touch();
            }
        }
    }

    /// Opens every fold, including a worker's rows, for step renderings.
    #[cfg(test)]
    pub(crate) fn open_all_for_renderings(&mut self, view: View, worker: Option<&str>) {
        self.open_for_renderings(view);
        for entry in &mut self.entries {
            entry.open = true;
            entry.open_quiet = true;
            entry.touch();
        }
        let filter = match worker {
            Some(worker) => Filter::Worker(worker.to_string(), view),
            None => Filter::Main(view),
        };
        let placed = self.quiet_placed(&filter, &HashMap::new(), &HashSet::new());
        for row in placed {
            if let Placed::Quiet(row) = row {
                self.quiet_open.insert(self.entries[row.members[0]].item.id);
            }
        }
        self.generation += 1;
    }

    /// How long the call of a Quiet row that runs has run.
    fn quiet_clock(&self, row: &QuietRow, now: i64) -> Option<String> {
        if !row.words.running {
            return None;
        }
        row.members
            .iter()
            .map(|member| &self.entries[*member].item)
            .filter(|item| item.status == Some(CallStatus::Running))
            .find_map(|item| running_time(item, now))
    }

    /// Whether a Quiet row opens at `width`, because it has more to show
    /// than its words or its line cuts them.
    fn quiet_opens(row: &QuietRow, width: usize) -> bool {
        row.opens || quiet_cut(&row.words, width)
    }

    /// The lines of a Quiet row, which are its words, and when it's open what
    /// its calls hold behind a rail under them. A row of one call shows the
    /// call's detail and content, after the call as Full writes it where the
    /// row's line cut its words. A row of several shows each call's Full
    /// line, which opens to the call's detail and content under it, and the
    /// thinking between them. On the rail, anything open stands apart from
    /// its neighbours by a blank line.
    fn quiet_lines(
        &self,
        row: &QuietRow,
        theme: &Theme,
        width: usize,
        now: i64,
    ) -> (Vec<Wrapped>, Vec<(u64, std::ops::Range<usize>)>) {
        let open = self
            .quiet_open
            .contains(&self.entries[row.members[0]].item.id);
        let clock = self.quiet_clock(row, now);
        let fold = Self::quiet_opens(row, width).then_some(open);
        let mut lines = quiet_line(&row.words, fold, theme, width, clock.as_deref());
        let mut spans = Vec::new();
        if fold != Some(true) {
            return (lines, spans);
        }
        let bar = || Span::styled(glyphs::render("│ ").into_owned(), theme.dim());
        let inner = width.saturating_sub(MARGIN).max(1);
        let rail = |lines: &mut Vec<Wrapped>, rows: Vec<Wrapped>, indent: usize| {
            lines.extend(
                indented(rows, indent)
                    .into_iter()
                    .map(|line| line.decorated(bar())),
            );
        };
        if !row.merged {
            for member in &row.members {
                let entry = &self.entries[*member];
                let item = &entry.item;
                match item.kind {
                    Kind::Call => {
                        let detail = item.act.as_ref().map_or("", |act| act.detail.as_str());
                        // (Its detail says the whole of what the words cut.)
                        if detail.is_empty() && quiet_cut(&row.words, width) {
                            rail(&mut lines, detail_lines(&call_words(item), theme, inner), 0);
                        }
                        if !detail.is_empty() {
                            rail(&mut lines, detail_lines(detail, theme, inner), 0);
                        }
                    }
                    Kind::Result => {
                        let start = lines.len();
                        rail(&mut lines, content_lines(item, theme, inner, entry.open), 0);
                        spans.push((item.id, start..lines.len()));
                    }
                    _ => {}
                }
            }
            return (lines, spans);
        }
        // Whether the item before on the rail is open, none before the first.
        let mut before: Option<bool> = None;
        let mut gap = |lines: &mut Vec<Wrapped>, open: bool| {
            if before.is_some_and(|before| before || open) {
                lines.push(Wrapped::line(Vec::new()).decorated(bar()));
            }
            before = Some(open);
        };
        // (Inner text stands two columns in from the rail.)
        let room = inner.saturating_sub(MARGIN).max(1);
        let mut members = row.members.iter().peekable();
        while let Some(member) = members.next() {
            let entry = &self.entries[*member];
            let item = &entry.item;
            // A call's contents follow it.
            let mut results = Vec::new();
            while let Some(next) =
                members.next_if(|next| self.entries[**next].item.kind == Kind::Result)
            {
                results.push(*next);
            }
            match item.kind {
                Kind::Call => {
                    let folds = self.folds_inside(item);
                    let open = folds && entry.open_quiet;
                    gap(&mut lines, open);
                    let start = lines.len();
                    let line =
                        call_line(item, theme, inner, true, None, folds.then_some(open), true);
                    rail(&mut lines, line, 0);
                    if open {
                        if call_cut(item, inner, true) {
                            rail(
                                &mut lines,
                                detail_lines(&call_words(item), theme, room),
                                MARGIN,
                            );
                        }
                        if let Some(detail) = &item.detail {
                            rail(&mut lines, detail_lines(detail, theme, room), MARGIN);
                        }
                    }
                    spans.push((item.id, start..lines.len()));
                    for result in results.iter().filter(|_| open) {
                        let entry = &self.entries[*result];
                        let start = lines.len();
                        let content = content_lines(&entry.item, theme, room, entry.open);
                        rail(&mut lines, content, MARGIN);
                        spans.push((entry.item.id, start..lines.len()));
                    }
                }
                Kind::Thinking => {
                    let open = entry.open_quiet && thought_has_more(&item.text, inner);
                    gap(&mut lines, open);
                    let start = lines.len();
                    rail(
                        &mut lines,
                        thought_lines(&item.text, theme, room, open, None),
                        MARGIN,
                    );
                    spans.push((item.id, start..lines.len()));
                }
                _ => {}
            }
        }
        (lines, spans)
    }

    /// The line of `block` the keyboard is on, if it's there. That is a
    /// block's first line, or inside an open Quiet row the line that opens
    /// and closes a call, a thinking or a content ([`Self::inner_line`]).
    fn focus_line(&self, block: &Block) -> Option<usize> {
        match self.focus? {
            Target::Block(key) if key == block.key => Some(usize::from(block.gap)),
            Target::Inner { row, item } if row == block.key => self.inner_line(block, item),
            _ => None,
        }
    }

    /// The line of `block`, an open Quiet row, that opens and closes the
    /// call, the content or the thinking `id` inside it (the first line of
    /// a call or a thinking, the last of a content).
    fn inner_line(&self, block: &Block, id: u64) -> Option<usize> {
        let Shown::Quiet(_) = block.shown else {
            return None;
        };
        let cached = self.quiet_cache.get(&block.key)?;
        let call = self
            .item(id)
            .is_some_and(|item| matches!(item.kind, Kind::Call | Kind::Thinking));
        cached
            .spans
            .iter()
            .find(|(inner, _)| *inner == id)
            .map(|(_, lines)| {
                usize::from(block.gap) + if call { lines.start } else { lines.end - 1 }
            })
    }

    /// The entries in the order they are drawn, by ID, except that a
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
        // The lines of text. "Working…" comes and goes without being new.
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
        // A fold opened or closed. The line clicked stays under the mouse.
        if let Some(pin) = self.pin.take()
            && let Some(index) = blocks.iter().position(|block| block.key == pin.key)
        {
            top = (starts[index] + pin.line).saturating_sub(pin.row);
        }
        if self.pending_scroll != 0 {
            top = (top as i64 + self.pending_scroll).clamp(0, bottom as i64) as usize;
            self.pending_scroll = 0;
        }
        // The line the keyboard moved to comes into sight.
        if std::mem::take(&mut self.reveal)
            && let Some((index, line)) = blocks
                .iter()
                .enumerate()
                .find_map(|(index, block)| self.focus_line(block).map(|line| (index, line)))
        {
            let at = starts[index] + line;
            if at < top {
                top = at;
            } else if at >= top + height {
                top = at + 1 - height;
            }
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
            // At the bottom, following again (unless a drag holds it).
            _ if top >= bottom && !self.hold => self.anchor = None,
            Some(here) => {
                // Following stopped (or the view changed). What comes from
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
            let focused = self.focus_line(block);
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
                // The line the keyboard is on.
                if focused == Some(line) {
                    buffer.set_style(Rect::new(area.x, row, area.width, 1), highlight);
                }
                if let Some((from, to)) =
                    range.and_then(|range| selected_columns(range, (index, line), wrapped))
                {
                    for x in from..to.min(width) {
                        buffer[(area.x + x as u16, row)].set_style(highlight);
                    }
                }
                let gap = block.gap && line == 0;
                // The thinking that streams, the live line, its spinner in
                // the markers' column on its first line.
                if let Shown::Entry(entry) = block.shown
                    && self.motion.thinking == Some(self.entries[entry].item.id)
                    && line == usize::from(block.gap)
                {
                    let x = area.x + SPINNER as u16;
                    motion::paint(buffer, (x, row), 0, theme, ms);
                    moving = true;
                }
                if block.shown == Shown::Working {
                    // (Not text, so the mouse finds the line above it.)
                    if line == usize::from(block.gap) {
                        let label = text_width(&glyphs::render(self.working_label()));
                        let x = area.x + SPINNER as u16;
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

        // The marker of what is new below, the lines that came since.
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
            // (Not text, so a copy leaves it out.)
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
            // (As `blocks` drew them just before.)
            Shown::Quiet(row) => match self.quiet_cache.get(&block.key) {
                Some(cached) if cached.words == row.words => cached.lines.clone(),
                _ => {
                    self.quiet_lines(row, theme, width, chrono::Utc::now().timestamp_millis())
                        .0
                }
            },
            Shown::Worker(id) => vec![margin(worker_line(
                self.worker(id),
                id,
                &self.usual,
                theme,
                width,
            ))],
            Shown::Heading(id) => self
                .worker(id)
                .map(|worker| heading_lines(worker, theme, width))
                .unwrap_or_default(),
            Shown::Counter {
                calls,
                failed,
                running,
                open,
            } => {
                let mut spans = vec![
                    marker(if *open { "▾" } else { "▸" }, theme.dim()),
                    Span::styled(
                        format!("{calls} tool call{}", if *calls == 1 { "" } else { "s" }),
                        theme.text(),
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
            // (The spinner goes in the markers' column as it is drawn.)
            Shown::Working => {
                let label = glyphs::render(self.working_label()).into_owned();
                // The spinner stands in the markers' column and its words
                // where the text of every row starts, as a thinking that
                // takes its place does.
                let label = cut_end(&label, width.saturating_sub(MARGIN).max(1));
                let mut line = Wrapped::line(vec![
                    Span::raw(" ".repeat(MARGIN)),
                    Span::styled(label, theme.dim()),
                ]);
                line.bare = true;
                vec![line]
            }
        }
    }

    /// What the "Working…" line says.
    fn working_label(&self) -> &str {
        self.motion.label.as_deref().unwrap_or(WORKING)
    }

    // ----- selecting text with the mouse -----

    /// The place in the text at the screen's `x`, `y`, as last drawn. Above
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

    /// Holds the view still while a drag selects text. New output doesn't
    /// move the text under the mouse (it comes below, counted by the
    /// marker), as when the reader scrolls up.
    pub fn hold(&mut self) {
        if self.anchor.is_none() {
            self.anchor = self.drawn.top;
        }
        self.hold = true;
    }

    /// The drag ended. At the bottom, the view follows again.
    pub fn release(&mut self) {
        self.hold = false;
    }

    /// The fold at the screen's `x`, `y`, namely a Quiet row's line, a call's
    /// line, a content's door, any line of an agent's thinking, and the same
    /// lines of those inside an open Quiet row.
    pub fn fold_at(&mut self, x: u16, y: u16, filter: &Filter, theme: &Theme) -> Option<Fold> {
        let row = self.drawn.row_at(x, y)?;
        let width = self.drawn.area.width as usize;
        let blocks = self.blocks(filter, theme, width);
        let block = blocks.iter().find(|block| block.key == row.key)?;
        let found = match &block.shown {
            Shown::Quiet(quiet) if row.line == usize::from(block.gap) => {
                Self::quiet_opens(quiet, self.width).then_some(row.key)
            }
            Shown::Quiet(_) => self.quiet_cache.get(&row.key).and_then(|cached| {
                let gap = usize::from(block.gap);
                cached
                    .spans
                    .iter()
                    .find(|(id, lines)| {
                        let thinking = self
                            .item(*id)
                            .is_some_and(|item| item.kind == Kind::Thinking);
                        if thinking {
                            row.line
                                .checked_sub(gap)
                                .is_some_and(|at| lines.contains(&at))
                        } else {
                            self.inner_line(block, *id) == Some(row.line)
                        }
                    })
                    .map(|(id, _)| *id)
                    .filter(|id| self.item(*id).is_some_and(|item| self.folds_inside(item)))
            }),
            Shown::Entry(index) if self.folds_in(&self.entries[*index].item, filter) => {
                let thinking = self.entries[*index].item.kind == Kind::Thinking;
                let line = self.fold_line(block, *index, filter, theme, width);
                (thinking || row.line == line).then_some(row.key)
            }
            _ => None,
        };
        found.map(|item| Fold {
            item,
            block: row.key,
            line: row.line,
        })
    }

    /// Where the fold of `item` is in `filter`'s view, or with `inner` its
    /// fold inside an open Quiet row (the first call of a row of several has
    /// the row's key).
    fn fold(&mut self, item: u64, inner: bool, filter: &Filter, theme: &Theme) -> Option<Fold> {
        let width = self.drawn.area.width as usize;
        let blocks = self.blocks(filter, theme, width);
        for block in &blocks {
            let line = if block.key == item && !inner {
                match &block.shown {
                    Shown::Quiet(quiet) if Self::quiet_opens(quiet, self.width) => {
                        Some(usize::from(block.gap))
                    }
                    Shown::Entry(index) if self.folds_in(&self.entries[*index].item, filter) => {
                        Some(self.fold_line(block, *index, filter, theme, width))
                    }
                    _ => return None,
                }
            } else {
                self.inner_line(block, item)
                    .filter(|_| self.item(item).is_some_and(|item| self.folds_inside(item)))
            };
            if let Some(line) = line {
                return Some(Fold {
                    item,
                    block: block.key,
                    line,
                });
            }
        }
        None
    }

    /// The line of the entry `index`'s block in `filter`'s view that opens
    /// and closes it, the first of a call, a thinking or a worker's row,
    /// else its last.
    fn fold_line(
        &self,
        block: &Block,
        index: usize,
        filter: &Filter,
        theme: &Theme,
        width: usize,
    ) -> usize {
        let item = &self.entries[index].item;
        if matches!(item.kind, Kind::Call | Kind::Thinking)
            || self.status_row(item, filter).is_some()
        {
            usize::from(block.gap)
        } else {
            self.block_rows(block, theme, width).len().saturating_sub(1)
        }
    }

    /// Opens or closes the fold of the item `item`, its line held where it
    /// is drawn, or else on the screen's row `y`, at the next draw. Returns
    /// where that line is.
    pub fn toggle(&mut self, item: u64, y: u16, filter: &Filter, theme: &Theme) -> Option<Fold> {
        // A Quiet row, unless the line is inside it, on its first call.
        let gap = self.quiet_row(item, filter, theme);
        let row = gap.is_some_and(|gap| {
            let drawn = (y as usize)
                .checked_sub(self.drawn.area.y as usize)
                .and_then(|at| self.drawn.rows.get(at).copied().flatten());
            drawn.is_none_or(|at| at.key != item || at.line == usize::from(gap))
        });
        let inner = gap.is_some() && !row;
        // (A click on a thinking's text folds it, and its label stays put.)
        let shown_at = self.fold(item, inner, filter, theme).and_then(|fold| {
            self.drawn
                .rows
                .iter()
                .position(|at| at.is_some_and(|at| (at.key, at.line) == (fold.block, fold.line)))
        });
        if row {
            if !self.quiet_open.remove(&item) {
                self.quiet_open.insert(item);
                self.seal(item, filter, theme);
            }
        } else {
            let quiet = self.shown_quiet(item, filter);
            let alone = self.shown_alone(item, filter, theme);
            let folds = self.item(item).is_some_and(|item| match alone {
                true => self.folds_in(item, filter),
                false => self.folds_inside(item),
            });
            if !folds {
                return None;
            }
            let owned = filter.owns(self.item(item)?);
            let entry = self.find_mut(item)?;
            // A thinking and a call of a Quiet row open in the Quiet view
            // apart from the Full view.
            let mut sealed = false;
            if quiet && owned && matches!(entry.item.kind, Kind::Thinking | Kind::Call) {
                entry.open_quiet = !entry.open_quiet;
                sealed = alone && entry.open_quiet && entry.item.kind == Kind::Thinking;
            } else {
                entry.open = !entry.open;
            }
            entry.touch();
            // A thinking the reader opened on a line of its own stays there.
            if sealed && self.quiet_sealed.insert(item) {
                self.generation += 1;
            }
        }
        let fold = self.fold(item, inner, filter, theme)?;
        self.pin = Some(Pin {
            key: fold.block,
            line: fold.line,
            row: shown_at.unwrap_or(y.saturating_sub(self.drawn.area.y) as usize),
        });
        Some(fold)
    }

    /// Whether the item `id` shows as a block of its own in `filter`'s view,
    /// not inside a Quiet row.
    fn shown_alone(&mut self, id: u64, filter: &Filter, theme: &Theme) -> bool {
        let width = self.drawn.area.width as usize;
        self.blocks(filter, theme, width)
            .iter()
            .any(|block| block.key == id && matches!(block.shown, Shown::Entry(_)))
    }

    /// The Quiet row `key` the reader opened keeps its form, when it holds
    /// one call, so later calls start rows of their own.
    fn seal(&mut self, key: u64, filter: &Filter, theme: &Theme) {
        let width = self.drawn.area.width as usize;
        let alone = self.blocks(filter, theme, width).iter().any(|block| {
            block.key == key && matches!(&block.shown, Shown::Quiet(row) if !row.merged)
        });
        if alone && self.quiet_sealed.insert(key) {
            self.generation += 1;
        }
    }

    /// Whether the block `key` is a Quiet row of `filter`'s view that opens,
    /// and if so whether a blank line comes before its line.
    fn quiet_row(&mut self, key: u64, filter: &Filter, theme: &Theme) -> Option<bool> {
        let width = self.drawn.area.width as usize;
        self.blocks(filter, theme, width)
            .into_iter()
            .find(|block| {
                block.key == key && matches!(&block.shown, Shown::Quiet(row) if Self::quiet_opens(row, self.width))
            })
            .map(|block| block.gap)
    }

    /// The blocks of `filter`'s view that open and close, by key. Those are Quiet
    /// rows, calls, contents and the agents' thinking.
    fn openable(&mut self, filter: &Filter, theme: &Theme) -> Vec<Target> {
        let width = self.drawn.area.width as usize;
        let mut keys = Vec::new();
        for block in self.blocks(filter, theme, width) {
            match &block.shown {
                Shown::Quiet(row) if !Self::quiet_opens(row, self.width) => {}
                Shown::Quiet(_) => {
                    keys.push(Target::Block(block.key));
                    // The calls, contents and thinking inside it, while it's
                    // open.
                    if let Some(cached) = self.quiet_cache.get(&block.key) {
                        keys.extend(
                            cached
                                .spans
                                .iter()
                                .map(|(id, _)| *id)
                                .filter(|id| {
                                    self.item(*id).is_some_and(|item| self.folds_inside(item))
                                })
                                .map(|item| Target::Inner {
                                    row: block.key,
                                    item,
                                }),
                        );
                    }
                }
                Shown::Entry(index) if self.folds_in(&self.entries[*index].item, filter) => {
                    keys.push(Target::Block(block.key))
                }
                Shown::Counter { .. } => keys.push(Target::Block(block.key)),
                _ => {}
            }
        }
        keys
    }

    /// Puts the keyboard on the last block that opens, or `by` blocks that
    /// open from the one it's on. False when there is none.
    pub fn move_focus(&mut self, by: isize, filter: &Filter, theme: &Theme) -> bool {
        let keys = self.openable(filter, theme);
        let Some(last) = keys.len().checked_sub(1) else {
            self.focus = None;
            return false;
        };
        let at = match self
            .focus
            .and_then(|focus| keys.iter().position(|key| *key == focus))
        {
            Some(at) => at.saturating_add_signed(by).min(last),
            None => last,
        };
        self.focus = Some(keys[at]);
        self.reveal = true;
        true
    }

    /// Opens (`Some(true)`), closes (`Some(false)`) or toggles (`None`) the
    /// block the keyboard is on.
    pub fn open_focus(&mut self, open: Option<bool>, filter: &Filter, theme: &Theme) {
        let Some(focus) = self.focus else {
            return;
        };
        self.reveal = true;
        let key = match focus {
            Target::Block(key) => key,
            // A call or a thinking inside a Quiet row opens there, a
            // content to its whole.
            Target::Inner { item, .. } => {
                if !self.item(item).is_some_and(|item| self.folds_inside(item)) {
                    return;
                }
                if let Some(entry) = self.find_mut(item) {
                    if matches!(entry.item.kind, Kind::Thinking | Kind::Call) {
                        entry.open_quiet = open.unwrap_or(!entry.open_quiet);
                    } else {
                        entry.open = open.unwrap_or(!entry.open);
                    }
                    entry.touch();
                }
                return;
            }
        };
        if is_count(key) {
            self.open_count(key, open);
            return;
        }
        if self.quiet_row(key, filter, theme).is_some() {
            let now = self.quiet_open.contains(&key);
            if open.unwrap_or(!now) {
                self.quiet_open.insert(key);
                self.seal(key, filter, theme);
            } else {
                self.quiet_open.remove(&key);
            }
            return;
        }
        let quiet = self.shown_quiet(key, filter);
        if !self
            .item(key)
            .is_some_and(|item| self.folds_in(item, filter))
        {
            return;
        }
        let owned = self.item(key).is_some_and(|item| filter.owns(item));
        let mut sealed = false;
        if let Some(entry) = self.find_mut(key) {
            if quiet && owned && matches!(entry.item.kind, Kind::Thinking | Kind::Call) {
                entry.open_quiet = open.unwrap_or(!entry.open_quiet);
                sealed = entry.open_quiet && entry.item.kind == Kind::Thinking;
            } else {
                entry.open = open.unwrap_or(!entry.open);
            }
            entry.touch();
        }
        // A thinking the reader opened on a line of its own stays there.
        if sealed && self.quiet_sealed.insert(key) {
            self.generation += 1;
        }
    }

    /// The lines of the block `key` in `filter`'s view.
    fn rows_of(&mut self, key: u64, filter: &Filter, theme: &Theme) -> Option<Vec<Wrapped>> {
        let width = self.drawn.area.width as usize;
        let blocks = self.blocks(filter, theme, width);
        let block = blocks.iter().find(|block| block.key == key)?;
        Some(self.block_rows(block, theme, width))
    }

    /// The word at `point`, which a double click selects.
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
    /// wrapped into), which a triple click selects.
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

    /// The highlighted text, as written, without the view's margins,
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
/// characters'), which a double click selects.
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
/// wrapped into, which a triple click selects.
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
/// (block, line)), as written, without the margins, gutters and indents,
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
/// that `range` covers, within its text, or `None` when it covers none.
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

/// What a view shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Filter {
    /// The main transcript, in a view.
    Main(View),
    /// A worker's own transcript, in a view.
    Worker(String, View),
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

impl View {
    /// What this view shows of an item of the agent whose transcript it
    /// is, the main agent's in the main transcript and a worker's in its
    /// own. One rule serves both, so a worker's view can't differ from the
    /// selected one.
    fn shows(self, item: &Item) -> Show {
        match item.kind {
            // The lines of the workers the agent started.
            Kind::WorkerLine => item.worker.clone().map_or(Show::Hidden, Show::Worker),
            // The status line shows how long the last turn ran, and the
            // context and cost as they change. A line after each turn would
            // repeat it.
            Kind::Turn => Show::Hidden,
            _ => match self {
                View::Full => Show::Item,
                // The line of a worker stands for the call that started
                // it. A call that is still running, was refused or queued
                // its worker shows.
                View::Quiet if item.kind == Kind::Call && item.started.is_some() => Show::Hidden,
                View::Quiet => Show::Item,
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
                    _ => Show::Hidden,
                },
            },
        }
    }
}

impl Filter {
    /// The same agent's transcript in `view`.
    fn in_view(&self, view: View) -> Filter {
        match self {
            Filter::Main(_) => Filter::Main(view),
            Filter::Worker(id, _) => Filter::Worker(id.clone(), view),
        }
    }

    /// The view whose rule applies to the agent's own items.
    fn view(&self) -> View {
        match self {
            Filter::Main(view) | Filter::Worker(_, view) => *view,
        }
    }

    /// The agent whose transcript it is, the main agent or a worker.
    fn agent(&self) -> &str {
        match self {
            Filter::Main(_) => MAIN_AGENT,
            Filter::Worker(id, _) => id,
        }
    }

    /// Whether `item` is of the agent whose transcript this is, the main
    /// agent's or the worker's.
    fn owns(&self, item: &Item) -> bool {
        match self {
            Filter::Main(_) => item.is_main(),
            Filter::Worker(id, _) => &item.agent == id,
        }
    }
}

/// What kind of block an item makes, for the spacing. Tool activity runs
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

/// A number that changes when the theme does, so the lines are drawn again.
fn theme_key(theme: &Theme) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    theme.name.hash(&mut hasher);
    format!("{:?}{:?}", theme.text, theme.dim).hash(&mut hasher);
    hasher.finish()
}

/// The left margin every line has, two columns, where the agent's text and
/// the text of every row with a marker start. The marker stands in it
/// ([`marker`]).
const MARGIN: usize = 2;

/// What a content's door says once its whole is no longer kept.
const NO_LONGER_KEPT: &str = "No longer kept";

/// The last line of a command's block that shows its whole output. A click
/// on it shows the first lines again.
const FOLD_CLOSE: &str = "↑ show the first lines only";

/// The line that says the agent works, while it waits on its model.
const WORKING: &str = "Working…";
/// The column of the spinner on the live line, the "Working…" line or a
/// thinking that streams, which is the markers' column.
const SPINNER: usize = 0;

/// The fewest columns a line keeps for its words when what follows them
/// (its outcome) is wide.
const WORDS_MIN: usize = 8;

/// How an item's lines look beyond the item itself.
#[derive(Clone, Copy, Default)]
struct Look<'a> {
    /// A command's block shows its whole output, or an agent's thinking
    /// shows whole.
    open: bool,
    /// How long a running call has run.
    clock: Option<&'a str>,
    /// A call has a detail or content, which its fold opens.
    folds: bool,
    /// It is a worker's row in its lead's view, a report or a message.
    status: Option<quiet::WorkerRow>,
}

/// A Quiet row's marker, ▸ (▾ while `fold` is open), or a dot for a row
/// without a fold.
fn fold_symbol(fold: Option<bool>) -> &'static str {
    match fold {
        Some(true) => "▾",
        Some(false) => "▸",
        None => "·",
    }
}

/// What follows a Quiet row's words, which its line keeps whole. That is
/// its news, its note, and while it runs how long it has run.
fn quiet_tail(words: &quiet::Words, clock: Option<&str>) -> String {
    let mut tail = String::new();
    if let Some(news) = &words.news {
        tail.push_str(&format!(" ({news})"));
    }
    if let Some(note) = &words.note {
        tail.push_str(&format!(" · {note}"));
    }
    if words.running {
        if let Some(clock) = clock {
            tail.push_str(&format!(" · {clock}"));
        }
        tail.push_str(" …");
    }
    rendered(tail)
}

/// The columns a Quiet row's words have on its line at `width`, past its
/// marker, what follows them and its ✗, which stands in the last column, two
/// from its words.
fn quiet_room(words: &quiet::Words, width: usize, clock: Option<&str>) -> usize {
    let cross = if words.failed { 3 } else { 0 };
    width
        .saturating_sub(MARGIN + cross + text_width(&quiet_tail(words, clock)))
        .max(WORDS_MIN)
}

/// Whether a Quiet row's line at `width` cuts its words, so the row opens to
/// them whole. (Not counting how long it has run, so a row doesn't come to
/// open as that grows.)
fn quiet_cut(words: &quiet::Words, width: usize) -> bool {
    text_width(&rendered(&words.text)) > quiet_room(words, width, None)
}

/// A Quiet row's line, one line at any width. Its marker (▸, ▾ while `fold`
/// is open, a dot for a row without a fold), its words cut to fit, then
/// what follows them whole, and ✗ at the right of a row that failed. (Its
/// calls are the agent's whose transcript it is, so it names no worker.)
fn quiet_line(
    words: &quiet::Words,
    fold: Option<bool>,
    theme: &Theme,
    width: usize,
    clock: Option<&str>,
) -> Vec<Wrapped> {
    let style = if words.muted {
        theme.dim()
    } else {
        theme.text()
    };
    let text = rendered(&words.text);
    let shown = cut_end(&text, quiet_room(words, width, clock));
    let cut = shown != text;
    let mut spans = vec![Span::styled(shown, style)];
    let tail = quiet_tail(words, clock);
    if !tail.is_empty() {
        spans.push(Span::styled(tail, theme.dim()));
    }
    let cross = if words.failed { 3 } else { 0 };
    let room = width.saturating_sub(MARGIN + cross).max(1);
    let mut line =
        Wrapped::line(fit_spans(spans, room)).decorated(marker(fold_symbol(fold), theme.dim()));
    if cut {
        line.whole = Some(rendered(words.said()));
    }
    let mut lines = vec![line];
    if words.failed {
        cross_at_right(&mut lines, theme, width);
    }
    lines
}

/// `spans` cut to `room` columns, with an ellipsis where they end early.
fn fit_spans(spans: Vec<Span<'static>>, room: usize) -> Vec<Span<'static>> {
    let mut line = Vec::new();
    let mut used = 0;
    for span in spans {
        let w = text_width(&span.content);
        if used + w <= room {
            used += w;
            line.push(span);
            continue;
        }
        let rest = room.saturating_sub(used);
        if rest > 0 {
            line.push(Span::styled(cut_end(&span.content, rest), span.style));
        }
        break;
    }
    line
}

/// The ✗ of a row that failed, in the last column of its first line.
fn cross_at_right(lines: &mut [Wrapped], theme: &Theme, width: usize) {
    if let Some(first) = lines.first_mut() {
        let used = spans_width(&first.spans);
        let cross = glyphs::render("✗").into_owned();
        let cross_width = text_width(&cross);
        let gap = width.saturating_sub(used + cross_width).max(1);
        // (Not the row's words, so a copy leaves it out.)
        first.spans.push(Span::raw(" ".repeat(gap)));
        first.spans.push(Span::styled(cross, theme.error()));
        first.tail += gap + cross_width;
    }
}

/// `lines` moved right by `indent` columns.
fn indented(lines: Vec<Wrapped>, indent: usize) -> Vec<Wrapped> {
    if indent == 0 {
        return lines;
    }
    lines
        .into_iter()
        .map(|line| line.decorated(Span::raw(" ".repeat(indent))))
        .collect()
}

/// A call's content (its result's block) at `width`, a file's numbers
/// muted, its code in the theme's syntax colours, a diff's removed and added
/// lines on the theme's error and success tints (or in those colours, where
/// the theme has no tints), and when its preview leaves some out the door
/// to the rest, muted, or while it's `open` the whole and the line that
/// closes it. Its numbers and marks are not copied with its text.
fn content_lines(item: &Item, theme: &Theme, width: usize, open: bool) -> Vec<Wrapped> {
    // (A result from an Arko before contents reads as muted text.)
    let kind = item.content.unwrap_or(ContentKind::Muted);
    let whole = item.whole.as_deref().filter(|_| open);
    let text = whole.unwrap_or(&item.text);
    let numbers = content::number_width(kind, text);
    // The highlighting of its numbered lines, in their order.
    let mut painted = code_spans(kind, text, item.language.as_deref(), theme).into_iter();
    let mut out = Vec::new();
    for line in text.split('\n') {
        let (lead, spans, tinted) = match content::line(kind, line) {
            content::Line::Numbered { number, mark, text } => {
                let spans = painted.next().unwrap_or_default();
                let tint = match mark {
                    content::Mark::Removed => Some(Tint::Removed),
                    content::Mark::Added => Some(Tint::Added),
                    _ => None,
                };
                let tinted = tint.and_then(|tint| theme.tint(tint));
                // (Shaded to read on the tint.)
                let on = |style: Style| tint.map_or(style, |tint| theme.on_tint(style, tint));
                let (lead, lead_style, base) = match mark {
                    content::Mark::None => {
                        (format!("{number:>numbers$}  "), theme.dim(), theme.text())
                    }
                    content::Mark::Kept => {
                        (format!("{number:>numbers$}   "), theme.dim(), theme.text())
                    }
                    content::Mark::Removed => (
                        format!("{number:>numbers$} - "),
                        on(theme.error()),
                        if tinted.is_some() {
                            on(theme.text())
                        } else {
                            theme.error()
                        },
                    ),
                    content::Mark::Added => (
                        format!("{number:>numbers$} + "),
                        on(theme.success()),
                        if tinted.is_some() {
                            on(theme.text())
                        } else {
                            theme.success()
                        },
                    ),
                };
                let spans =
                    painted_spans(text, &spans, base, |name| theme.syntax(name, tint).map(on));
                (Some(Span::styled(lead, lead_style)), spans, tinted)
            }
            content::Line::Plain(text) => {
                let style = match kind {
                    ContentKind::Plain => theme.text(),
                    _ => theme.dim(),
                };
                (None, painted_spans(text, &[], style, |_| None), None)
            }
        };
        let hang = lead.as_ref().map_or(0, |lead| text_width(&lead.content));
        let mut wrapped = wrap::wrap_lines(&spans, width.saturating_sub(hang).max(1), 0);
        // (A blank line keeps its row.)
        if wrapped.is_empty() {
            wrapped.push(Wrapped::line(Vec::new()));
        }
        let mut rows = match lead {
            Some(lead) => {
                let rest = Span::styled(" ".repeat(hang), tinted.unwrap_or_default());
                wrap::prefixed(wrapped, lead, rest)
            }
            None => wrapped,
        };
        // A tinted line's tint runs to its end.
        if let Some(tinted) = tinted {
            for row in &mut rows {
                let pad = width.saturating_sub(spans_width(&row.spans));
                row.spans.push(Span::styled(" ".repeat(pad), tinted));
                row.tail += pad;
            }
        }
        out.extend(rows);
    }
    // (The line that closes it isn't text, so a copy leaves it out.)
    if whole.is_some() {
        out.extend(
            plain(FOLD_CLOSE, theme.dim(), width)
                .into_iter()
                .map(|line| Wrapped { bare: true, ..line }),
        );
    } else if let Some(more) = &item.more {
        // (Its whole went, to keep the transcript's budget.)
        let more = if item.whole.is_some() {
            more.as_str()
        } else {
            NO_LONGER_KEPT
        };
        out.extend(plain(more, theme.dim(), width));
    }
    out
}

/// The highlighting of each numbered line of a content `text` of `kind`,
/// whose lines are of a file in `language`, where the theme colours code
/// ([`crate::remote::content_line_spans`]).
fn code_spans(
    kind: ContentKind,
    text: &str,
    language: Option<&str>,
    theme: &Theme,
) -> Vec<Vec<[u32; 3]>> {
    if !theme.colours_code() {
        return Vec::new();
    }
    #[cfg(feature = "remote")]
    return crate::remote::content_line_spans(kind, text, language).unwrap_or_default();
    #[cfg(not(feature = "remote"))]
    {
        let _ = (kind, text, language);
        Vec::new()
    }
}

/// `text` in `base`, but for its highlighted `spans` (`[start, end, name]`
/// in bytes), each in the style `style_of` gives its name, if any.
fn painted_spans(
    text: &str,
    spans: &[[u32; 3]],
    base: Style,
    style_of: impl Fn(usize) -> Option<Style>,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut push = |part: &str, style: Style| {
        if !part.is_empty() {
            out.push(Span::styled(wrap::clean(&glyphs::render(part)), style));
        }
    };
    let mut at = 0;
    for &[start, end, name] in spans {
        let (start, end) = (start as usize, end as usize);
        if start < at
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            continue;
        }
        push(&text[at..start], base);
        push(&text[start..end], style_of(name as usize).unwrap_or(base));
        at = end;
    }
    push(&text[at..], base);
    out
}

/// A call's detail, one muted line (wrapped) at `width`, which says what
/// its line cut.
fn detail_lines(detail: &str, theme: &Theme, width: usize) -> Vec<Wrapped> {
    plain(detail, theme.dim(), width)
}

/// `lines` behind the bar that hangs from the marker's column, the rail of
/// a call's content in the Full view.
fn railed(lines: Vec<Wrapped>, theme: &Theme) -> Vec<Wrapped> {
    let side = Span::styled(glyphs::render("│ ").into_owned(), theme.dim());
    lines
        .into_iter()
        .map(|line| line.decorated(side.clone()))
        .collect()
}

/// A row's marker (a call's fold triangle, the user's bar, ✉, ·) at the
/// screen's left edge, and the space up to the margin, where the row's text
/// starts. So the calls and the user's messages make a column of their own
/// beside what the agent says and thinks.
fn marker(symbol: &str, style: Style) -> Span<'static> {
    let symbol = glyphs::render(symbol).into_owned();
    let pad = MARGIN.saturating_sub(text_width(&symbol));
    Span::styled(format!("{symbol}{}", " ".repeat(pad)), style)
}

/// The first line of an agent's thinking, the first with text, without
/// Markdown's bold marks, and whether lines with text follow it.
fn thought_first(text: &str) -> (String, bool) {
    let text = wrap::clean(text);
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default().replace("**", "");
    (first, lines.next().is_some())
}

/// Whether a thinking shown closed at `width` leaves some of it out, so it
/// opens. One that fits on its line has nothing to open.
fn thought_has_more(text: &str, width: usize) -> bool {
    let (first, more) = thought_first(text);
    more || text_width(&rendered(&first)) > width.saturating_sub(MARGIN)
}

/// An agent's thinking at `width`, in the text column (a caller adds the
/// margin or the rail before it), dim and italic. Closed it is its first
/// line, which ends with … when it leaves some out. Open it is whole, its
/// paragraphs a blank line apart. `who` names a worker whose thinking shows
/// in another's view, on its first line.
fn thought_lines(
    text: &str,
    theme: &Theme,
    width: usize,
    open: bool,
    who: Option<Span<'static>>,
) -> Vec<Wrapped> {
    let style = theme.dim().add_modifier(Modifier::ITALIC);
    let width = width.max(1);
    let who_width = who.as_ref().map_or(0, |who| text_width(&who.content));
    let room = width.saturating_sub(who_width).max(1);
    let (first, more) = thought_first(text);
    let first = rendered(&first);
    let mut lines = if open && (more || text_width(&first) > room) {
        markdown::render(text, style, theme, room)
    } else {
        let mut shown = cut_end(&first, room);
        if more && shown == first && text_width(&shown) + 2 <= room {
            shown.push_str(&rendered(" …"));
        }
        let mut line = Wrapped::line(vec![Span::styled(shown.clone(), style)]);
        // (A copy of it, selected whole, takes it whole.)
        if shown != first {
            line.whole = Some(wrap::clean(text).trim().to_string());
        }
        vec![line]
    };
    if lines.is_empty() {
        lines.push(Wrapped::line(Vec::new()));
    }
    if let Some(who) = who {
        let rest = Span::raw(" ".repeat(who_width));
        lines = wrap::prefixed(lines, who, rest);
    }
    lines
}

/// A worker's report or message to its lead as its row in its lead's view
/// says it ("r1 reported", "r1 messaged the lead"), with how a message's
/// call stands, after ▸ (▾ while it's `open`) when it `folds`. Open, a
/// report shows its Markdown behind the bar. A message's content is its
/// result's block under it.
fn status_lines(
    item: &Item,
    row: quiet::WorkerRow,
    theme: &Theme,
    width: usize,
    look: Look,
) -> Vec<Wrapped> {
    let words = row.words(item.outcome.as_deref());
    let who = format!("{} ", item.agent);
    // (A failed row's ✗ stands in its last column, two from its words.)
    let cross = if words.failed { 3 } else { 0 };
    let room = width.saturating_sub(MARGIN + cross).max(1);
    let mut spans = vec![Span::styled(
        who.clone(),
        theme.text().add_modifier(Modifier::BOLD),
    )];
    let style = if words.muted {
        theme.dim()
    } else {
        theme.text()
    };
    spans.push(Span::styled(rendered(&words.text), style));
    let tail = quiet_tail(&words, look.clock);
    if !tail.is_empty() {
        spans.push(Span::styled(tail, theme.dim()));
    }
    let fold = look.folds.then_some(look.open);
    let line =
        Wrapped::line(fit_spans(spans, room)).decorated(marker(fold_symbol(fold), theme.dim()));
    let mut lines = vec![line];
    if words.failed {
        cross_at_right(&mut lines, theme, width);
    }
    if row == quiet::WorkerRow::Reported && look.folds && look.open {
        let body = markdown::render(
            &item.text,
            theme.text(),
            theme,
            width.saturating_sub(MARGIN).max(1),
        );
        lines.extend(railed(body, theme));
    }
    lines
}

/// A call's words as its Full line says them, the call as written, on one
/// line.
fn call_words(item: &Item) -> String {
    rendered(wrap::clean(&item.text).replace('\n', " "))
}

/// What follows a call's words on its Full line, which the line keeps
/// whole, ✓ or ✗ and its outcome, or while it runs how long it has run
/// (`clock`), unless its line is in a Quiet row (`in_row`), whose own line
/// shows that. Each part with whether it's the outcome's mark, ✓ or ✗.
fn call_tail(item: &Item, clock: Option<&str>, in_row: bool) -> Vec<(String, Option<CallStatus>)> {
    match item.status {
        Some(CallStatus::Ok) => {
            let mut parts = vec![
                (rendered(" · "), None),
                (glyphs::render("✓").into_owned(), Some(CallStatus::Ok)),
            ];
            if let Some(outcome) = &item.outcome {
                parts.push((rendered(format!(" {outcome}")), None));
            }
            parts
        }
        Some(CallStatus::Failed) => {
            let outcome = item.outcome.as_deref().unwrap_or("failed");
            vec![
                (rendered(" · "), None),
                (rendered(format!("✗ {outcome}")), Some(CallStatus::Failed)),
            ]
        }
        _ if in_row => Vec::new(),
        _ => {
            let mut parts = Vec::new();
            if let Some(clock) = clock {
                parts.push((rendered(format!(" · {clock}")), None));
            }
            parts.push((format!(" {}", glyphs::render("…")), None));
            parts
        }
    }
}

/// The columns a call's words have on its Full line at `width`, past the
/// marker, the worker's name when `who` shows it, and what follows them.
fn call_room(item: &Item, width: usize, who: bool, clock: Option<&str>, in_row: bool) -> usize {
    let who = if who { text_width(&item.agent) + 1 } else { 0 };
    let tail: usize = call_tail(item, clock, in_row)
        .iter()
        .map(|(part, _)| text_width(part))
        .sum();
    width.saturating_sub(MARGIN + who + tail).max(WORDS_MIN)
}

/// Whether a call's Full line at `width` cuts its words, so the call opens
/// to them whole. (Not counting how long it has run.)
fn call_cut(item: &Item, width: usize, in_row: bool) -> bool {
    text_width(&call_words(item)) > call_room(item, width, false, None, in_row)
}

/// A call's line as the Full view says it, the call as written and how it
/// went, one line at `width`, its words cut to fit and what follows them
/// whole, after its fold's triangle (`fold`, open or not) or, for a call
/// with nothing to show, a dot. `own` is set in its worker's own view, where
/// a worker's lines don't name it. Inside a Quiet row (`in_row`) the line
/// is muted, as the row's own line says what it does.
fn call_line(
    item: &Item,
    theme: &Theme,
    width: usize,
    own: bool,
    clock: Option<&str>,
    fold: Option<bool>,
    in_row: bool,
) -> Vec<Wrapped> {
    let who = !(item.is_main() || own);
    let mut spans = Vec::new();
    if who {
        spans.push(Span::styled(
            format!("{} ", item.agent),
            theme.text().add_modifier(Modifier::BOLD),
        ));
    }
    let failed = item.status == Some(CallStatus::Failed);
    let words = call_words(item);
    let shown = cut_end(&words, call_room(item, width, who, clock, in_row));
    let cut = shown != words;
    let style = if in_row { theme.dim() } else { theme.text() };
    spans.push(Span::styled(shown, style));
    for (part, mark) in call_tail(item, clock, in_row) {
        let style = match mark {
            Some(CallStatus::Ok) => theme.success(),
            Some(CallStatus::Failed) => theme.error(),
            _ => theme.dim(),
        };
        spans.push(Span::styled(part, style));
    }
    let marker_style = if failed { theme.error() } else { theme.dim() };
    let room = width.saturating_sub(MARGIN).max(1);
    let mut line =
        Wrapped::line(fit_spans(spans, room)).decorated(marker(fold_symbol(fold), marker_style));
    if cut {
        line.whole = Some(words);
    }
    vec![line]
}

/// An item's lines at `width` (margins included), with what a copy of each
/// needs. `own` is set in its worker's own view, where a worker's lines don't name it.
/// `look` is how they look.
fn entry_lines(item: &Item, theme: &Theme, width: usize, own: bool, look: Look) -> Vec<Wrapped> {
    let Look {
        open,
        clock,
        folds,
        status,
    } = look;
    // A worker's report or message to its lead, in its lead's view.
    if let Some(row) = status {
        return status_lines(item, row, theme, width, look);
    }
    // The agent's text starts at the margin, and so does the text of a row
    // with a marker, which stands at the left edge.
    let inner = width.saturating_sub(MARGIN).max(1);
    let blank = || Span::raw(" ".repeat(MARGIN));
    let mark = |symbol: &str, style: Style| marker(symbol, style);
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
            let mut lines = match item.text.is_empty() && !item.files.is_empty() {
                true => Vec::new(),
                false => plain(&item.text, style, inner),
            };
            // The files it carried, a dim line each.
            for sent in &item.files {
                lines.extend(plain(&sent.file.line(), theme.dim(), inner));
            }
            if let Some(tag) = user_tag(item) {
                // (Not the user's text, so a copy leaves it out.)
                let tag = cut_end(&tag, inner);
                let tag_width = text_width(&tag);
                let style = theme.dim().add_modifier(Modifier::ITALIC);
                // At the right of the first row, two columns from its text,
                // where the message doesn't wrap. A message that wraps
                // keeps its rows whole, and the tag has a row of its own
                // under the last.
                let wraps = lines.iter().any(|line| line.join != Join::Break);
                match lines.first_mut() {
                    Some(first) if !wraps && spans_width(&first.spans) + 2 + tag_width <= inner => {
                        let tail = inner - spans_width(&first.spans);
                        let gap = " ".repeat(tail - tag_width);
                        first.spans.push(Span::styled(format!("{gap}{tag}"), style));
                        first.tail += tail;
                    }
                    _ => {
                        let gap = " ".repeat(inner.saturating_sub(tag_width));
                        let mut line =
                            Wrapped::line(vec![Span::styled(format!("{gap}{tag}"), style)]);
                        line.bare = true;
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
            // A message a worker got says whose it is, and outside its own
            // view a worker's message to the user says whose it is too.
            let header = match &item.from {
                Some(from) => Some(quiet::mail_label(from, &item.agent)),
                None if !item.is_main() && !own => Some(quiet::mail_label(&item.agent, "you")),
                None => None,
            };
            if let Some(header) = header {
                lines.push(Wrapped::line(vec![Span::styled(
                    header,
                    theme.dim().add_modifier(Modifier::BOLD),
                )]));
            }
            lines.extend(markdown::render(&item.text, theme.text(), theme, inner));
            wrap::prefixed(lines, mark("✉", theme.dim()), blank())
        }
        Kind::Question => {
            // One quiet line while its questions are open (the answer panel
            // has them), then the record of what became of each, boxed as
            // the panel is.
            let Some(questions) = &item.questions else {
                // Asked before questions had ids, or no longer kept.
                let mut lines = vec![Wrapped::line(vec![Span::styled(
                    "Arko asked".to_string(),
                    theme.dim(),
                )])];
                lines.extend(markdown::render(&item.text, theme.text(), theme, inner));
                return wrap::prefixed(lines, mark("?", theme.accent()), blank());
            };
            if questions.iter().any(|question| !question.state.is_open()) {
                return record_lines(&item.text, questions, theme, width);
            }
            let lines: Vec<Wrapped> =
                wrap::wrap(&[Span::styled(rendered(&item.text), theme.dim())], inner, 0)
                    .into_iter()
                    .map(Wrapped::line)
                    .collect();
            wrap::prefixed(lines, mark("?", theme.accent()), blank())
        }
        Kind::Call => {
            // A call that has something to show opens to it, the whole of
            // its words where its line cut them, its detail here, and its
            // content as its result's block.
            let fold = folds.then_some(open);
            let mut lines = call_line(item, theme, width, own, clock, fold, false);
            if folds && open {
                if call_cut(item, width, false) {
                    lines.extend(railed(detail_lines(&call_words(item), theme, inner), theme));
                }
                if let Some(detail) = &item.detail {
                    lines.extend(railed(detail_lines(detail, theme, inner), theme));
                }
            }
            lines
        }
        Kind::Result => railed(content_lines(item, theme, inner, open), theme),
        Kind::Notice => {
            let (symbol, style) = match item.level {
                Some(Level::Error) => ("✗", theme.error()),
                Some(Level::Warn) => ("!", theme.warning()),
                _ => ("·", theme.dim()),
            };
            let mut spans = worker_prefix(item);
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
        // In the text column, with no label or marker. The spinner stands in
        // the marker's column while it streams, and a click on any of its
        // lines opens or closes it.
        Kind::Thinking => {
            let who = worker_prefix(item).pop();
            wrap::prefixed(
                thought_lines(&item.text, theme, inner, open, who),
                blank(),
                blank(),
            )
        }
        // Drawn from the worker's summary.
        Kind::WorkerLine => Vec::new(),
    }
}

/// The banner's labels' column.
const BANNER_LABELS: usize = 11;

/// The banner, boxed, with the title, a row for each fact (its label and its
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
    // A line in the box, `lead` (a label, or nothing) and the text, with
    // the box's sides. The copy starts after the lead.
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
    // A row's value, shortened to fit, and a copy of the row takes it whole.
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
    // A blank line above the box, since one row reads as wide as the two
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
        // Tools may take more than a line, under their value.
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

/// The record of a question card whose questions settled, boxed as the
/// answer panel is, with `heading` in its top border, then each settled
/// question, numbered as the panel numbered it, with what became of it
/// under it. A copy takes the questions and outcomes, not the box.
fn record_lines(
    heading: &str,
    questions: &[crate::questions::AgentQuestion],
    theme: &Theme,
    width: usize,
) -> Vec<Wrapped> {
    let set = theme.borders();
    // Settled questions need nothing more from the user, so the box is
    // drawn as quietly as an overlay's, not in the open panel's accent.
    let border = Style::new().fg(if theme.is_terminal() {
        ratatui::style::Color::Reset
    } else {
        theme.border
    });
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
            let title = cut_end(&rendered(title), outer.saturating_sub(4));
            used += text_width(&title) + 2;
            spans.push(Span::styled(format!(" {title} "), theme.dim()));
        }
        spans.push(Span::styled(
            set.horizontal_top.repeat(outer.saturating_sub(used)),
            border,
        ));
        spans.push(Span::styled(right.to_string(), border));
        Wrapped {
            bare: true,
            ..Wrapped::line(spans)
        }
    };
    // `line` in the box, `lead` columns in, between its sides.
    let side = |line: Wrapped, lead: usize| {
        let pad = inner.saturating_sub(lead + spans_width(&line.spans));
        let mut spans = vec![
            Span::raw(" ".repeat(MARGIN)),
            Span::styled(format!("{} ", set.vertical_left), border),
            Span::raw(" ".repeat(lead)),
        ];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(format!(" {}", set.vertical_right), border));
        Wrapped {
            spans,
            skip: MARGIN + 2 + lead + line.skip,
            tail: line.tail + pad + 2,
            ..line
        }
    };
    let mut lines = vec![edge(set.top_left, set.top_right, Some(heading))];
    for question in questions {
        let Some(outcome) = outcome(&question.state) else {
            continue;
        };
        let number = question
            .number
            .map(|number| format!("{number}. "))
            .unwrap_or_default();
        let indent = 2 + text_width(&number);
        let room = inner.saturating_sub(indent).max(4);
        let text = question.question.trim().to_string();
        let mut text = markdown::render(&text, theme.text(), theme, room);
        if text.is_empty() {
            text.push(Wrapped::line(Vec::new()));
        }
        for (row, line) in text.into_iter().enumerate() {
            if row == 0 {
                let number = Span::styled(number.clone(), theme.text());
                lines.push(side(line.decorated(number), 2));
            } else {
                lines.push(side(line, indent));
            }
        }
        let outcome = [Span::styled(rendered(&outcome), theme.dim())];
        for line in wrap::wrap_lines(&outcome, room, 0) {
            lines.push(side(line, indent));
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
    // (The ellipsis follows the last word, not a space after it.)
    cut.trim_end().to_string() + &glyphs::render("…")
}

/// `text` in `width` columns, cut in its middle with an ellipsis, so a path
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

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| text_width(&span.content)).sum()
}

/// How long a running call has run, once that is a second or more, so a
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

/// What sets a worker apart from the usual ones, for its line, with its profile,
/// or else its model when workers start on another, and its effort when that
/// is another too (`impl-light`, `luna high`).
pub fn setup(worker: &WorkerSummary, usual: &Usual) -> Option<String> {
    if let Some(profile) = worker.profile.as_deref().filter(|name| !name.is_empty()) {
        return Some(profile.to_string());
    }
    let model = worker
        .model
        .as_deref()
        .filter(|model| *model != usual.model)
        .map(crate::providers::short_model_name);
    let effort = worker
        .effort
        .as_deref()
        .filter(|effort| *effort != usual.effort);
    match (model, effort) {
        (Some(model), Some(effort)) => Some(format!("{model} {effort}")),
        (Some(model), None) => Some(model),
        (None, Some(effort)) => Some(format!("{effort} effort")),
        (None, None) => None,
    }
}

/// `text` cut to `room` columns, with an ellipsis, when it is wider.
fn fit(text: &str, room: usize) -> String {
    if text_width(text) <= room {
        return text.to_string();
    }
    let ellipsis = glyphs::render("…");
    let mut fitted = String::new();
    let mut taken = text_width(&ellipsis);
    for c in text.chars() {
        let width = wrap::char_width(c);
        if taken + width > room {
            break;
        }
        fitted.push(c);
        taken += width;
    }
    fitted + &ellipsis
}

/// A worker's line, like `r2 Fix flaky test · 18 tool calls · 2m · running`, with
/// its setup after the task when it isn't the usual one.
fn worker_line(
    worker: Option<&WorkerSummary>,
    id: &str,
    usual: &Usual,
    theme: &Theme,
    width: usize,
) -> Line<'static> {
    let mut spans = vec![
        Span::raw(" ".repeat(MARGIN)),
        Span::styled(id.to_string(), theme.text().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
    ];
    if let Some(worker) = worker {
        let mut facts = Vec::new();
        facts.extend(setup(worker, usual));
        facts.push(format!(
            "{} tool call{}",
            worker.tool_calls,
            if worker.tool_calls == 1 { "" } else { "s" }
        ));
        facts.extend(worker_time(worker));
        let facts = rendered(format!(" · {} · ", facts.join(" · ")));
        let (word, style) = state(worker, theme);
        // A long task gives way to what follows it, so the setup, the time
        // and the state stay on the line.
        let taken = MARGIN + text_width(id) + 1 + text_width(&facts) + text_width(word);
        let title = fit(&rendered(&worker.title), width.saturating_sub(taken).max(8));
        spans.push(Span::styled(title, theme.text()));
        spans.push(Span::styled(facts, theme.dim()));
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

/// A worker view's heading, the worker's ID, task and state.
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

/// What became of a question, as its card's record says it under the
/// question (`None` while it is open).
fn outcome(state: &crate::questions::QuestionState) -> Option<String> {
    use crate::questions::QuestionState;
    Some(match state {
        QuestionState::Open => return None,
        QuestionState::Answered { answer } => answer.clone(),
        QuestionState::AnsweredInMessage => "Answered in your message".to_string(),
        QuestionState::Closed { why: Some(why) } => format!("Arko closed it. {}", why.trim()),
        QuestionState::Closed { why: None } => "Arko closed it".to_string(),
        QuestionState::Dismissed => "Dismissed".to_string(),
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

    /// A question card is one quiet line while its questions are open (the
    /// answer panel has them). Once one settles, it is their record, boxed
    /// as the panel is, with each question settled, numbered as the panel
    /// numbered it, and what became of it.
    #[test]
    fn a_question_card_is_a_line_until_its_questions_settle() {
        use crate::questions::QuestionState;
        let mut t = Transcript::new();
        let mut card = item(1, Kind::Question, "Arko asked 2 questions");
        let mut questions = crate::tui::app::tests::asking().questions;
        questions[1].question = "Push now?".into();
        card.questions = Some(questions.clone());
        t.apply(Update::Item(card.clone()));
        assert_eq!(rows(&mut t, &full(), 40, 1), ["? Arko asked 2 questions"]);
        questions[0].state = QuestionState::Answered {
            answer: "Spaces".into(),
        };
        card.questions = Some(questions.clone());
        t.apply(Update::Change(card.clone()));
        let top = format!("  ┌ Arko asked 2 questions {}┐", "─".repeat(10));
        let bottom = format!("  └{}┘", "─".repeat(34));
        let boxed = |text: &str| format!("  │ {text:<32} │");
        assert_eq!(
            rows(&mut t, &full(), 40, 4),
            [
                top.clone(),
                boxed("  1. Tabs or spaces?"),
                boxed("     Spaces"),
                bottom.clone(),
            ]
        );
        questions[1].state = QuestionState::Closed {
            why: Some("Found it".into()),
        };
        card.questions = Some(questions);
        t.apply(Update::Change(card));
        assert_eq!(
            rows(&mut t, &full(), 40, 6),
            [
                top,
                boxed("  1. Tabs or spaces?"),
                boxed("     Spaces"),
                boxed("  2. Push now?"),
                boxed("     Arko closed it. Found it"),
                bottom,
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
            ["· message r1: Also the docs. · ✓ sent", ""]
        );
        assert_eq!(
            rows(&mut t, &Filter::Worker("r1".into(), View::Full), 40, 2),
            ["✉ main to r1", "  Also the docs."]
        );
    }

    /// A worker's message to the user shows in its own view only, as its
    /// text does. The main transcript has none of it.
    #[test]
    fn a_workers_message_to_the_user_shows_in_its_own_view_only() {
        let mut t = Transcript::new();
        t.apply(Update::Item(item(1, Kind::User, "Fix it")));
        t.apply(Update::Item(item(2, Kind::Text, "Looking.").agent("r3")));
        t.apply(Update::Item(
            item(3, Kind::Message, "Found it.").agent("r3"),
        ));
        for view in View::ALL {
            assert_eq!(
                rows(&mut t, &Filter::Main(view), 40, 2),
                ["▎ Fix it", ""],
                "{view:?}"
            );
        }
        assert_eq!(
            rows(&mut t, &Filter::Worker("r3".into(), View::Full), 40, 3),
            ["  Looking.", "", "✉ Found it."]
        );
    }

    /// The main transcript shows a worker of the main agent only as its
    /// reports and its messages to its lead, in every view, each a row that
    /// opens to its words. Its text, its calls and what went wrong are its
    /// own view's, and so is a report of a worker's worker.
    #[test]
    fn a_worker_shows_in_the_main_transcript_as_its_reports_and_messages() {
        use crate::remote::{Act, ActKind};
        let theme = Theme::terminal();
        let mut t = Transcript::new();
        t.apply(Update::Item(item(1, Kind::User, "Fix it")));
        t.apply(Update::Item(item(2, Kind::Text, "Looking.").agent("r3")));
        let mut read = item(3, Kind::Call, "read a.rs").agent("r3");
        read.status = Some(CallStatus::Ok);
        read.act = Some(Act {
            kind: ActKind::Read,
            target: "a.rs".into(),
            ..Act::default()
        });
        t.apply(Update::Item(read));
        let mut message = item(4, Kind::Call, "message main “Halfway.”").agent("r3");
        message.status = Some(CallStatus::Ok);
        message.outcome = Some("sent".into());
        message.act = Some(Act {
            kind: ActKind::Message,
            target: "main".into(),
            ..Act::default()
        });
        t.apply(Update::Item(message));
        let mut said = item(5, Kind::Result, "Halfway.").agent("r3");
        said.content = Some(ContentKind::Plain);
        said.call = Some(4);
        t.apply(Update::Item(said));
        let mut failed = item(6, Kind::Notice, "failed (boom)").agent("r3");
        failed.level = Some(Level::Error);
        t.apply(Update::Item(failed));
        t.apply(Update::Item(
            item(7, Kind::Reply, "Fixed **it**.").agent("r3"),
        ));
        // (r5 is r3's worker, whose report goes to r3.)
        t.workers.push(WorkerSummary {
            parent: Some("r3".into()),
            ..WorkerSummary::new("r5")
        });
        t.apply(Update::Item(item(8, Kind::Reply, "Counted.").agent("r5")));
        for view in View::ALL {
            let filter = Filter::Main(view);
            assert_eq!(
                rows(&mut t, &filter, 40, 5),
                [
                    "▎ Fix it",
                    "",
                    "▸ r3 messaged the lead",
                    "▸ r3 reported",
                    ""
                ],
                "{view:?}"
            );
            t.toggle(4, 2, &filter, &theme);
            t.toggle(7, 3, &filter, &theme);
            // (Open, each stands apart from the other by a blank line.)
            t.follow();
            assert_eq!(
                rows(&mut t, &filter, 40, 7),
                [
                    "▎ Fix it",
                    "",
                    "▾ r3 messaged the lead",
                    "│ Halfway.",
                    "",
                    "▾ r3 reported",
                    "│ Fixed it."
                ],
                "{view:?}"
            );
            t.toggle(4, 2, &filter, &theme);
            t.toggle(7, 3, &filter, &theme);
        }
        // Its own view has all of it.
        let own = rows(&mut t, &Filter::Worker("r3".into(), View::Full), 40, 12);
        assert!(own.iter().any(|row| row.contains("Looking.")), "{own:?}");
        assert!(own.iter().any(|row| row.contains("read a.rs")), "{own:?}");
    }

    /// A worker's message to its lead keeps how its call stands. While it
    /// runs its row says so, with no fold until its content comes. Sent, it
    /// opens to its content as a call's does, its first lines and the door
    /// to the rest. Failed, it says so with ✗, and cut short, with the stop.
    /// A worker's worker shows so in its lead's view, not in main.
    #[test]
    fn a_workers_message_row_keeps_its_call_and_its_content() {
        use crate::remote::{Act, ActKind};
        let theme = Theme::terminal();
        let mut t = Transcript::new();
        t.workers.push(WorkerSummary {
            parent: Some("r1".into()),
            ..WorkerSummary::new("r2")
        });
        let message = |id: u64, agent: &str, lead: &str| {
            let mut call = item(id, Kind::Call, "message the lead").agent(agent);
            call.status = Some(CallStatus::Running);
            call.act = Some(Act {
                kind: ActKind::Message,
                target: lead.into(),
                ..Act::default()
            });
            call
        };
        let main = full();
        t.apply(Update::Item(item(1, Kind::User, "Fix it")));
        let mut call = message(2, "r1", "main");
        t.apply(Update::Item(call.clone()));
        assert_eq!(
            rows(&mut t, &main, 40, 3)[2],
            "· r1 messaging the lead …",
            "no fold while it runs"
        );
        call.status = Some(CallStatus::Ok);
        t.apply(Update::Change(call));
        let lines: Vec<String> = (1..=40).map(|at| format!("Line {at}")).collect();
        let mut said = item(3, Kind::Result, &lines[..16].join("\n")).agent("r1");
        said.content = Some(ContentKind::Plain);
        said.call = Some(2);
        said.more = Some("… 24 more lines".into());
        said.whole = Some(lines.join("\n").into());
        t.apply(Update::Item(said));
        assert_eq!(rows(&mut t, &main, 40, 3)[2], "▸ r1 messaged the lead");
        t.toggle(2, 2, &main, &theme);
        t.follow();
        let shown = rows(&mut t, &main, 40, 20);
        assert_eq!(shown[2], "▾ r1 messaged the lead", "{shown:?}");
        assert_eq!(shown[3], "│ Line 1");
        assert_eq!(shown[18], "│ Line 16");
        assert_eq!(shown[19], "│ … 24 more lines", "its door, not all 40");
        // Its door opens the rest.
        t.toggle(3, 19, &main, &theme);
        t.follow();
        let shown = rows(&mut t, &main, 40, 50);
        assert!(shown.iter().any(|row| row == "│ Line 40"), "{shown:?}");
        // Once its whole is no longer kept, its door says so.
        t.toggle(3, 0, &main, &theme);
        if let Some(entry) = t.find_mut(3) {
            entry.item.whole = None;
            entry.touch();
        }
        let shown = rows(&mut t, &main, 40, 20);
        assert_eq!(shown[19], "│ No longer kept", "{shown:?}");
        t.toggle(2, 2, &main, &theme);

        // One that failed, and one a stop cut short.
        let mut failed = message(4, "r1", "main");
        failed.status = Some(CallStatus::Failed);
        t.apply(Update::Item(failed));
        let mut why = item(5, Kind::Result, "Couldn't deliver the message.").agent("r1");
        why.content = Some(ContentKind::Muted);
        why.call = Some(4);
        t.apply(Update::Item(why));
        let mut stopped = message(6, "r1", "main");
        stopped.status = Some(CallStatus::Failed);
        stopped.outcome = Some(quiet::STOPPED.into());
        t.apply(Update::Item(stopped));
        rows(&mut t, &main, 60, 5);
        t.follow();
        let shown = rows(&mut t, &main, 60, 5);
        let at = shown
            .iter()
            .position(|row| row.starts_with("▸ r1 couldn’t message the lead"))
            .unwrap_or_else(|| panic!("{shown:?}"));
        assert!(shown[at].ends_with("✗"), "{shown:?}");
        assert_eq!(shown[at + 1], "· r1 couldn’t message the lead · stopped");
        t.toggle(4, at as u16, &main, &theme);
        rows(&mut t, &main, 60, 6);
        t.follow();
        let shown = rows(&mut t, &main, 60, 6);
        assert!(
            shown
                .iter()
                .any(|row| row == "│ Couldn't deliver the message."),
            "{shown:?}"
        );

        // r2, r1's worker, messages r1 and reports to it. r1's view shows
        // those rows. Main doesn't, and r1's own message is its own call.
        let mut child = message(7, "r2", "r1");
        child.status = Some(CallStatus::Ok);
        t.apply(Update::Item(child));
        t.apply(Update::Item(item(8, Kind::Reply, "Counted.").agent("r2")));
        let lead = Filter::Worker("r1".into(), View::Full);
        let shown = rows(&mut t, &lead, 40, 12);
        assert!(
            shown.iter().any(|row| row == "· r2 messaged the lead"),
            "{shown:?}"
        );
        assert!(shown.iter().any(|row| row == "▸ r2 reported"), "{shown:?}");
        assert!(
            !shown.iter().any(|row| row.contains("r1 messaged")),
            "{shown:?}"
        );
        let shown = rows(&mut t, &main, 40, 12);
        assert!(!shown.iter().any(|row| row.contains("r2")), "{shown:?}");
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
            rows(&mut t, &Filter::Worker("r1".into(), View::Full), 50, 2),
            [
                format!("▎ Use tabs.{}from the web page", " ".repeat(22)),
                String::new(),
            ]
        );
        assert_eq!(
            rows(&mut t, &Filter::Worker("r2".into(), View::Full), 50, 2),
            ["  Testing.", ""]
        );
    }

    /// A running call shows how long it has run once that is a second,
    /// and the time goes on with nothing new from the feed. Its outcome
    /// takes its place.
    #[test]
    fn a_running_call_shows_its_time() {
        let mut t = Transcript::new();
        let mut call = item(1, Kind::Call, "$ sleep 30");
        call.status = Some(CallStatus::Running);
        call.agent = "r1".into();
        call.at -= 900;
        t.apply(Update::Item(call.clone()));
        let own = Filter::Worker("r1".into(), View::Full);
        assert_eq!(rows(&mut t, &own, 40, 1)[0], "· $ sleep 30 …");
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert_eq!(rows(&mut t, &own, 40, 1)[0], "· $ sleep 30 · 1s …");
        call.at -= 2 * 60_000;
        t.apply(Update::Change(call.clone()));
        assert_eq!(rows(&mut t, &own, 40, 1)[0], "· $ sleep 30 · 2m …");
        call.status = Some(CallStatus::Ok);
        call.outcome = Some("30.0 s".into());
        t.apply(Update::Change(call));
        assert_eq!(rows(&mut t, &own, 40, 1)[0], "· $ sleep 30 · ✓ 30.0 s");
    }

    /// A call's outcome is on its line. A command's output is its content,
    /// open under it from the start. The user's message has the bar. The
    /// end of a turn draws nothing (the status line says how long it ran).
    #[test]
    fn items_draw_as_the_design_says() {
        let mut t = Transcript::new();
        let mut user = item(1, Kind::User, "Run the tests");
        user.delivery = Some(Delivery::Turn);
        t.apply(Update::Item(user));
        let mut call = item(2, Kind::Call, "$ cargo test");
        call.status = Some(CallStatus::Running);
        call.act = Some(crate::remote::Act {
            kind: ActKind::Command,
            ..Default::default()
        });
        t.apply(Update::Item(call.clone()));
        assert_eq!(rows(&mut t, &full(), 40, 3)[2], "· $ cargo test …");
        call.status = Some(CallStatus::Ok);
        call.outcome = Some("12 passed".into());
        t.apply(Update::Change(call));
        let mut result = item(3, Kind::Result, "running 12 tests");
        result.content = Some(ContentKind::Plain);
        result.call = Some(2);
        t.apply(Update::Item(result));
        t.apply(Update::Item(item(4, Kind::Reply, "All **12** pass.")));
        t.apply(Update::Item(item(5, Kind::Turn, "3 s · $0.010")));
        assert_eq!(
            rows(&mut t, &full(), 40, 8),
            [
                "▎ Run the tests",
                "",
                "▾ $ cargo test · ✓ 12 passed",
                "│ running 12 tests",
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

    /// An agent's thinking is its first line, in the text column, with no
    /// label or marker, in Full and Quiet and in its worker's own view, and
    /// none in Messages. A click on any of its lines opens it whole and
    /// closes it again, and one that fits on its line has nothing to open.
    /// Open, it stands apart from its neighbours by a blank line on each
    /// side. With `--show-thinking` it starts whole.
    #[test]
    fn thinking_is_its_first_line_and_opens_whole() {
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
        let folded = ["  Planning …", "", "  Running them."];
        assert_eq!(rows(&mut t, &full(), 40, 3), folded);
        assert_eq!(rows(&mut t, &Filter::Main(View::Quiet), 40, 3), folded);
        assert_eq!(
            rows(&mut t, &Filter::Main(View::Messages), 40, 1),
            ["  Running them."]
        );
        let worker = Filter::Worker("r1".into(), View::Full);
        assert_eq!(rows(&mut t, &worker, 40, 1), ["  Where is the parser?"]);
        assert_eq!(t.fold_at(4, 0, &worker, &theme), None, "it fits");
        // Any of its lines opens it and folds it again.
        let open = [
            "  Planning",
            "  Read the tests first.",
            "",
            "  Running them.",
        ];
        for line in [0, 1] {
            rows(&mut t, &full(), 40, 8);
            assert_eq!(
                t.fold_at(4, 0, &full(), &theme).map(|fold| fold.item),
                Some(1)
            );
            t.toggle(1, 0, &full(), &theme);
            assert_eq!(rows(&mut t, &full(), 40, 8)[..4], open);
            assert_eq!(
                t.fold_at(10, line, &full(), &theme).map(|fold| fold.item),
                Some(1),
                "{line}"
            );
            t.toggle(1, line, &full(), &theme);
            assert_eq!(rows(&mut t, &full(), 40, 8)[..3], folded, "{line}");
        }
        assert_eq!(t.fold_at(4, 2, &full(), &theme), None, "not the text after");
        // Its words are dim and italic.
        let area = Rect::new(0, 0, 40, 3);
        let mut buffer = Buffer::empty(area);
        t.draw(&mut buffer, area, &full(), &theme);
        let cell = &buffer[(2, 0)];
        assert!(cell.modifier.contains(Modifier::ITALIC), "{cell:?}");
        assert_eq!(cell.fg, theme.dim().fg.unwrap());

        let mut shown = Transcript::new();
        shown.set_open_thinking(true);
        shown.apply(Update::Item(item(
            1,
            Kind::Thinking,
            "All of it.\n\nAnd more.",
        )));
        let mut call = item(2, Kind::Call, "$ ls");
        call.status = Some(CallStatus::Ok);
        shown.apply(Update::Item(call));
        assert_eq!(
            rows(&mut shown, &full(), 40, 5),
            ["  All of it.", "", "  And more.", "", "· $ ls · ✓"],
            "a blank line apart from the call after it"
        );
    }

    /// What follows the spinner in its column of `row`, if it has one.
    fn after_spinner(row: &str) -> Option<&str> {
        let (frames, _) = glyphs::spinner_frames(glyphs::symbols());
        let row = row.strip_prefix(" ".repeat(SPINNER).as_str())?;
        let first = row.chars().next()?;
        let spins = frames.contains(&first.to_string().as_str());
        spins.then(|| &row[first.len_utf8()..])
    }

    /// Whether `row` is the "Working…" line, with its spinner in the
    /// markers' column, then the word.
    fn is_working(row: &str) -> bool {
        after_spinner(row) == Some(" Working…")
    }

    /// Every row's marker (the user's bar, a call's triangle, ✉, a notice's
    /// dot) stands at the left edge, and its text at the margin, where the
    /// agent's text starts, so the calls and the user's messages make a
    /// column of their own. The live line's spinner stands in that column
    /// too, and a thinking has no marker, its text at the margin. In every
    /// view.
    #[test]
    fn markers_stand_in_the_gutter() {
        use crate::remote::{Act, ActKind};
        let mut t = Transcript::new();
        let mut user = item(1, Kind::User, "Fix it");
        user.turn = Some(1);
        let text = item(2, Kind::Text, "Looking.");
        let mut thinking = item(3, Kind::Thinking, "Where to start?");
        thinking.turn = Some(1);
        let mut call = item(4, Kind::Call, "$ cargo test");
        call.turn = Some(1);
        call.status = Some(CallStatus::Ok);
        call.act = Some(Act {
            kind: ActKind::Command,
            target: "cargo test".into(),
            ..Act::default()
        });
        let mut notice = item(5, Kind::Notice, "news");
        notice.level = Some(Level::Info);
        let message = item(6, Kind::Message, "Found it.\nIn a.rs.").agent("r3");
        for item in [user, text, thinking, call, notice, message] {
            t.apply(Update::Item(item));
        }
        t.working = Some(0);
        for view in View::ALL {
            let shown = rows(&mut t, &Filter::Main(view), 40, 16);
            assert!(shown.iter().any(|row| is_working(row)), "{shown:?}");
            let mut marked = 0;
            for row in shown.iter().filter(|row| !row.trim().is_empty()) {
                let chars: Vec<char> = row.chars().collect();
                let at = |column: usize| chars.get(column).copied().unwrap_or(' ');
                // A marker, or none for the agent's text, its thinking and
                // a row's other lines, then the text at the margin.
                marked += usize::from(at(0) != ' ' && !is_working(row));
                assert_eq!(at(1), ' ', "{view:?} {row:?}");
                assert_ne!(at(MARGIN), ' ', "{view:?} {row:?}");
            }
            // (The user's bar and the call's, and in Full and Quiet the notice's.)
            assert!(marked >= 2, "{view:?} {shown:?}");
        }
    }

    /// A thinking between two calls of a Quiet row is inside it, between
    /// them. Open, the row shows it on its rail in the text column of its
    /// calls, with no label or marker. Opened, it stands apart from the
    /// calls by a blank rail line on each side, and a click on its text
    /// folds it.
    #[test]
    fn a_thinking_in_an_open_quiet_row_stays_on_the_rail() {
        use crate::remote::{Act, ActKind};
        let quiet = Filter::Main(View::Quiet);
        let theme = Theme::terminal();
        let mut t = Transcript::new();
        let read = |id: u64, path: &str| {
            let mut call = item(id, Kind::Call, path);
            call.turn = Some(1);
            call.status = Some(CallStatus::Ok);
            call.outcome = Some("3 lines".into());
            call.act = Some(Act {
                kind: ActKind::Read,
                target: path.into(),
                ..Act::default()
            });
            call
        };
        let mut thinking = item(2, Kind::Thinking, "Which one?\nThe second.");
        thinking.turn = Some(1);
        for item in [read(1, "a.rs"), thinking, read(3, "b.rs")] {
            t.apply(Update::Item(item));
        }
        assert_eq!(rows(&mut t, &quiet, 40, 2), ["▸ Read a.rs and b.rs", ""]);
        t.toggle(1, 0, &quiet, &theme);
        let closed = [
            "▾ Read a.rs and b.rs",
            "│ · a.rs · ✓ 3 lines",
            "│   Which one? …",
            "│ · b.rs · ✓ 3 lines",
        ];
        assert_eq!(rows(&mut t, &quiet, 40, 8)[..4], closed);
        t.toggle(2, 2, &quiet, &theme);
        let open = [
            "▾ Read a.rs and b.rs",
            "│ · a.rs · ✓ 3 lines",
            "│",
            "│   Which one?",
            "│   The second.",
            "│",
            "│ · b.rs · ✓ 3 lines",
        ];
        let shown = rows(&mut t, &quiet, 40, 8);
        assert_eq!(shown[..7], open);
        // A click on its text folds it.
        assert_eq!(
            t.fold_at(10, 4, &quiet, &theme).map(|fold| fold.item),
            Some(2)
        );
        t.toggle(2, 4, &quiet, &theme);
        assert_eq!(rows(&mut t, &quiet, 40, 8)[..4], closed);
    }

    /// The live line has its spinner in the markers' column and its words
    /// in the text column. It is "Working…" while the agent waits on its
    /// model, then the thinking that streams, in its place. Once the thinking
    /// stops the spinner goes and its words stay where they are.
    #[test]
    fn the_live_lines_spinner_stands_in_the_markers_column() {
        let theme = Theme::terminal();
        let (frames, _) = glyphs::spinner_frames(glyphs::symbols());
        let columns = |row: &str, word: char| {
            let cell = row
                .chars()
                .position(|c| frames.contains(&c.to_string().as_str()));
            (cell, row.chars().position(|c| c == word))
        };
        let mut t = Transcript::new();
        let mut user = item(1, Kind::User, "Fix the tests");
        user.delivery = Some(Delivery::Turn);
        t.apply(Update::Item(user));
        t.working = Some(0);
        let shown = rows(&mut t, &full(), 40, 4);
        assert_eq!(
            columns(&shown[2], 'W'),
            (Some(0), Some(MARGIN)),
            "{shown:?}"
        );
        t.apply(Update::Item(item(2, Kind::Thinking, "Where to start?")));
        let shown = rows(&mut t, &full(), 40, 4);
        assert_eq!(
            after_spinner(&shown[2]),
            Some(" Where to start?"),
            "{shown:?}"
        );
        assert!(
            !shown.iter().any(|row| row.contains("Thinking")),
            "{shown:?}"
        );
        // The reply ends it, and its words stay put.
        t.apply(Update::Item(item(3, Kind::Reply, "Done.")));
        let shown = rows(&mut t, &full(), 40, 5);
        assert_eq!(shown[2], "  Where to start?", "{shown:?}");
        // It fits on its line, so it has nothing to open.
        assert_eq!(t.fold_at(MARGIN as u16, 2, &full(), &theme), None);
    }

    /// In the quiet view only a click opens a thinking, also with thinking
    /// open by default, and the full view keeps its own state.
    #[test]
    fn the_quiet_view_opens_thinking_only_when_clicked() {
        let theme = Theme::terminal();
        let quiet = Filter::Main(View::Quiet);
        let mut t = Transcript::new();
        t.set_open_thinking(true);
        t.apply(Update::Item(item(
            1,
            Kind::Thinking,
            "Where to start?\nThe parser.",
        )));
        assert_eq!(
            rows(&mut t, &full(), 40, 3)[..2],
            ["  Where to start?", "  The parser."]
        );
        // It streams, its first line the live line.
        t.working = Some(0);
        assert_eq!(
            after_spinner(&rows(&mut t, &quiet, 40, 3)[0]),
            Some(" Where to start? …")
        );
        t.apply(Update::Item(item(2, Kind::Reply, "Done.")));
        t.working = None;
        assert_eq!(rows(&mut t, &quiet, 40, 3)[..1], ["  Where to start? …"]);
        // A click opens it there, and leaves the full view as it was.
        assert_eq!(
            t.fold_at(MARGIN as u16, 0, &quiet, &theme)
                .map(|fold| fold.item),
            Some(1)
        );
        t.toggle(1, 0, &quiet, &theme);
        t.follow();
        assert_eq!(
            rows(&mut t, &quiet, 40, 6)[..3],
            ["  Where to start?", "  The parser.", ""]
        );
        assert_eq!(rows(&mut t, &full(), 40, 6)[0], "  Where to start?");
        t.toggle(1, 0, &quiet, &theme);
        t.follow();
        assert_eq!(rows(&mut t, &quiet, 40, 3)[..1], ["  Where to start? …"]);
        assert_eq!(
            rows(&mut t, &full(), 40, 5)[..2],
            ["  Where to start?", "  The parser."]
        );
    }

    /// In Messages a turn's count of calls opens to the turn's Quiet rows in
    /// place, each where it came among what the agent said, its line staying
    /// on its row. It closes again, and each turn keeps one count, which the
    /// keyboard opens too.
    #[test]
    fn a_turns_count_opens_to_its_quiet_rows_in_place() {
        use crate::remote::{Act, ActKind};
        let theme = Theme::terminal();
        let messages = Filter::Main(View::Messages);
        let mut t = Transcript::new();
        let of_turn = |mut item: Item, turn: u64| {
            item.turn = Some(turn);
            item
        };
        let read = |id: u64, path: &str, turn: u64| {
            let mut call = item(id, Kind::Call, &format!("read {path}"));
            call.status = Some(CallStatus::Ok);
            call.act = Some(Act {
                kind: ActKind::Read,
                target: path.into(),
                ..Act::default()
            });
            of_turn(call, turn)
        };
        let mut user = item(1, Kind::User, "Fix the parser");
        user.delivery = Some(Delivery::Turn);
        let mut thanks = item(7, Kind::User, "Thanks");
        thanks.delivery = Some(Delivery::Turn);
        for update in [
            of_turn(user, 1),
            read(2, "a.rs", 1),
            read(3, "b.rs", 1),
            of_turn(item(4, Kind::Text, "Found it."), 1),
            read(5, "c.rs", 1),
            of_turn(item(6, Kind::Reply, "Fixed."), 1),
            of_turn(thanks, 2),
            read(8, "d.rs", 2),
        ] {
            t.apply(Update::Item(update));
        }
        let closed = [
            "▎ Fix the parser",
            "",
            "▸ 3 tool calls",
            "",
            "  Found it.",
            "",
            "  Fixed.",
            "",
            "▎ Thanks",
            "",
            "▸ 1 tool call",
        ];
        assert_eq!(rows(&mut t, &messages, 40, 11), closed);
        // A screen too short for the turn, its count on the top row.
        rows(&mut t, &messages, 40, 8);
        t.scroll(-1);
        let shown = rows(&mut t, &messages, 40, 8);
        assert_eq!(shown[0], "▸ 3 tool calls", "{shown:?}");
        t.toggle_count(COUNT_KEY | 2, 0);
        let shown = rows(&mut t, &messages, 40, 8);
        assert_eq!(
            shown[..7],
            [
                "▾ 3 tool calls",
                "▸ Read a.rs and b.rs",
                "",
                "  Found it.",
                "",
                "· Read c.rs",
                "",
            ],
            "the turn's rows under its count, the count on its row"
        );
        // Closed again, as it was.
        t.toggle_count(COUNT_KEY | 2, 0);
        t.follow();
        assert_eq!(rows(&mut t, &messages, 40, 11), closed);
        // The keyboard reaches each turn's count, and opens it.
        assert!(t.move_focus(0, &messages, &theme));
        assert_eq!(t.focus, Some(Target::Block(COUNT_KEY | 8)));
        t.open_focus(Some(true), &messages, &theme);
        let shown = rows(&mut t, &messages, 40, 12);
        assert_eq!(
            shown[shown.len() - 2..],
            ["▾ 1 tool call", "· Read d.rs"],
            "{shown:?}"
        );
        assert_eq!(
            shown.iter().filter(|row| row.contains("tool call")).count(),
            2,
            "one count a turn"
        );
        // A call that runs shows it in its row there, so no line says it
        // again, as one does while the count is closed.
        let mut running = read(9, "e.rs", 2);
        running.status = Some(CallStatus::Running);
        t.apply(Update::Item(running));
        t.working = Some(0);
        let shown = rows(&mut t, &messages, 40, 12);
        assert!(!shown.iter().any(|row| is_working(row)), "{shown:?}");
        t.open_focus(Some(false), &messages, &theme);
        let shown = rows(&mut t, &messages, 40, 12);
        assert!(
            shown.iter().any(|row| row.ends_with("Reading e.rs…")),
            "{shown:?}"
        );
    }

    /// A thinking after a row's call is the live line at the end, its
    /// words where they are, and when the next call joins the row, it goes
    /// in the row, between the calls, and "Working…" takes the live line.
    #[test]
    fn a_thinking_after_a_row_goes_in_it_when_the_next_call_joins() {
        use crate::remote::{Act, ActKind};
        let quiet = Filter::Main(View::Quiet);
        let mut t = Transcript::new();
        let read = |id: u64, path: &str| {
            let mut call = item(id, Kind::Call, &format!("read {path}"));
            call.turn = Some(1);
            call.status = Some(CallStatus::Ok);
            call.act = Some(Act {
                kind: ActKind::Read,
                target: path.into(),
                ..Act::default()
            });
            call
        };
        t.apply(Update::Item(read(1, "a.rs")));
        let mut thinking = item(2, Kind::Thinking, "Which next?");
        thinking.turn = Some(1);
        t.apply(Update::Item(thinking));
        t.working = Some(0);
        let shown = rows(&mut t, &quiet, 40, 4);
        assert_eq!(shown[0], "· Read a.rs", "{shown:?}");
        assert_eq!(after_spinner(&shown[1]), Some(" Which next?"), "{shown:?}");
        assert!(t.drawn.moving);
        // The row's line stays where it was when the next read joins.
        t.apply(Update::Item(read(3, "b.rs")));
        let shown = rows(&mut t, &quiet, 40, 4);
        assert_eq!(shown[0], "▸ Read a.rs and b.rs", "{shown:?}");
        assert!(is_working(&shown[1]), "{shown:?}");
    }

    /// A thinking the reader opened on a line of its own stays there when
    /// the next call comes, and that call starts a row of its own.
    #[test]
    fn an_opened_thinking_stays_on_its_line() {
        use crate::remote::{Act, ActKind};
        let quiet = Filter::Main(View::Quiet);
        let theme = Theme::terminal();
        let mut t = Transcript::new();
        let read = |id: u64, path: &str| {
            let mut call = item(id, Kind::Call, &format!("read {path}"));
            call.turn = Some(1);
            call.status = Some(CallStatus::Ok);
            call.act = Some(Act {
                kind: ActKind::Read,
                target: path.into(),
                ..Act::default()
            });
            call
        };
        t.apply(Update::Item(read(1, "a.rs")));
        let mut thinking = item(2, Kind::Thinking, "Which next?\nThe other one.");
        thinking.turn = Some(1);
        t.apply(Update::Item(thinking));
        rows(&mut t, &quiet, 40, 4);
        t.toggle(2, 1, &quiet, &theme);
        t.apply(Update::Item(read(3, "b.rs")));
        t.follow();
        assert_eq!(
            rows(&mut t, &quiet, 40, 6)[..5],
            ["· Read a.rs", "", "  Which next?", "  The other one.", ""]
        );
        assert_eq!(rows(&mut t, &quiet, 40, 6)[5], "· Read b.rs");
    }

    /// Times read in whole units, as the terminal and the page both say
    /// them.
    #[test]
    fn durations_read_in_whole_units() {
        assert_eq!(duration(999), "0s");
        assert_eq!(duration(45_000), "45s");
        assert_eq!(duration(125_000), "2m");
        assert_eq!(duration(3_900_000), "1h 5m");
    }

    /// A worker's own view shows its thinking as the selected view shows
    /// the main agent's, and a click in Quiet opens it there alone.
    #[test]
    fn a_workers_thinking_follows_the_selected_view() {
        let theme = Theme::terminal();
        let quiet = Filter::Worker("r1".into(), View::Quiet);
        let full = Filter::Worker("r1".into(), View::Full);
        let mut t = Transcript::new();
        t.apply(Update::Item(
            item(1, Kind::Thinking, "Where to start?\nThe parser.").agent("r1"),
        ));
        assert_eq!(rows(&mut t, &quiet, 40, 1), ["  Where to start? …"]);
        assert_eq!(rows(&mut t, &full, 40, 1), ["  Where to start? …"]);
        rows(&mut t, &quiet, 40, 1);
        assert_eq!(
            t.fold_at(MARGIN as u16, 0, &quiet, &theme)
                .map(|fold| fold.item),
            Some(1)
        );
        t.toggle(1, 0, &quiet, &theme);
        t.follow();
        assert_eq!(
            rows(&mut t, &quiet, 40, 6)[..3],
            ["  Where to start?", "  The parser.", ""]
        );
        assert_eq!(rows(&mut t, &full, 40, 3)[..1], ["  Where to start? …"]);
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

        // Its thinking streams, and the thinking shows it, its spinner
        // turning, but not in Messages.
        t.apply(Update::Item(item(2, Kind::Thinking, "Where to start?")));
        let thinking = last(&mut t, View::Full);
        assert_eq!(
            after_spinner(&thinking),
            Some(" Where to start?"),
            "{thinking:?}"
        );
        assert!(t.drawn.moving);
        assert!(is_working(&last(&mut t, View::Messages)));
        // A call runs, then ends, so the model has its result.
        let mut call = item(3, Kind::Call, "$ cargo test");
        call.status = Some(CallStatus::Running);
        t.apply(Update::Item(call.clone()));
        assert_eq!(last(&mut t, View::Full), "· $ cargo test …");
        call.status = Some(CallStatus::Ok);
        t.apply(Update::Change(call));
        let shown = rows(&mut t, &full(), 40, 5);
        assert_eq!(shown[2], "  Where to start?", "it no longer moves");
        assert_eq!(shown[3], "· $ cargo test · ✓", "no blank line before it");
        assert!(is_working(&shown[4]), "{shown:?}");
        assert!(is_working(&last(&mut t, View::Quiet)));
        assert!(is_working(&last(&mut t, View::Messages)));
        // Under the output's rail, which hangs from the call's marker at the
        // left edge, its spinner in the markers' column.
        t.apply(Update::Item(item(4, Kind::Result, "12 passed")));
        let shown = rows(&mut t, &full(), 40, 8);
        let rail = shown
            .iter()
            .find_map(|row| row.chars().position(|c| c == '│'));
        assert_eq!(rail, Some(0), "{shown:?}");
        // (Apart from the open content above it, a blank line on each side.)
        assert_eq!(shown.last().map(String::as_str), Some(""), "{shown:?}");
        assert!(is_working(&shown[shown.len() - 2]), "{shown:?}");

        // Two calls side by side, and while one runs, it shows the agent works.
        let mut calls = [
            item(4, Kind::Call, "read a.rs"),
            item(5, Kind::Call, "read b.rs"),
        ];
        calls[0].status = Some(CallStatus::Running);
        calls[1].status = Some(CallStatus::Ok);
        for call in &calls {
            t.apply(Update::Item(call.clone()));
        }
        assert_eq!(last(&mut t, View::Full), "· read b.rs · ✓");
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
        t.apply(Update::Item(item(9, Kind::Notice, "r1 reported")));
        t.working = Some(ended + 1);
        assert!(is_working(&last(&mut t, View::Full)));
    }

    /// A worker's thinking that streams has the turning spinner in its own
    /// view, also open whole (`--show-thinking`). Thinking a later item
    /// followed is still, as is the thinking of a worker that ended.
    #[test]
    fn the_thinking_that_streams_moves() {
        use crate::tui::app::tests::worker;
        let mut t = Transcript::new();
        t.set_open_thinking(true);
        t.apply(Update::Worker(worker("r1", WorkerState::Running)));
        t.apply(Update::Item(item(1, Kind::Thinking, "Plan.").agent("r1")));
        let own = Filter::Worker("r1".into(), View::Full);
        let shown = rows(&mut t, &own, 40, 5);
        assert_eq!(after_spinner(&shown[2]), Some(" Plan."), "{shown:?}");
        assert_eq!(shown[3..], ["", ""], "nothing below it");
        assert!(t.drawn.moving);
        t.apply(Update::Item(
            item(2, Kind::Text, "Done planning.").agent("r1"),
        ));
        let shown = rows(&mut t, &own, 40, 9);
        assert_eq!(shown[2], "  Plan.", "{shown:?}");
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
    /// every view, not folded into the waiting row, and not dropped in the
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

    #[test]
    fn user_tags_fit_at_the_right_of_the_first_line() {
        let theme = Theme::terminal();
        for (delivery, pending, source, tag) in [
            (Delivery::Note, true, None, "pending"),
            (Delivery::Queued, false, None, "queued"),
            (Delivery::Answer, false, None, "answer"),
            (
                Delivery::Turn,
                false,
                Some(Source::Web),
                "from the web page",
            ),
            (
                Delivery::Note,
                true,
                Some(Source::Web),
                "pending · from the web page",
            ),
        ] {
            let mut user = item(1, Kind::User, "Hi.\nSecond line.");
            user.delivery = Some(delivery);
            user.pending = pending;
            user.source = source;
            let lines = entry_lines(&user, &theme, 40, false, Look::default());
            assert_eq!(lines.len(), 2);
            assert_eq!(spans_width(&lines[0].spans), 40);
            let last = lines[0].spans.last().unwrap();
            assert_eq!(last.content.trim_start(), tag);
            assert!(last.content.starts_with("  "));
            assert_eq!(last.style, theme.dim().add_modifier(Modifier::ITALIC));
            assert_eq!(
                copied(
                    lines.iter().enumerate().map(|(n, line)| ((0, n), line)),
                    ((0, 0, 0), (0, lines.len() - 1, 39)),
                )
                .as_deref(),
                Some(user.text.as_str())
            );
        }
    }

    /// A message's files are dim rows under its words, one each, its path
    /// and size, and a message of files alone has only them.
    #[test]
    fn a_messages_files_are_dim_rows_under_it() {
        use crate::attachments::{FileRef, SentFile};
        let theme = Theme::terminal();
        let sent = |name: &str, bytes| SentFile {
            file: FileRef {
                name: name.into(),
                mime: "video/mp4".into(),
                bytes,
                path: std::path::PathBuf::from("/w/.arko/attachments").join(name),
                lines: None,
            },
            shown: false,
        };
        let mut user = item(1, Kind::User, "Look.");
        user.files = vec![sent("bug.png", 70), sent("RPReplay.MP4", 48_200_000)];
        let lines = entry_lines(&user, &theme, 60, false, Look::default());
        assert_eq!(
            shown(&lines),
            [
                "▎ Look.",
                "▎ /w/.arko/attachments/bug.png · 70 bytes",
                "▎ /w/.arko/attachments/RPReplay.MP4 · 46.0 MB",
            ]
        );
        assert_eq!(lines[1].spans.last().unwrap().style, theme.dim());
        user.text.clear();
        let lines = entry_lines(&user, &theme, 60, false, Look::default());
        assert_eq!(lines.len(), 2);
    }

    /// The rows of `lines` as drawn.
    fn shown(lines: &[Wrapped]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_message_without_room_beside_its_tag_has_it_on_a_row_under_it() {
        let theme = Theme::terminal();
        let under = format!("▎ {}pending", " ".repeat(11));
        for (text, rows) in [
            // Exactly two spaces before the tag.
            ("123456789", vec!["▎ 123456789  pending".to_string()]),
            // One space isn't enough.
            (
                "1234567890",
                vec!["▎ 1234567890".to_string(), under.clone()],
            ),
            (
                "One two three four five six",
                vec![
                    "▎ One two three four".to_string(),
                    "▎ five six".to_string(),
                    under.clone(),
                ],
            ),
        ] {
            let mut user = item(1, Kind::User, text);
            user.delivery = Some(Delivery::Note);
            user.pending = true;
            let lines = entry_lines(&user, &theme, 20, false, Look::default());
            assert_eq!(shown(&lines), rows, "{text}");
            assert_eq!(lines.last().unwrap().bare, rows.len() > 1, "{text}");
            assert_eq!(
                copied(
                    lines.iter().enumerate().map(|(n, line)| ((0, n), line)),
                    ((0, 0, 0), (0, lines.len() - 1, 19)),
                )
                .as_deref(),
                Some(text)
            );
        }
    }

    /// Whatever the tag, the width and the rows the message wraps to. A
    /// message that doesn't wrap has its tag at the right of its first row
    /// when it fits there. Else the message's rows are as they are without
    /// it (the full width), and the tag has a row of its own under the
    /// last, never between them. A copy takes the message without it.
    #[test]
    fn a_user_messages_tag_never_comes_between_its_rows() {
        let theme = Theme::terminal();
        let long = "Look at the parser and the tests that cover it. ".repeat(12);
        let texts = [
            "Fix it.",
            "Fix the tests.",
            "Fix the tests that fail on the CI since the last merge, then tell me why.",
            "Fix the tests that fail on the CI since the last merge, then tell me why \
             they failed and whether the fix should go in the release that ships on Friday.",
            long.trim(),
            "Hi.\nSecond line.",
            "Two lines.\nThe second one is long enough to wrap at the narrower widths here.",
            "A_word_too_long_for_any_row_at_all_so_it_breaks_in_the_middle_of_itself_twice.",
        ];
        let tags: [(Delivery, bool, Option<Source>); 5] = [
            (Delivery::Turn, false, Some(Source::Web)),
            (Delivery::Note, true, Some(Source::Web)),
            (Delivery::Note, true, None),
            (Delivery::Answer, false, None),
            (Delivery::Queued, false, Some(Source::App)),
        ];
        for width in [5, 12, 24, 30, 40, 60, 80, 120] {
            let inner = width - MARGIN;
            for text in texts {
                // The message's rows without a tag.
                let alone: Vec<String> = plain(text, Style::default(), inner)
                    .iter()
                    .map(|line| format!("▎ {}", shown(std::slice::from_ref(line))[0]))
                    .collect();
                let wraps = alone.len() > text.split('\n').count();
                for (delivery, pending, source) in tags {
                    let mut user = item(1, Kind::User, text);
                    user.delivery = Some(delivery);
                    user.pending = pending;
                    user.source = source;
                    let tag = cut_end(&user_tag(&user).unwrap(), inner);
                    let lines = entry_lines(&user, &theme, width, false, Look::default());
                    let rows = shown(&lines);
                    let case = format!("{width} {tag:?} {text:?}\n{}", rows.join("\n"));
                    assert!(
                        lines.iter().all(|line| spans_width(&line.spans) <= width),
                        "{case}"
                    );
                    let first = text_width(text.split('\n').next().unwrap());
                    if !wraps && first + 2 + text_width(&tag) <= inner {
                        assert_eq!(rows.len(), alone.len(), "{case}");
                        assert!(rows[0].starts_with(&alone[0]), "{case}");
                        assert!(rows[0].ends_with(&format!("  {tag}")), "{case}");
                        assert_eq!(rows[1..], alone[1..], "{case}");
                        assert!(lines.iter().all(|line| !line.bare), "{case}");
                    } else {
                        assert_eq!(rows[..rows.len() - 1], alone, "{case}");
                        let last = rows.last().unwrap();
                        assert_eq!(last.trim_start_matches(['▎', ' ']), tag, "{case}");
                        assert_eq!(spans_width(&lines.last().unwrap().spans), width, "{case}");
                        assert!(lines.last().unwrap().bare, "{case}");
                    }
                    assert_eq!(
                        copied(
                            lines.iter().enumerate().map(|(n, line)| ((0, n), line)),
                            ((0, 0, 0), (0, lines.len() - 1, width - 1)),
                        )
                        .as_deref(),
                        Some(text),
                        "{case}"
                    );
                }
            }
        }
    }

    /// "Working…" stands where the agent's output comes next, above a
    /// note the agent hasn't read, which waits at the end. The note has a
    /// blank line above it and one below it, before the status line.
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
        let note = format!("▎ Use tabs.{}pending", " ".repeat(22));
        let shown = rows(&mut t, &full(), 40, 6);
        assert_eq!(shown[..2], ["· $ make · ✓", ""]);
        assert!(is_working(&shown[2]), "{shown:?}");
        assert_eq!(shown[3..], ["", note.as_str(), ""]);
        // While a call runs, with no Working line, the same.
        let mut call = item(3, Kind::Call, "$ make test");
        call.status = Some(CallStatus::Running);
        t.apply(Update::Item(call));
        let shown = rows(&mut t, &full(), 40, 5);
        assert_eq!(shown[1..], ["· $ make test …", "", note.as_str(), ""]);
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
        let own = Filter::Worker("r1".into(), View::Full);
        let shown = rows(&mut t, &own, 40, 4);
        assert!(shown[0].starts_with("  r1 Test it"), "{shown:?}");
        assert_eq!(shown[1], "");
        assert!(is_working(&shown[2]), "{shown:?}");
        assert_eq!(shown[3], "");
        // The main agent doesn't work, so its view has no such line.
        assert!(
            !rows(&mut t, &full(), 40, 3)
                .iter()
                .any(|row| is_working(row))
        );
        t.apply(Update::Item(item(1, Kind::Text, "Reading.").agent("r1")));
        t.apply(Update::Item(item(2, Kind::Result, "done").agent("r1")));
        // (A content whose call isn't there stands apart, as open.)
        let shown = rows(&mut t, &own, 40, 7);
        assert!(is_working(&shown[5]), "{shown:?}");
        // Selected to the end, it isn't copied.
        assert_eq!(shown[1], "  Reading.");
        let (from, to) = (t.point_at(0, 1).unwrap(), t.point_at(39, 5).unwrap());
        t.select(from, to, &own);
        assert_eq!(
            t.selected_text(&own, &Theme::terminal()).unwrap(),
            "Reading.\n\ndone"
        );
        assert_eq!(t.drawn.key_at(4, 5), None);
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
            let colours = note.map(|y| {
                let at = |x: usize| buffer[(x as u16, y as u16)].fg;
                (at(0), at(MARGIN))
            });
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
                &format!("▎ Keep the old name.{}pending", " ".repeat(33)),
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

    /// Lines of work stack. Anything open stands apart from its neighbours
    /// by a blank line on each side, and the line the reader opened stays
    /// on its row of the screen, so the blank line above it comes from what
    /// is above moving up.
    #[test]
    fn an_open_item_stands_apart_and_its_line_stays_put() {
        use crate::remote::{Act, ActKind};
        let quiet = Filter::Main(View::Quiet);
        let theme = Theme::terminal();
        let mut t = Transcript::new();
        let mut user = item(1, Kind::User, "Go");
        user.turn = Some(1);
        t.apply(Update::Item(user));
        let command = |id: u64, line: &str| {
            let mut call = item(id, Kind::Call, &format!("$ {line}"));
            call.turn = Some(1);
            call.status = Some(CallStatus::Ok);
            call.act = Some(Act {
                kind: ActKind::Command,
                target: line.into(),
                alone: true,
                ..Act::default()
            });
            call
        };
        let mut thinking = item(3, Kind::Thinking, "First this.\nThen that.");
        thinking.turn = Some(1);
        let mut output = item(5, Kind::Result, "ok");
        output.content = Some(ContentKind::Plain);
        output.call = Some(4);
        output.turn = Some(1);
        for item in [
            command(2, "make"),
            thinking,
            command(4, "make test"),
            output,
        ] {
            t.apply(Update::Item(item));
        }
        let closed = [
            "▎ Go",
            "",
            "· Ran make",
            "  First this. …",
            "▸ Ran make test",
            "",
        ];
        assert_eq!(rows(&mut t, &quiet, 40, 6), closed);
        // The thinking opens on its row, a blank line on each side, and the
        // command above moves up.
        t.toggle(3, 3, &quiet, &theme);
        assert_eq!(
            rows(&mut t, &quiet, 40, 6)[..5],
            ["", "· Ran make", "", "  First this.", "  Then that."]
        );
        t.toggle(3, 3, &quiet, &theme);
        assert_eq!(rows(&mut t, &quiet, 40, 6), closed);
        // So does the row.
        t.toggle(4, 4, &quiet, &theme);
        assert_eq!(
            rows(&mut t, &quiet, 40, 6),
            [
                "",
                "· Ran make",
                "  First this. …",
                "",
                "▾ Ran make test",
                "│ ok"
            ]
        );
    }

    /// Scrolled up, the screen stays put as items come, and the marker
    /// counts them. At the bottom again, it follows.
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

    /// A selection copies what the screen shows as it is written, without
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
        call.act = Some(crate::remote::Act {
            kind: ActKind::Command,
            ..Default::default()
        });
        t.apply(Update::Item(call));
        let mut result = item(3, Kind::Result, "a.rs\nb.rs");
        result.content = Some(ContentKind::Plain);
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
        // Part of a line, from its column to the other's.
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
        // Text added to an item keeps it. Another width (lines wrapped
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
    /// and the same margin on both sides, its long path cut in the middle,
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

    /// A drag whose release never came (the terminal lost it) doesn't stop
    /// End from following the bottom.
    #[test]
    fn end_follows_again_after_a_drag_whose_release_never_came() {
        let mut t = Transcript::new();
        for id in 1..=5 {
            t.apply(Update::Item(item(id, Kind::Output, &format!("line {id}"))));
        }
        let filter = full();
        rows(&mut t, &filter, 20, 4);
        t.hold();
        t.follow();
        rows(&mut t, &filter, 20, 4);
        t.apply(Update::Item(item(6, Kind::Output, "line 6")));
        assert_eq!(rows(&mut t, &filter, 20, 4)[3], "  line 6");
        assert!(t.following());
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

        // (A worker's calls are in its own view.)
        let full = rows(&mut t, &Filter::Main(View::Full), 60, 8);
        assert!(
            !full.iter().any(|r| r.contains("r1 cargo test")),
            "{full:?}"
        );
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
        let worker = rows(&mut t, &Filter::Worker("r1".into(), View::Full), 60, 8);
        assert!(worker[0].contains("r1 Test it · running"), "{worker:?}");
        assert!(worker.iter().any(|r| r.contains("Thinking about it")));
        assert!(!worker.iter().any(|r| r.contains("Fix it")));
        // Its own view doesn't name it on its calls.
        assert!(
            worker.iter().any(|r| r.starts_with("· cargo test")),
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

    /// The Quiet view's rows follow from the items alone, so switching to
    /// Full and back while a command runs, and after it ended, shows the
    /// rows a reader who stayed in Quiet saw.
    #[test]
    fn quiet_rows_survive_a_view_switch() {
        use crate::remote::{Act, ActKind};
        let mut t = Transcript::new();
        let call = |id: u64, kind: ActKind, target: &str, status| {
            let mut call = item(id, Kind::Call, target);
            call.turn = Some(1);
            call.status = Some(status);
            call.act = Some(Act {
                kind,
                target: target.into(),
                ..Act::default()
            });
            call
        };
        t.apply(Update::Item(call(
            1,
            ActKind::Command,
            "rg x",
            CallStatus::Ok,
        )));
        t.apply(Update::Item(call(
            2,
            ActKind::Command,
            "cargo build",
            CallStatus::Running,
        )));
        t.apply(Update::Item(call(3, ActKind::Read, "a.rs", CallStatus::Ok)));
        let quiet = Filter::Main(View::Quiet);
        let before = rows(&mut t, &quiet, 50, 4);
        assert_eq!(
            before[..2],
            ["▸ Ran rg x · running cargo build …", "· Read a.rs"]
        );
        rows(&mut t, &Filter::Main(View::Full), 50, 4);
        assert_eq!(rows(&mut t, &quiet, 50, 4), before);
        // It ends in its row, and no row comes or goes.
        t.apply(Update::Change(call(
            2,
            ActKind::Command,
            "cargo build",
            CallStatus::Ok,
        )));
        let after = rows(&mut t, &quiet, 50, 4);
        assert_eq!(after[..2], ["▸ Ran 2 commands (rg, cargo)", "· Read a.rs"]);
        let count = |rows: &[String]| rows.iter().filter(|row| !row.is_empty()).count();
        assert_eq!(count(&before), count(&after));
        rows(&mut t, &Filter::Main(View::Full), 50, 4);
        assert_eq!(rows(&mut t, &quiet, 50, 4), after);
    }

    /// The Messages view counts calls, so while one of the agent's runs,
    /// its line at the end says what the call does, where "Working…" goes.
    #[test]
    fn messages_say_what_a_running_call_does() {
        use crate::remote::{Act, ActKind};
        let mut t = Transcript::new();
        t.working = Some(0);
        let mut user = item(1, Kind::User, "Wait for it");
        user.turn = Some(1);
        t.apply(Update::Item(user));
        let mut wait = item(2, Kind::Call, "waiting for r91");
        wait.turn = Some(1);
        wait.status = Some(CallStatus::Running);
        wait.act = Some(Act {
            kind: ActKind::Wait,
            target: "r91".into(),
            ..Act::default()
        });
        t.apply(Update::Item(wait.clone()));
        let messages = Filter::Main(View::Messages);
        let shown = rows(&mut t, &messages, 50, 8);
        assert_eq!(
            shown
                .iter()
                .filter(|row| row.contains("Waiting for r91…"))
                .count(),
            1,
            "{shown:?}"
        );
        assert!(
            !shown.iter().any(|row| row.contains("Working…")),
            "{shown:?}"
        );
        // Full shows the call itself, and no line at the end.
        let full = rows(&mut t, &Filter::Main(View::Full), 50, 8);
        assert!(
            !full.iter().any(|row| row.contains("Waiting for r91…")),
            "{full:?}"
        );
        // Once it ended, the agent waits on its model again.
        wait.status = Some(CallStatus::Ok);
        t.apply(Update::Change(wait));
        let shown = rows(&mut t, &messages, 50, 8);
        assert!(
            shown.iter().any(|row| row.contains("Working…")),
            "{shown:?}"
        );
        assert!(
            !shown.iter().any(|row| row.contains("Waiting for")),
            "{shown:?}"
        );
    }

    /// An open Quiet row draws again when an item it shows changes, such
    /// as a read of the file it edits, which runs with it.
    #[test]
    fn an_open_quiet_row_follows_its_items() {
        use crate::remote::{Act, ActKind};
        let mut t = Transcript::new();
        let call = |id: u64, text: &str, kind: ActKind, status: CallStatus| {
            let mut call = item(id, Kind::Call, text);
            call.turn = Some(1);
            call.status = Some(status);
            call.act = Some(Act {
                kind,
                target: "a.rs".into(),
                ..Act::default()
            });
            call
        };
        t.apply(Update::Item(call(
            2,
            "edit a.rs",
            ActKind::Edit,
            CallStatus::Ok,
        )));
        t.apply(Update::Item(call(
            3,
            "read a.rs",
            ActKind::Read,
            CallStatus::Running,
        )));
        let quiet = Filter::Main(View::Quiet);
        let theme = Theme::terminal();
        rows(&mut t, &quiet, 50, 4);
        t.toggle(2, 0, &quiet, &theme);
        let shown = rows(&mut t, &quiet, 50, 4);
        assert_eq!(
            shown[..3],
            [
                // (The row's line says it runs.)
                "▾ Read a.rs and made 1 edit …",
                "│ · edit a.rs · ✓",
                "│ · read a.rs"
            ],
            "{shown:?}"
        );
        let mut read = call(3, "read a.rs", ActKind::Read, CallStatus::Ok);
        read.outcome = Some("12 lines".into());
        t.apply(Update::Change(read));
        let shown = rows(&mut t, &quiet, 50, 4);
        assert_eq!(
            shown[..4],
            [
                "▾ Read a.rs and made 1 edit",
                "│ · edit a.rs · ✓",
                "│ · read a.rs · ✓ 12 lines",
                ""
            ],
            "{shown:?}"
        );
    }

    /// A read's code shows in the theme's syntax colours, and a diff's added
    /// line on the success tint to its end, its code in those colours too.
    /// The terminal's own colours show code in the text's colour.
    #[cfg(feature = "remote")]
    #[test]
    fn a_contents_code_shows_in_the_themes_colours() {
        use crate::remote::protocol::HIGHLIGHT_NAMES;
        use crate::tui::theme::{Catalog, Depth};
        let theme = Catalog::load(None).theme("macOS Classic Dark", Depth::Rgb);
        let keyword = HIGHLIGHT_NAMES
            .iter()
            .position(|name| *name == "keyword")
            .unwrap();
        let keyword = theme.syntax(keyword, None).unwrap().fg;
        let mut read = item(2, Kind::Result, "1\tfn main() {}\n2\tlet x = 1;");
        read.content = Some(ContentKind::Lines);
        read.language = Some("rs".into());
        let lines = content_lines(&read, &theme, 40, false);
        let word = |line: &Wrapped, word: &str| {
            line.spans
                .iter()
                .find(|span| span.content == word)
                .unwrap_or_else(|| panic!("{word}: {:?}", line.spans))
                .style
        };
        assert_eq!(word(&lines[0], "fn").fg, keyword);
        assert_eq!(word(&lines[1], "let").fg, keyword);
        let mut diff = item(3, Kind::Result, " 1\tfn a() {}\n+2\tlet y = 2;");
        diff.content = Some(ContentKind::Diff);
        diff.language = Some("rs".into());
        let lines = content_lines(&diff, &theme, 40, false);
        let tint = theme.tint(Tint::Added).unwrap().bg;
        assert!(tint.is_some());
        assert!(
            lines[1].spans.iter().all(|span| span.style.bg == tint),
            "{:?}",
            lines[1].spans
        );
        assert_eq!(spans_width(&lines[1].spans), 40);
        assert!(
            word(&lines[1], "let")
                .fg
                .is_some_and(|fg| Some(fg) != theme.text().fg)
        );
        assert_eq!(
            lines[0]
                .spans
                .iter()
                .find(|span| span.content == "fn")
                .unwrap()
                .style
                .bg,
            None
        );
        let plain = content_lines(&read, &Theme::terminal(), 40, false);
        assert_eq!(
            word(&plain[0], "fn main() {}").fg,
            Theme::terminal().text().fg
        );
    }

    /// A content whose whole is no longer kept says so where its door was,
    /// and isn't a fold.
    #[test]
    fn a_content_no_longer_kept_says_so() {
        let mut t = Transcript::new();
        let mut call = item(1, Kind::Call, "$ seq 40");
        call.status = Some(CallStatus::Ok);
        call.act = Some(crate::remote::Act {
            kind: ActKind::Command,
            ..Default::default()
        });
        t.apply(Update::Item(call));
        let mut output = item(2, Kind::Result, "1\n2");
        output.content = Some(ContentKind::Plain);
        output.more = Some("… 38 more lines".into());
        output.call = Some(1);
        t.apply(Update::Item(output));
        let shown = rows(&mut t, &full(), 40, 4);
        assert_eq!(shown[3], "│ No longer kept", "{shown:?}");
        assert!(!t.item(2).is_some_and(|item| t.folds(item)));
    }

    /// The keyboard reaches each call inside an open Quiet row of several,
    /// its first one too, whose key is the row's, and opens it there.
    #[test]
    fn the_keyboard_opens_the_first_call_of_a_quiet_row() {
        use crate::remote::{Act, ActKind};
        let quiet = Filter::Main(View::Quiet);
        let theme = Theme::terminal();
        let mut t = Transcript::new();
        for (call, path) in [(1, "a.rs"), (3, "b.rs")] {
            let mut read = item(call, Kind::Call, path);
            read.turn = Some(1);
            read.status = Some(CallStatus::Ok);
            read.act = Some(Act {
                kind: ActKind::Read,
                target: path.into(),
                ..Act::default()
            });
            t.apply(Update::Item(read));
            let mut lines = item(call + 1, Kind::Result, "1\tone");
            lines.content = Some(ContentKind::Lines);
            lines.call = Some(call);
            lines.turn = Some(1);
            t.apply(Update::Item(lines));
        }
        rows(&mut t, &quiet, 40, 6);
        assert!(t.move_focus(0, &quiet, &theme));
        assert_eq!(t.focus, Some(Target::Block(1)));
        t.open_focus(Some(true), &quiet, &theme);
        rows(&mut t, &quiet, 40, 6);
        t.move_focus(1, &quiet, &theme);
        assert_eq!(t.focus, Some(Target::Inner { row: 1, item: 1 }));
        t.open_focus(None, &quiet, &theme);
        // (Open, it stands apart from the call after it on the rail.)
        assert_eq!(
            rows(&mut t, &quiet, 40, 6),
            [
                "▾ Read a.rs and b.rs",
                "│ ▾ a.rs · ✓",
                "│   1  one",
                "│",
                "│ ▸ b.rs · ✓",
                ""
            ]
        );
    }

    /// A Quiet row of one call that the reader opened keeps its form. The
    /// next read starts a row of its own, under it and apart from the open
    /// row by a blank line, and the opened row's content stays as it was,
    /// also once it closes and opens again.
    #[test]
    fn an_opened_quiet_row_of_one_call_keeps_its_form() {
        use crate::remote::{Act, ActKind};
        let quiet = Filter::Main(View::Quiet);
        let theme = Theme::terminal();
        let read = |id: u64, path: &str| {
            let mut call = item(id, Kind::Call, path);
            call.turn = Some(1);
            call.status = Some(CallStatus::Ok);
            call.act = Some(Act {
                kind: ActKind::Read,
                target: path.into(),
                ..Act::default()
            });
            call
        };
        let lines = |id: u64, call: u64| {
            let mut result = item(id, Kind::Result, "1\tone\n2\ttwo");
            result.content = Some(ContentKind::Lines);
            result.call = Some(call);
            result.turn = Some(1);
            result
        };
        let mut t = Transcript::new();
        t.apply(Update::Item(read(1, "a.rs")));
        t.apply(Update::Item(lines(2, 1)));
        rows(&mut t, &quiet, 40, 6);
        t.toggle(1, 0, &quiet, &theme);
        let open = rows(&mut t, &quiet, 40, 6);
        assert_eq!(open[..3], ["▾ Read a.rs", "│ 1  one", "│ 2  two"]);
        t.apply(Update::Item(read(3, "b.rs")));
        let shown = rows(&mut t, &quiet, 40, 6);
        assert_eq!(
            shown[..5],
            ["▾ Read a.rs", "│ 1  one", "│ 2  two", "", "· Read b.rs"]
        );
        // Closed again, it still keeps its calls.
        t.toggle(1, 0, &quiet, &theme);
        let closed = rows(&mut t, &quiet, 40, 6);
        assert_eq!(closed[..2], ["▸ Read a.rs", "· Read b.rs"]);
    }

    /// A failure ends its Quiet row, which keeps its ✗, and the command
    /// after it starts a row of its own.
    #[test]
    fn a_failure_ends_its_quiet_row() {
        use crate::remote::{Act, ActKind};
        let call = |id: u64, target: &str, status: CallStatus, ended: Option<u64>| {
            let mut call = item(id, Kind::Call, target);
            call.turn = Some(1);
            call.status = Some(status);
            call.ended = ended;
            call.act = Some(Act {
                kind: ActKind::Command,
                target: target.into(),
                ..Act::default()
            });
            call
        };
        let mut t = Transcript::new();
        for call in [
            call(1, "rg x", CallStatus::Ok, None),
            call(2, "cargo clippy", CallStatus::Failed, Some(3)),
            call(3, "sed -i x", CallStatus::Ok, None),
        ] {
            t.apply(Update::Item(call));
        }
        let shown = rows(&mut t, &Filter::Main(View::Quiet), 50, 3);
        assert!(
            shown[0].starts_with("▸ Ran 2 commands (rg, cargo)"),
            "{shown:?}"
        );
        assert!(shown[0].ends_with('✗'), "{shown:?}");
        assert_eq!(shown[1], "· Ran sed -i x", "{shown:?}");
    }

    /// An open Quiet row shows what its calls hold on a rail under its
    /// line, and no line twice. A row of one call has its content right
    /// under the line, and a row of several shows each call's Full line,
    /// which opens to its content under it, apart from its neighbours by a
    /// blank rail line, and the thinking between its calls.
    #[test]
    fn an_open_quiet_row_shows_what_its_calls_hold() {
        use crate::remote::{Act, ActKind};
        let call = |id: u64, kind: ActKind, target: &str, outcome: &str| {
            let mut call = item(id, Kind::Call, target);
            call.turn = Some(1);
            call.status = Some(CallStatus::Ok);
            call.outcome = Some(outcome.into());
            call.act = Some(Act {
                kind,
                target: target.into(),
                ..Act::default()
            });
            call
        };
        let output = |id: u64, call: u64, text: &str| {
            let mut result = item(id, Kind::Result, text);
            result.content = Some(ContentKind::Plain);
            result.call = Some(call);
            result.turn = Some(1);
            result
        };
        let quiet = Filter::Main(View::Quiet);
        let theme = Theme::terminal();
        let opened = |calls: Vec<Item>, height: u16| {
            let mut t = Transcript::new();
            for call in calls {
                t.apply(Update::Item(call));
            }
            rows(&mut t, &quiet, 40, height);
            t.toggle(1, 0, &quiet, &theme);
            rows(&mut t, &quiet, 40, height)
        };
        let build = || call(1, ActKind::Command, "cargo build", "2.0 s");
        assert_eq!(
            opened(vec![build(), output(2, 1, "Compiling arko\nFinished")], 5),
            [
                "▾ Ran cargo build",
                "│ Compiling arko",
                "│ Finished",
                "",
                ""
            ]
        );
        let two = vec![
            build(),
            output(2, 1, "Compiling arko\nFinished"),
            call(3, ActKind::Command, "rg x", "0.1 s"),
            output(4, 3, "a.rs:1: x"),
        ];
        assert_eq!(
            opened(two.clone(), 5),
            [
                "▾ Ran 2 commands (cargo, rg)",
                "│ ▸ cargo build · ✓ 2.0 s",
                "│ ▸ rg x · ✓ 0.1 s",
                "",
                ""
            ]
        );
        // Its first call opens to its content, under its line.
        let mut t = Transcript::new();
        for item in two {
            t.apply(Update::Item(item));
        }
        rows(&mut t, &quiet, 40, 6);
        t.toggle(1, 0, &quiet, &theme);
        rows(&mut t, &quiet, 40, 6);
        t.toggle(1, 1, &quiet, &theme);
        assert_eq!(
            rows(&mut t, &quiet, 40, 7),
            [
                "▾ Ran 2 commands (cargo, rg)",
                "│ ▾ cargo build · ✓ 2.0 s",
                "│   Compiling arko",
                "│   Finished",
                "│",
                "│ ▸ rg x · ✓ 0.1 s",
                ""
            ]
        );
        // The thinking between its calls shows in their text column, with
        // no label or marker.
        let mut thinking = item(2, Kind::Thinking, "Which one?");
        thinking.turn = Some(1);
        assert_eq!(
            opened(
                vec![
                    call(1, ActKind::Read, "a.rs", "3 lines"),
                    thinking,
                    call(3, ActKind::Read, "b.rs", "4 lines"),
                ],
                5
            ),
            [
                "▾ Read a.rs and b.rs",
                "│ · a.rs · ✓ 3 lines",
                "│   Which one?",
                "│ · b.rs · ✓ 4 lines",
                ""
            ]
        );
    }

    /// The call of `fixture` and its content's block, as the feed makes
    /// them.
    fn fixture_items(fixture: &crate::remote::quiet::Fixture) -> Vec<Item> {
        let mut call = item(1, Kind::Call, fixture.full);
        call.turn = Some(1);
        call.status = Some(fixture.status);
        call.outcome = Some(fixture.outcome.to_string()).filter(|outcome| !outcome.is_empty());
        call.act = Some(fixture.act.clone());
        call.detail = fixture.full_detail.map(str::to_string);
        call.job = fixture.job.then(|| "4d2c1a9b".to_string());
        call.ended = fixture.ended;
        let mut items = vec![call];
        if let Some((kind, preview)) = fixture.content {
            let mut result = item(2, Kind::Result, preview);
            result.content = Some(kind);
            result.more = fixture.more.map(str::to_string);
            // (The whole, which the feed keeps when the preview leaves some out.)
            result.whole = fixture.more.map(|_| Arc::from(preview));
            result.call = Some(1);
            result.turn = Some(1);
            items.push(result);
        }
        items
    }

    /// A content's first line as it's drawn behind the bar, its number
    /// muted and aligned, a diff's mark after it.
    fn first_content_row(kind: crate::remote::ContentKind, preview: &str) -> String {
        let numbers = content::number_width(kind, preview);
        let first = preview.split('\n').next().unwrap_or_default();
        let shown = match content::line(kind, first) {
            content::Line::Numbered { number, mark, text } => match mark {
                content::Mark::None => format!("{number:>numbers$}  {text}"),
                _ => format!("{number:>numbers$} {} {text}", mark.symbol()),
            },
            content::Line::Plain(text) => text.to_string(),
        };
        format!("│ {shown}").trim_end().to_string()
    }

    /// Each of the shared fixtures, alone in its Quiet row, says what it did
    /// with one ✗ at the row's right when it failed, and opens to its detail
    /// and its content right under its line, with the door to the rest. A
    /// row with nothing to show has a dot for a fold. In Full its line opens to the
    /// same content, a command's open from the start.
    #[test]
    fn quiet_rows_open_to_the_fixtures_contents() {
        let quiet = Filter::Main(View::Quiet);
        let theme = Theme::terminal();
        for fixture in crate::remote::quiet::fixtures() {
            let name = fixture.name;
            let mut t = Transcript::new();
            for item in fixture_items(&fixture) {
                t.apply(Update::Item(item));
            }
            let closed = rows(&mut t, &quiet, 220, 40);
            if fixture.failed {
                assert!(closed[0].ends_with('✗'), "{name}: {closed:?}");
                assert_eq!(closed[0].chars().count(), 220, "{name}");
            }
            t.toggle(1, 0, &quiet, &theme);
            let open = rows(&mut t, &quiet, 220, 40);
            if !fixture.opens {
                assert_eq!(open, closed, "{name}");
                assert!(closed[0].starts_with("· "), "{name}: {closed:?}");
                continue;
            }
            assert!(closed[0].starts_with("▸ "), "{name}: {closed:?}");
            assert!(open[0].starts_with("▾ "), "{name}: {open:?}");
            let end = open.iter().position(String::is_empty).unwrap_or(open.len());
            let mut expected: Vec<String> = fixture
                .act
                .detail
                .split('\n')
                .filter(|_| !fixture.act.detail.is_empty())
                .map(|line| format!("│ {line}"))
                .collect();
            if let Some((kind, preview)) = fixture.content {
                expected.push(first_content_row(kind, preview));
            }
            assert_eq!(open[1..=expected.len()], expected, "{name}: {open:?}");
            if let Some(more) = fixture.more {
                assert_eq!(open[end - 1], format!("│ {more}"), "{name}");
            }
            // Full, where the call's line opens to the same content.
            let full = Filter::Main(View::Full);
            let lines = rows(&mut t, &full, 220, 40);
            let command = fixture.act.kind == crate::remote::ActKind::Command;
            match fixture.content {
                Some((kind, preview)) if command => {
                    assert!(lines[0].starts_with("▾ "), "{name}: {lines:?}");
                    assert!(lines.contains(&first_content_row(kind, preview)), "{name}");
                }
                Some(_) => assert!(lines[0].starts_with("▸ "), "{name}: {lines:?}"),
                None if fixture.full_detail.is_none() => {
                    assert!(lines[0].starts_with("· "), "{name}: {lines:?}");
                }
                None => {}
            }
        }
    }

    /// How long a draw of the Quiet view takes at the transcript's cap of
    /// calls, against Full (`cargo test --release --lib quiet_draw_cost --
    /// --ignored --nocapture`).
    #[test]
    #[ignore]
    fn quiet_draw_cost() {
        use crate::remote::{Act, ActKind};
        let mut t = Transcript::new();
        let kinds = [
            (ActKind::Read, "src/tui/app.rs"),
            (ActKind::Edit, "src/tui/app.rs"),
            (ActKind::Command, "cargo test -p arko-tui"),
            (ActKind::Search, "arko"),
            (ActKind::Read, "src/remote/feed.rs"),
        ];
        for id in 1..=KEPT as u64 {
            let (kind, target) = kinds[id as usize % kinds.len()];
            let mut call = item(id, Kind::Call, target);
            call.turn = Some(id / 40);
            call.status = Some(CallStatus::Ok);
            call.act = Some(Act {
                kind,
                target: target.into(),
                ..Act::default()
            });
            if id % 25 == 0 {
                call = item(id, Kind::Text, "A paragraph of the agent's.");
            }
            t.apply(Update::Item(call));
        }
        for view in [View::Full, View::Quiet] {
            let filter = Filter::Main(view);
            rows(&mut t, &filter, 100, 50);
            let start = Instant::now();
            for _ in 0..20 {
                rows(&mut t, &filter, 100, 50);
            }
            println!("{view:?}: {:?} a draw", start.elapsed() / 20);
        }
    }

    /// A worker's own view is the selected view over its own items, as the
    /// main view is over the main agent's. Messages shows its text, messages
    /// and replies and counts its calls, Quiet leaves out the call that
    /// started a worker, and Full shows everything.
    #[test]
    fn a_workers_own_view_follows_the_selected_view() {
        let build = |agent: &str| {
            let mut t = Transcript::new();
            let mut user = item(1, Kind::User, "Use tabs.").agent(agent);
            user.turn = Some(1);
            t.apply(Update::Item(user));
            t.apply(Update::Item(item(2, Kind::Thinking, "Hmm.").agent(agent)));
            t.apply(Update::Item(item(3, Kind::Text, "On it.").agent(agent)));
            for (id, text, started) in [
                (4, "cargo test", None),
                (5, "start worker “Count”", Some("r2")),
            ] {
                let mut call = item(id, Kind::Call, text).agent(agent);
                call.turn = Some(1);
                call.status = Some(CallStatus::Ok);
                call.started = started.map(str::to_string);
                t.apply(Update::Item(call));
            }
            let mut line = item(6, Kind::WorkerLine, "Count").agent(agent);
            line.worker = Some("r2".into());
            t.apply(Update::Item(line));
            t.apply(Update::Item(
                item(7, Kind::Message, "Found it.").agent(agent),
            ));
            t.apply(Update::Item(item(8, Kind::Reply, "Fixed.").agent(agent)));
            t.apply(Update::Worker(WorkerSummary {
                title: "Count".into(),
                state: WorkerState::Running,
                ..WorkerSummary::new("r2")
            }));
            t.apply(Update::Worker(WorkerSummary {
                title: "Fix".into(),
                state: WorkerState::Running,
                ..WorkerSummary::new("r1")
            }));
            t
        };
        // The same items of the main agent. What the view shows of them is
        // what it shows of the worker's, less the worker's heading.
        let mut own = build("r1");
        let mut main = build(MAIN_AGENT);
        for view in View::ALL {
            let worker = rows(&mut own, &Filter::Worker("r1".into(), view), 60, 14);
            let main = rows(&mut main, &Filter::Main(view), 60, 14);
            let body = |rows: &[String]| {
                rows.iter()
                    .map(|r| {
                        r.trim()
                            .trim_start_matches(['▎', '✉', '▸'])
                            .trim()
                            .to_string()
                    })
                    .filter(|r| !r.is_empty() && !r.starts_with("r1 Fix"))
                    .collect::<Vec<_>>()
            };
            assert_eq!(body(&worker), body(&main), "{view:?}");
        }
        let has = |rows: &[String], text: &str| rows.iter().any(|r| r.contains(text));
        let full = rows(&mut own, &Filter::Worker("r1".into(), View::Full), 60, 14);
        assert!(has(&full, "Hmm.") && has(&full, "cargo test"), "{full:?}");
        assert!(has(&full, "start worker “Count”"), "{full:?}");
        let quiet = rows(&mut own, &Filter::Worker("r1".into(), View::Quiet), 60, 14);
        assert!(
            has(&quiet, "cargo test") && has(&quiet, "r2 Count"),
            "{quiet:?}"
        );
        assert!(!has(&quiet, "start worker “Count”"), "{quiet:?}");
        let messages = rows(
            &mut own,
            &Filter::Worker("r1".into(), View::Messages),
            60,
            14,
        );
        for text in [
            "Use tabs.",
            "On it.",
            "Found it.",
            "Fixed.",
            "▸ 2 tool calls",
        ] {
            assert!(has(&messages, text), "{text}: {messages:?}");
        }
        for text in ["Hmm.", "cargo test"] {
            assert!(!has(&messages, text), "{text}: {messages:?}");
        }
        // The view in the filter is the selected one, so a switch shows at once.
        assert_eq!(
            rows(&mut own, &Filter::Worker("r1".into(), View::Full), 60, 14),
            full
        );
    }

    /// A call that started its worker is left out of the quiet view, where
    /// the worker's own line stands for it. One that is running, refused or
    /// queued shows, and the other views keep every call.
    #[test]
    fn quiet_leaves_out_the_call_that_started_a_worker() {
        let mut t = Transcript::new();
        let mut user = item(1, Kind::User, "Fix it");
        user.turn = Some(1);
        t.apply(Update::Item(user));
        let start = |id: u64, text: &str, status: CallStatus, started: Option<&str>| {
            let mut call = item(id, Kind::Call, text);
            call.turn = Some(1);
            call.status = Some(status);
            call.started = started.map(str::to_string);
            call
        };
        t.apply(Update::Item(start(
            2,
            "start worker “Fix flaky test”",
            CallStatus::Ok,
            Some("r1"),
        )));
        let mut line = item(3, Kind::WorkerLine, "Fix flaky test");
        line.worker = Some("r1".into());
        line.turn = Some(1);
        t.apply(Update::Item(line));
        t.apply(Update::Item(start(
            4,
            "start worker “Update docs” queued",
            CallStatus::Ok,
            None,
        )));
        t.apply(Update::Item(start(
            5,
            "start worker “Run migration” refused",
            CallStatus::Failed,
            None,
        )));
        t.apply(Update::Item(start(
            6,
            "start worker “Waiting for billing”",
            CallStatus::Running,
            None,
        )));
        t.apply(Update::Worker(WorkerSummary {
            title: "Fix flaky test".into(),
            state: WorkerState::Running,
            tool_calls: 3,
            ..WorkerSummary::new("r1")
        }));
        let quiet = rows(&mut t, &Filter::Main(View::Quiet), 70, 12);
        let count = |rows: &[String], text: &str| rows.iter().filter(|r| r.contains(text)).count();
        assert_eq!(count(&quiet, "Fix flaky test"), 1, "{quiet:?}");
        assert_eq!(count(&quiet, "r1 Fix flaky test · 3 tool calls"), 1);
        assert_eq!(count(&quiet, "queued"), 1, "{quiet:?}");
        assert_eq!(count(&quiet, "refused"), 1, "{quiet:?}");
        assert_eq!(count(&quiet, "Waiting for billing"), 1, "{quiet:?}");
        let full = rows(&mut t, &Filter::Main(View::Full), 70, 14);
        assert_eq!(count(&full, "start worker “Fix flaky test”"), 1, "{full:?}");
        assert_eq!(count(&full, "r1 Fix flaky test · 3 tool calls"), 1);
        let messages = rows(&mut t, &Filter::Main(View::Messages), 70, 12);
        assert!(
            messages.iter().any(|r| r.contains("▸ 4 tool calls")),
            "{messages:?}"
        );
        // The worker's own view keeps its lines of the workers it started.
        let mut nested = start(7, "start worker “Count”", CallStatus::Ok, Some("r2"));
        nested = nested.agent("r1");
        t.apply(Update::Item(nested));
        let own = rows(&mut t, &Filter::Worker("r1".into(), View::Full), 70, 6);
        assert_eq!(count(&own, "start worker “Count”"), 1, "{own:?}");
    }

    /// A worker's line names its setup when it isn't the usual one, with its
    /// profile, or its model when workers start on another, and the effort
    /// when that is another too.
    #[test]
    fn a_worker_line_names_its_setup_only_when_it_isnt_the_usual_one() {
        let usual = Usual {
            model: "gpt-6-luna".into(),
            effort: "high".into(),
        };
        let worker =
            |model: Option<&str>, effort: Option<&str>, profile: Option<&str>| WorkerSummary {
                title: "Fix flaky test".into(),
                state: WorkerState::Running,
                tool_calls: 3,
                model: model.map(str::to_string),
                effort: effort.map(str::to_string),
                profile: profile.map(str::to_string),
                ..WorkerSummary::new("r5")
            };
        let tag = |worker: &WorkerSummary| setup(worker, &usual);
        assert_eq!(tag(&worker(None, None, None)), None);
        assert_eq!(tag(&worker(Some("gpt-6-luna"), Some("high"), None)), None);
        assert_eq!(
            tag(&worker(
                Some("gpt-6-luna"),
                Some("high"),
                Some("impl-light")
            )),
            Some("impl-light".to_string())
        );
        assert_eq!(
            tag(&worker(Some("gpt-6.1-sol"), Some("high"), None)),
            Some("sol".to_string())
        );
        assert_eq!(
            tag(&worker(Some("gpt-6.1-sol"), Some("xhigh"), None)),
            Some("sol xhigh".to_string())
        );
        assert_eq!(
            tag(&worker(Some("gpt-6-luna"), Some("low"), None)),
            Some("low effort".to_string())
        );
        let mut t = Transcript::new();
        t.usual = usual;
        let mut line = item(1, Kind::WorkerLine, "Fix flaky test");
        line.worker = Some("r5".into());
        line.turn = Some(1);
        t.apply(Update::Item(line));
        let quiet = Filter::Main(View::Quiet);
        t.apply(Update::Worker(worker(
            Some("gpt-6-luna"),
            Some("high"),
            None,
        )));
        let rows_of = rows(&mut t, &quiet, 70, 3);
        assert!(
            rows_of
                .iter()
                .any(|r| r == "  r5 Fix flaky test · 3 tool calls · running"),
            "{rows_of:?}"
        );
        t.apply(Update::Worker(worker(None, None, Some("impl-light"))));
        let rows_of = rows(&mut t, &quiet, 70, 3);
        assert!(
            rows_of
                .iter()
                .any(|r| r == "  r5 Fix flaky test · impl-light · 3 tool calls · running"),
            "{rows_of:?}"
        );
    }

    /// A long task is cut short, so the setup, the time and the state stay on
    /// the line.
    #[test]
    fn a_long_task_gives_way_to_the_state_on_a_workers_line() {
        let mut t = Transcript::new();
        let mut line = item(1, Kind::WorkerLine, "Fix flaky test");
        line.worker = Some("r5".into());
        line.turn = Some(1);
        t.apply(Update::Item(line));
        t.apply(Update::Worker(WorkerSummary {
            title:
                "Fix the flaky test in tests/sync.rs by waiting for the lock instead of sleeping"
                    .into(),
            state: WorkerState::Running,
            tool_calls: 3,
            profile: Some("impl-light".into()),
            last: Some("cargo test".into()),
            ..WorkerSummary::new("r5")
        }));
        let quiet = Filter::Main(View::Quiet);
        let rows_of = rows(&mut t, &quiet, 70, 3);
        assert_eq!(
            rows_of[0], "  r5 Fix the flaky test in test… · impl-light · 3 tool calls · running",
            "{rows_of:?}"
        );
        assert_eq!(text_width(&rows_of[0]), 70);
        // A wide screen has room for the whole task and the last call.
        let rows_of = rows(&mut t, &quiet, 140, 3);
        assert_eq!(
            rows_of[0],
            "  r5 Fix the flaky test in tests/sync.rs by waiting for the lock instead of sleeping · impl-light · 3 tool calls · running · cargo test"
        );
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
        let own = rows(&mut t, &Filter::Worker("r1".into(), View::Full), 70, 10);
        assert!(
            own.iter().any(|r| r.contains("r2 Count the files")),
            "{own:?}"
        );
    }

    /// A worker's stop or failure shows in its own view, and the main view
    /// leaves it to the worker's line, which says how the worker ended.
    #[test]
    fn a_workers_notice_shows_in_its_own_view() {
        let mut t = Transcript::new();
        let mut user = item(1, Kind::User, "Review it");
        user.turn = Some(1);
        t.apply(Update::Item(user));
        let mut stopped =
            item(2, Kind::Notice, "stopped (the user stopped the worker)").agent("r1");
        stopped.level = Some(Level::Info);
        stopped.stop = true;
        stopped.turn = Some(1);
        t.apply(Update::Item(stopped));
        let mut failed = item(3, Kind::Notice, "failed (boom)").agent("r1");
        failed.level = Some(Level::Error);
        failed.turn = Some(1);
        t.apply(Update::Item(failed));
        for view in View::ALL {
            let main = rows(&mut t, &Filter::Main(view), 70, 4);
            assert_eq!(main[..2], ["▎ Review it", ""], "{view:?}");
            assert!(
                !main
                    .iter()
                    .any(|r| r.contains("stopped") || r.contains("boom"))
            );
        }
        let own = rows(&mut t, &Filter::Worker("r1".into(), View::Full), 70, 4);
        assert!(
            own.iter()
                .any(|r| r.contains("stopped (the user stopped the worker)")),
            "{own:?}"
        );
        assert!(own.iter().any(|r| r.contains("failed (boom)")), "{own:?}");
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
        let own = rows(&mut t, &Filter::Worker("r1".into(), View::Full), 60, 4);
        assert!(own[0].contains("r1 Test it · finished · 2h 5m"), "{own:?}");
    }

    /// A view that followed every update of a random session shows what a
    /// view that starts over from the session's snapshot shows, in the same
    /// order, within what the feed keeps.
    #[test]
    fn a_view_that_starts_over_shows_what_one_that_followed_does() {
        use crate::remote::Received;
        use crate::testing::{FeedMirror, RandomSession};
        for seed in 0..8 {
            let mut session = RandomSession::new(seed);
            let feed = session.feed().clone();
            let (start, mut updates) = feed.follow();
            let mut followed = Transcript::new();
            followed.reset(start);
            for step in 0..=800 {
                // (At the end, the time passes for which the feed holds the
                // workers' summaries back.)
                if step == 800 {
                    session.settle();
                } else {
                    session.step();
                }
                // (It takes them after each call, so it never falls behind.)
                while let Some(received) = updates.try_next() {
                    match received {
                        Received::Update(update) => followed.apply(update),
                        Received::Reset(_) => panic!("seed {seed}, step {step}: behind"),
                    }
                }
            }
            let (snapshot, _) = feed.subscribe();
            let mut restarted = Transcript::new();
            restarted.reset(snapshot.clone());
            let shown = |t: &Transcript| {
                FeedMirror {
                    items: t
                        .entries
                        .iter()
                        .map(|entry| (entry.item.id, entry.item.clone()))
                        .collect(),
                    workers: t.workers.clone(),
                    ..FeedMirror::default()
                }
                .within(&snapshot)
            };
            let (a, b) = (shown(&followed), shown(&restarted));
            assert_eq!(a.difference(&b), None, "seed {seed}");
            let order = |t: &Transcript| -> Vec<u64> {
                t.entries
                    .iter()
                    .map(|entry| entry.item.id)
                    .filter(|id| b.items.contains_key(id))
                    .collect()
            };
            assert_eq!(order(&followed), order(&restarted), "seed {seed}");
        }
    }
}

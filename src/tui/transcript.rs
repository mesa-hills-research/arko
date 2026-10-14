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

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::markdown;
use super::theme::Theme;
use super::wrap::{self, width as text_width};
use crate::glyphs;
use crate::remote::{
    CallStatus, Delivery, Item, Kind, Level, Snapshot, Source, Update, WorkerState, WorkerSummary,
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

/// An item, with its lines as last drawn.
struct Entry {
    item: Item,
    cache: Option<Cached>,
}

struct Cached {
    /// What the lines were drawn for.
    width: usize,
    style: u64,
    lines: Vec<Line<'static>>,
}

/// What a block of the drawn transcript is.
#[derive(Debug, Clone, PartialEq)]
enum Shown {
    /// An entry's lines.
    Entry(usize),
    /// A worker's summary, where it started (`worker_line`).
    Worker(String),
    /// A worker view's heading: the worker's task.
    Heading(String),
    /// A turn's tool activity, counted (the Messages view).
    Counter { calls: usize, failed: usize },
}

/// A block of lines in the drawn transcript.
struct Block {
    /// The ID of its (first) item: where the reading position anchors.
    key: u64,
    shown: Shown,
    /// A blank line before it.
    gap: bool,
    lines: usize,
}

impl Block {
    fn height(&self) -> usize {
        self.lines + usize::from(self.gap)
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

/// What was drawn where, for the mouse.
#[derive(Debug, Clone, Default)]
pub struct Drawn {
    /// The transcript's area.
    pub area: Rect,
    /// The key of the block on each row of the area.
    rows: Vec<Option<u64>>,
    /// The "new" marker's area, while it shows.
    pub marker: Option<Rect>,
}

impl Drawn {
    /// The block on the screen's row `y`.
    pub fn key_at(&self, x: u16, y: u16) -> Option<u64> {
        if !self.area.contains(ratatui::layout::Position { x, y }) {
            return None;
        }
        self.rows.get((y - self.area.y) as usize).copied().flatten()
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
    /// The selected item (clicked): it is highlighted, shows its details,
    /// and is what a copy copies.
    pub selected: Option<u64>,
    /// The lines drawn last, by row.
    pub drawn: Drawn,
    /// Lines a scroll moved while the screen was not drawn yet.
    pending_scroll: i64,
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
            selected: None,
            drawn: Drawn::default(),
            pending_scroll: 0,
        }
    }

    /// Starts over from the feed's `snapshot` (when a client starts, or
    /// fell behind): the reading position and selection stay on their
    /// items.
    pub fn reset(&mut self, snapshot: Snapshot) {
        self.entries = snapshot
            .items
            .into_iter()
            .map(|item| Entry { item, cache: None })
            .collect();
        self.workers = snapshot.workers;
        self.trim();
    }

    /// Takes the feed's `update`.
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Item(item) => {
                self.entries.push_back(Entry { item, cache: None });
                self.trim();
            }
            Update::Append { id, text } => {
                if let Some(entry) = self.find_mut(id) {
                    entry.item.text.push_str(&text);
                    entry.cache = None;
                }
            }
            Update::Change(item) => {
                if let Some(entry) = self.find_mut(item.id) {
                    entry.item = item;
                    entry.cache = None;
                }
            }
            Update::Worker(summary) => match self.workers.iter_mut().find(|w| w.id == summary.id) {
                Some(worker) => *worker = summary,
                None => self.workers.push(summary),
            },
            Update::Activity => {}
        }
    }

    /// Keeps as many items as the feed does, the main agent's and the
    /// workers' each.
    fn trim(&mut self) {
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

    /// Scrolls to the top at the next draw.
    pub fn top(&mut self) {
        self.pending_scroll = i64::MIN / 2;
    }

    /// The text to copy of the item `id`: as written, without the view's
    /// margins or line breaks.
    pub fn copy_text(&self, id: u64) -> Option<String> {
        let item = self.item(id)?;
        if item.kind == Kind::WorkerLine {
            let worker = item.worker.as_deref().and_then(|w| self.worker(w))?;
            return Some(worker_text(worker));
        }
        let mut text = item.text.clone();
        if let Some(outcome) = &item.outcome {
            text.push_str(&format!(" · {outcome}"));
        }
        if let Some(detail) = &item.detail {
            text.push('\n');
            text.push_str(detail);
        }
        Some(text)
    }

    /// The blocks of `filter`'s view of the items, at `width`.
    fn blocks(&mut self, filter: &Filter, theme: &Theme, width: usize) -> Vec<Block> {
        let style = theme_key(theme) ^ self.selected.unwrap_or(0).rotate_left(17);
        let mut blocks: Vec<Block> = Vec::new();
        if let Filter::Worker(id) = filter
            && let Some(worker) = self.worker(id)
        {
            blocks.push(Block {
                key: 0,
                shown: Shown::Heading(worker.id.clone()),
                gap: false,
                lines: heading_lines(worker, theme, width).len(),
            });
        }
        // The Messages view counts each turn's tool activity on one line,
        // where the activity began.
        let mut counters: HashMap<Option<u64>, usize> = HashMap::new();
        let mut last_group: Option<Group> = blocks.first().map(|_| Group::Prose);
        for index in self.order() {
            let item = &self.entries[index].item;
            let shown = match filter.show(item) {
                Show::Hidden => continue,
                Show::Count => {
                    let turn = item.turn;
                    let failed = item.status == Some(CallStatus::Failed);
                    let calls = usize::from(item.kind == Kind::Call);
                    match counters.get(&turn) {
                        Some(&at) => {
                            if let Shown::Counter {
                                calls: count,
                                failed: failures,
                            } = &mut blocks[at].shown
                            {
                                *count += calls;
                                *failures += usize::from(failed);
                            }
                            continue;
                        }
                        None => {
                            counters.insert(turn, blocks.len());
                            Shown::Counter {
                                calls,
                                failed: usize::from(failed),
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
                    let selected = self.selected == Some(self.entries[*index].item.id);
                    let entry = &mut self.entries[*index];
                    let fresh = entry
                        .cache
                        .as_ref()
                        .is_some_and(|c| c.width == width && c.style == style);
                    if !fresh {
                        entry.cache = Some(Cached {
                            width,
                            style,
                            lines: entry_lines(&entry.item, theme, width, selected),
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
            });
        }
        blocks
    }

    /// The entries in the order they are drawn: by ID, except that a
    /// result's block comes right under its call (other items, such as a
    /// worker's, may have come between them).
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
        for (index, entry) in self.entries.iter().enumerate() {
            let placed = entry.item.kind == Kind::Result
                && entry
                    .item
                    .call
                    .is_some_and(|call| under.contains_key(&call));
            if placed {
                continue;
            }
            order.push(index);
            if let Some(results) = under.get(&entry.item.id) {
                order.extend(results);
            }
        }
        order
    }

    /// Draws `filter`'s view into `area` of `buffer`.
    pub fn draw(&mut self, buffer: &mut Buffer, area: Rect, filter: &Filter, theme: &Theme) {
        let width = area.width as usize;
        let height = area.height as usize;
        let blocks = self.blocks(filter, theme, width);
        let total: usize = blocks.iter().map(Block::height).sum();
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
        if self.pending_scroll != 0 {
            top = (top as i64 + self.pending_scroll).clamp(0, bottom as i64) as usize;
            self.pending_scroll = 0;
        }
        top = top.min(bottom);
        if top >= bottom {
            // At the bottom: following again.
            if self.anchor.is_some() {
                self.anchor = None;
            }
        } else {
            let index = starts
                .partition_point(|start| *start <= top)
                .saturating_sub(1);
            let block = &blocks[index];
            // Following stopped (or the view changed): what comes from
            // here on is new.
            if self
                .baseline
                .as_ref()
                .is_none_or(|(view, _)| view != filter)
            {
                self.baseline = Some((filter.clone(), total));
            }
            self.anchor = Some(Anchor {
                key: block.key,
                offset: top - starts[index],
                height: block.height(),
            });
        }
        if self.anchor.is_none() {
            self.baseline = None;
        }

        // The lines on the screen.
        buffer.set_style(area, theme.base());
        let mut rows: Vec<Option<u64>> = vec![None; height];
        let first = starts
            .partition_point(|start| *start <= top)
            .saturating_sub(1);
        let mut y = 0usize;
        let mut skip = top.saturating_sub(starts.get(first).copied().unwrap_or(0));
        for block in blocks.iter().skip(first) {
            if y >= height {
                break;
            }
            let lines = self.block_lines(block, theme, width);
            let selected = match &block.shown {
                Shown::Entry(index) => self.selected == Some(self.entries[*index].item.id),
                _ => false,
            };
            let mut rendered: Vec<Option<&Line>> = Vec::new();
            if block.gap {
                rendered.push(None);
            }
            rendered.extend(lines.iter().map(Some));
            for line in rendered.into_iter().skip(skip) {
                if y >= height {
                    break;
                }
                let row = area.y + y as u16;
                if let Some(line) = line {
                    if selected && !theme.is_terminal() {
                        buffer.set_style(
                            Rect::new(area.x, row, area.width, 1),
                            Style::new().bg(theme.selection),
                        );
                    }
                    buffer.set_line(area.x, row, line, area.width);
                    rows[y] = Some(block.key);
                }
                y += 1;
            }
            skip = 0;
        }

        // The marker of what is new below: the lines that came since.
        let new = self
            .baseline
            .as_ref()
            .map_or(0, |(_, then)| total.saturating_sub(*then));
        self.drawn = Drawn {
            area,
            rows,
            marker: None,
        };
        if self.anchor.is_some() && area.height > 0 {
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

    /// A block's lines (from the cache for an entry).
    fn block_lines(&self, block: &Block, theme: &Theme, width: usize) -> Vec<Line<'static>> {
        match &block.shown {
            Shown::Entry(index) => self.entries[*index]
                .cache
                .as_ref()
                .map(|c| c.lines.clone())
                .unwrap_or_default(),
            Shown::Worker(id) => vec![worker_line(self.worker(id), id, theme, width)],
            Shown::Heading(id) => self
                .worker(id)
                .map(|worker| heading_lines(worker, theme, width))
                .unwrap_or_default(),
            Shown::Counter { calls, failed } => {
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
                    spans.push(Span::styled(format!(" · {failed} failed"), theme.error()));
                }
                vec![Line::from(spans)]
            }
        }
    }
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
            Filter::Worker(id) => {
                if &item.agent == id && item.kind != Kind::WorkerLine {
                    Show::Item
                } else {
                    Show::Hidden
                }
            }
            Filter::Main(view) => {
                if item.kind == Kind::WorkerLine {
                    return worker_line();
                }
                if !item.is_main() {
                    // A worker's calls and what went wrong show in Full;
                    // its text and report are its own view's.
                    return match (view, item.kind) {
                        (View::Full, Kind::Call | Kind::Result) => Show::Item,
                        (View::Full, Kind::Notice) if item.level != Some(Level::Info) => Show::Item,
                        _ => Show::Hidden,
                    };
                }
                match view {
                    View::Full | View::Quiet => Show::Item,
                    View::Messages => match item.kind {
                        Kind::User
                        | Kind::Message
                        | Kind::Question
                        | Kind::Reply
                        | Kind::Output
                        | Kind::Turn => Show::Item,
                        Kind::Notice if item.level != Some(Level::Info) => Show::Item,
                        Kind::Call | Kind::Result | Kind::Text | Kind::Notice => Show::Count,
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
        Kind::Notice => Group::Activity,
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

/// An item's lines at `width` (margins included).
fn entry_lines(item: &Item, theme: &Theme, width: usize, selected: bool) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(MARGIN).max(1);
    let blank = || Span::raw(" ".repeat(MARGIN));
    let mark = |symbol: &str, style: Style| {
        let symbol = glyphs::render(symbol).into_owned();
        let pad = MARGIN.saturating_sub(text_width(&symbol));
        Span::styled(format!("{symbol}{}", " ".repeat(pad)), style)
    };
    let worker_prefix = |item: &Item| -> Vec<Span<'static>> {
        if item.is_main() {
            Vec::new()
        } else {
            vec![Span::styled(
                format!("{} ", item.agent),
                theme.dim().add_modifier(Modifier::BOLD),
            )]
        }
    };
    // The selected item's bar, in the margin.
    let bar = |default: Span<'static>| {
        if selected {
            mark("▎", theme.accent())
        } else {
            default
        }
    };
    match item.kind {
        Kind::User => {
            let mut lines = plain(&item.text, theme.text(), inner);
            if let Some(tag) = user_tag(item) {
                let tag = Span::styled(format!("  {tag}"), theme.dim());
                match lines.last_mut() {
                    Some(last) if spans_width(last) + text_width(&tag.content) <= inner => {
                        last.push(tag)
                    }
                    _ => lines.push(vec![tag]),
                }
            }
            wrap::prefixed(lines, mark("▎", theme.accent()), mark("▎", theme.accent()))
        }
        Kind::Text | Kind::Reply | Kind::Output => {
            let lines = if item.kind == Kind::Output {
                plain(&item.text, theme.text(), inner)
            } else {
                markdown::render(&item.text, theme.text(), theme, inner)
            };
            let lines = if lines.is_empty() {
                vec![Vec::new()]
            } else {
                lines
            };
            wrap::prefixed(lines, bar(blank()), bar(blank()))
        }
        Kind::Message => {
            let lines = markdown::render(&item.text, theme.text(), theme, inner);
            wrap::prefixed(lines, bar(mark("✉", theme.dim())), bar(blank()))
        }
        Kind::Question => {
            let mut lines = vec![vec![Span::styled(
                "The agent asks:".to_string(),
                theme.accent().add_modifier(Modifier::BOLD),
            )]];
            lines.extend(markdown::render(&item.text, theme.text(), theme, inner));
            wrap::prefixed(lines, bar(mark("?", theme.accent())), bar(blank()))
        }
        Kind::Call => {
            let mut spans = worker_prefix(item);
            let failed = item.status == Some(CallStatus::Failed);
            spans.push(Span::styled(item.text.clone(), theme.dim()));
            match item.status {
                Some(CallStatus::Ok) => {
                    spans.push(Span::styled(" · ".to_string(), theme.dim()));
                    spans.push(Span::styled(
                        glyphs::render("✓").into_owned(),
                        theme.success(),
                    ));
                    if let Some(outcome) = &item.outcome {
                        spans.push(Span::styled(format!(" {outcome}"), theme.dim()));
                    }
                }
                Some(CallStatus::Failed) => {
                    spans.push(Span::styled(" · ".to_string(), theme.dim()));
                    let outcome = item.outcome.as_deref().unwrap_or("failed");
                    spans.push(Span::styled(
                        format!("{} {outcome}", glyphs::render("✗")),
                        theme.error(),
                    ));
                }
                _ => spans.push(Span::styled(
                    format!(" {}", glyphs::render("…")),
                    theme.dim(),
                )),
            }
            let marker_style = if failed { theme.error() } else { theme.dim() };
            let mut lines = wrap::prefixed(
                wrap::wrap(&spans, inner, 2),
                bar(mark("▸", marker_style)),
                bar(blank()),
            );
            if selected && let Some(detail) = &item.detail {
                lines.extend(gutter(detail, theme.dim(), theme, inner));
            }
            lines
        }
        Kind::Result => {
            let style = match item.level {
                Some(Level::Error) => theme.error(),
                Some(Level::Warn) => theme.warning(),
                _ => theme.dim(),
            };
            let text = match &item.detail {
                Some(detail) if item.level == Some(Level::Error) => {
                    format!("{}\n{detail}", item.text)
                }
                Some(detail) => detail.clone(),
                None => item.text.clone(),
            };
            gutter(&text, style, theme, inner)
        }
        Kind::Notice => {
            let (symbol, style) = match item.level {
                Some(Level::Error) => ("✗", theme.error()),
                Some(Level::Warn) => ("!", theme.warning()),
                _ => ("·", theme.dim()),
            };
            let mut spans = worker_prefix(item);
            spans.push(Span::styled(item.text.clone(), style));
            wrap::prefixed(
                wrap::wrap(&spans, inner, 0),
                bar(mark(symbol, style)),
                bar(blank()),
            )
        }
        Kind::Turn => {
            let rule = glyphs::render("─").into_owned();
            let text = format!("{rule}{rule} {} ", item.text);
            let fill = inner.saturating_sub(text_width(&text));
            let mut spans = vec![Span::styled(text, theme.dim())];
            spans.push(Span::styled(rule.repeat(fill), theme.dim()));
            let lines = wrap::wrap(&spans, inner, 0);
            wrap::prefixed(lines, blank(), blank())
        }
        // Drawn from the worker's summary.
        Kind::WorkerLine => Vec::new(),
    }
}

/// What the user's text became, when it isn't the usual (a new turn).
fn user_tag(item: &Item) -> Option<String> {
    let delivery = match item.delivery {
        Some(Delivery::Note) => Some("added to the task"),
        Some(Delivery::Answer) => Some("answer"),
        Some(Delivery::Queued) => Some("waits for its turn"),
        _ => None,
    };
    let web = (item.source == Some(Source::Web)).then_some("from the web page");
    match (delivery, web) {
        (Some(d), Some(w)) => Some(format!("{d} · {w}")),
        (Some(d), None) => Some(d.to_string()),
        (None, Some(w)) => Some(w.to_string()),
        (None, None) => None,
    }
}

/// `text` as it is, wrapped.
fn plain(text: &str, style: Style, width: usize) -> Vec<Vec<Span<'static>>> {
    let text = wrap::clean(&glyphs::render(text));
    let mut lines = Vec::new();
    for line in text.split('\n') {
        lines.extend(wrap::wrap(
            &[Span::styled(line.to_string(), style)],
            width,
            0,
        ));
    }
    lines
}

/// `text` beside a gutter, under its call: a command's output, an error.
fn gutter(text: &str, style: Style, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let lines = plain(text, style, width.saturating_sub(2).max(1));
    let side = Span::styled(format!("  {}", glyphs::render("│ ")), theme.dim());
    lines
        .into_iter()
        .map(|mut spans| {
            spans.insert(0, side.clone());
            Line::from(spans)
        })
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

/// How long the worker has run (or ran).
pub fn worker_time(worker: &WorkerSummary) -> Option<String> {
    let start = worker.started_at?;
    let end = worker
        .ended_at
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    Some(duration(end - start))
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
            format!("{} · ", facts.join(" · ")),
            theme.dim(),
        ));
        let (word, style) = state(worker, theme);
        spans.push(Span::styled(word.to_string(), style));
        if worker.state == WorkerState::Running
            && let Some(last) = &worker.last
        {
            spans.push(Span::styled(format!(" · {last}"), theme.dim()));
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
fn heading_lines(worker: &WorkerSummary, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(MARGIN).max(1);
    let (word, style) = state(worker, theme);
    let mut spans = vec![
        Span::styled(
            format!("{} ", worker.id),
            theme.text().add_modifier(Modifier::BOLD),
        ),
        Span::styled(worker.title.clone(), theme.text()),
        Span::styled(" · ".to_string(), theme.dim()),
        Span::styled(word.to_string(), style),
    ];
    if let Some(time) = worker_time(worker) {
        spans.push(Span::styled(format!(" · {time}"), theme.dim()));
    }
    wrap::prefixed(
        wrap::wrap(&spans, inner, 0),
        Span::raw("  "),
        Span::raw("  "),
    )
}

/// A worker's summary as text, to copy.
fn worker_text(worker: &WorkerSummary) -> String {
    let mut text = format!("{} {}", worker.id, worker.title);
    if let Some(report) = &worker.report {
        text.push_str("\n\n");
        text.push_str(report);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::MAIN_AGENT;

    fn item(id: u64, kind: Kind, text: &str) -> Item {
        let mut item = Item::new(kind, text);
        item.id = id;
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

    /// A call's outcome is on its line; a command's output is a block
    /// under it; the user's message has the bar; a turn ends with a rule.
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
                "  ── 3 s · $0.010 ──────────────────────",
            ]
        );
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
            id: "r1".into(),
            title: "Test it".into(),
            state: WorkerState::Running,
            tool_calls: 1,
            started_at: None,
            ended_at: None,
            last: Some("cargo test".into()),
            report: None,
        }));
        let mut reply = item(7, Kind::Reply, "Done.");
        reply.turn = Some(1);
        t.apply(Update::Item(reply));

        let full = rows(&mut t, &Filter::Main(View::Full), 60, 8);
        assert!(full.iter().any(|r| r.contains("r1 cargo test")), "{full:?}");
        assert!(!full.iter().any(|r| r.contains("Thinking")));
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
        assert_eq!(t.item(1).unwrap().agent, MAIN_AGENT);
    }
}

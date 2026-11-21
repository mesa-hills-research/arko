//! [`Indexes`] covers Arko's indexes in one process. It finds them (the folders under
//! `<data root>/indexes/`), creates, builds, embeds and searches them, keeps the
//! monitored ones current with watcher threads, and counts what embedding costs.
//!
//! Everything here blocks (arko_index works on files and calls the embedder
//! synchronously), so async callers go through `spawn_blocking`, background work
//! runs on its own `std::thread`s, which [`Indexes::shutdown`] stops and joins.
//!
//! Freshness:
//! - A monitored index this process watches is rebuilt in the background once
//!   its tree has been quiet for [`Timing::quiet`] (2 s), at most every
//!   [`Timing::min_interval`] (30 s). The chunks that changed are embedded next.
//!   A search that finds changes not yet built rebuilds first when the last build
//!   took at most [`Timing::sync_rebuild_max`] (5 s). Otherwise it notes them.
//! - Any other index (Arko's one-shot `-p` mode, an index of another tree, or one
//!   not monitored) is reconciled when first searched (and again after
//!   [`Timing::reconcile_interval`]). Its watcher state is opened, which finds
//!   what changed since it was last acknowledged, and the index is rebuilt when
//!   anything did. An index another Arko process watches is left to it (a
//!   monitoring Arko takes over once the other stops).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arko_index::bench_v1::{Chunk, Options};
use arko_index::dense::{DenseBuild, DenseIndexInfo, EmbedProgress, EmbedState, StopSignal};
use arko_index::hybrid::{self, Mode, SearchRequest};
use arko_index::index2::{self, BuildOptions};
use arko_index::vector::Filter;
use chrono::{DateTime, Utc};
use tokio::runtime::Handle;

use super::census::{self, Census};
use super::embed::{self, Backend};
use super::lexical::{Cursor, Lines, Literal, columns};
use super::monitor::{self, OpenError};
use super::store::{self, BuildRecord, Kind, Layout, Manifest, Rules};

/// Serializes manifest updates in this process. An app runs several sessions
/// in one process, each with its own [`Indexes`] over the same folder, and
/// their updates would otherwise undo each other's and share a temporary file.
static MANIFESTS: Mutex<()> = Mutex::new(());

/// Receives progress lines of background work (the CLI prints them, and Arko logs them).
pub type Events = Arc<dyn Fn(&str) + Send + Sync>;

/// Tokens a search in Arko's one-shot mode may embed to bring the chunks changed
/// since the last build up to date (about $0.02 with voyage-code-4). The rest stay
/// lexical until Arko monitors the index or `arko index embed` runs.
pub const FRESHEN_EMBED_TOKENS: u64 = 200_000;
/// The most results a semantic or hybrid search returns. A lexical search
/// returns any number.
pub const MAX_LIMIT: usize = 50;
/// Results by default, per mode.
pub const DEFAULT_LIMITS: Limits = Limits {
    semantic: 10,
    hybrid: 10,
    lexical: 100,
};
/// The setting that changes [`DEFAULT_LIMITS`] for the process, such as
/// `lexical=50,semantic=5`.
pub const LIMITS_SETTING: &str = "ARKO_INDEX_LIMITS";

/// The results a search returns by default, per mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub semantic: usize,
    pub hybrid: usize,
    pub lexical: usize,
}

impl Limits {
    /// The default results of a search in `mode` (None is hybrid where it
    /// can be).
    pub fn of(&self, mode: Option<ModeArg>) -> usize {
        match mode {
            Some(ModeArg::Semantic) => self.semantic,
            Some(ModeArg::Lexical) => self.lexical,
            Some(ModeArg::Hybrid) | None => self.hybrid,
        }
    }

    /// These limits with those `text` sets (`lexical=50,semantic=5`), each 1
    /// or more, and semantic and hybrid at most [`MAX_LIMIT`].
    pub fn with(mut self, text: &str) -> Result<Self, String> {
        for part in text
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let (mode, count) = part
                .split_once('=')
                .ok_or_else(|| format!("{part:?} isn't MODE=COUNT, such as lexical=50"))?;
            let mode = ModeArg::parse(mode)
                .filter(|_| !mode.trim().is_empty())
                .ok_or_else(|| format!("{:?} isn't semantic, hybrid or lexical", mode.trim()))?;
            let count: usize = count
                .trim()
                .parse()
                .ok()
                .filter(|count| *count > 0)
                .ok_or_else(|| format!("{part:?} needs a count of 1 or more"))?;
            if mode != ModeArg::Lexical && count > MAX_LIMIT {
                return Err(format!(
                    "{part:?} is over {MAX_LIMIT}, the most in that mode"
                ));
            }
            match mode {
                ModeArg::Semantic => self.semantic = count,
                ModeArg::Hybrid => self.hybrid = count,
                ModeArg::Lexical => self.lexical = count,
            }
        }
        Ok(self)
    }
}

/// The default results of this process, meaning [`DEFAULT_LIMITS`] with what
/// `ARKO_INDEX_LIMITS` (in the environment or a `.env` file) sets. A setting
/// that doesn't parse is logged and left out.
pub fn limits() -> Limits {
    static LIMITS: std::sync::OnceLock<Limits> = std::sync::OnceLock::new();
    *LIMITS.get_or_init(|| {
        let Some(text) = crate::credentials::var(LIMITS_SETTING) else {
            return DEFAULT_LIMITS;
        };
        DEFAULT_LIMITS.with(&text).unwrap_or_else(|error| {
            log::warn!("{LIMITS_SETTING} is left out: {error}");
            DEFAULT_LIMITS
        })
    })
}
/// The files a lexical search's note names of those it couldn't read.
const UNREAD_NAMED: usize = 5;
/// Why a watcher thread ended when it panicked (the log has the panic).
const PANICKED_WATCHER: &str = "an unexpected error, which the log shows";
/// The last background run's line when its thread panicked.
const PANICKED_EMBEDDING: &str = "Embedding stopped on an unexpected error, which the log shows";

/// When monitoring and freshening act.
#[derive(Clone, Debug)]
pub struct Timing {
    /// A change is built once the tree has been quiet this long.
    pub quiet: Duration,
    /// At least this long between the end of a background build and the next.
    pub min_interval: Duration,
    /// How long an inotify watcher waits for a change per loop.
    pub poll: Duration,
    /// The polling watcher's interval between looks at the tree (longer for
    /// trees whose look takes long, at ten times its duration).
    pub poll_interval: Duration,
    /// A search rebuilds first when changes are pending and the last build took
    /// at most this many seconds.
    pub sync_rebuild_max: f64,
    /// An index no watcher of this process keeps current is reconciled when
    /// searched, at most this often.
    pub reconcile_interval: Duration,
    /// How long an interactive build waits for another process's build.
    pub busy_wait: Duration,
    /// How often a watcher thread tries again to take over an index another
    /// process watches (or holds briefly, reconciling it).
    pub busy_retry: Duration,
    /// Use the polling watcher on Linux too (tests).
    pub force_polling: bool,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            quiet: Duration::from_secs(2),
            min_interval: Duration::from_secs(30),
            poll: Duration::from_millis(500),
            poll_interval: Duration::from_secs(2),
            sync_rebuild_max: 5.0,
            reconcile_interval: Duration::from_secs(30),
            busy_wait: Duration::from_secs(60),
            busy_retry: Duration::from_secs(30),
            force_polling: false,
        }
    }
}

/// Whether and how this process watches an index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WatchState {
    /// Not watched by this process.
    #[default]
    Off,
    /// The watcher thread is opening its watcher (reconciling the tree).
    Starting,
    Watching {
        polling: bool,
    },
    /// Another process (another Arko) holds the watcher state. A watcher thread
    /// of this one tries again every [`Timing::busy_retry`], and a status taken
    /// without one probes the lock.
    Elsewhere,
    /// The watcher failed, and the thread opens it again soon.
    Retrying(String),
    /// The watcher thread ended on an error (or couldn't start), and the next
    /// search starts another.
    Stopped(String),
}

impl WatchState {
    pub fn describe(&self) -> String {
        match self {
            WatchState::Off => "not watching it now".into(),
            WatchState::Starting => "watcher starting".into(),
            WatchState::Watching { polling: false } => "watching (inotify)".into(),
            WatchState::Watching { polling: true } => "watching (polling)".into(),
            WatchState::Elsewhere => "watched by another Arko process".into(),
            WatchState::Retrying(error) => format!("watcher failed ({error}), trying again"),
            WatchState::Stopped(error) => format!("watcher stopped ({error})"),
        }
    }
    pub fn active(&self) -> bool {
        matches!(self, WatchState::Starting | WatchState::Watching { .. })
    }
}

/// What an index's monitor setting and this process's watcher say of it
/// together, as `index_status` words it. A monitored index that this process
/// doesn't watch is brought up to date when an agent searches it.
pub fn monitor_words(monitor: bool, watch: &WatchState) -> String {
    match (monitor, watch) {
        (false, _) => "off".into(),
        (true, WatchState::Off) => format!(
            "on, {} (brought up to date when an agent searches it)",
            watch.describe()
        ),
        (true, watch) => format!("on, {}", watch.describe()),
    }
}

#[derive(Default)]
struct Live {
    watch: WatchState,
    /// The last delta sequence the watcher delivered.
    delivered: u64,
    /// The last sequence a successful build covers (the watcher acknowledges it).
    covered: u64,
    /// Changes are waiting for a build.
    dirty: bool,
    /// Deltas since the last build.
    changes: u64,
    last_change: Option<Instant>,
    last_build_end: Option<Instant>,
    last_build_secs: Option<f64>,
    last_reconcile: Option<Instant>,
}

#[derive(Default)]
struct EmbedSlot {
    running: bool,
    again: bool,
    cap: Option<u64>,
    explicit: bool,
    thread: Option<JoinHandle<()>>,
    last: Option<String>,
}

struct WatchThread {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// One index's runtime state in this process.
struct Entry {
    layout: Layout,
    /// Serializes builds (and watcher opens) in this process, since arko_index's own
    /// lock refuses a second concurrent build.
    build_lock: Mutex<()>,
    live: Mutex<Live>,
    embed: Mutex<EmbedSlot>,
    /// The stop signals of the embedding runs in progress.
    runs: Mutex<Vec<Arc<StopSignal>>>,
    watcher: Mutex<Option<WatchThread>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Caller {
    /// A tool call or command waits for another process's build.
    Interactive,
    /// A watcher thread gets half the threads, with no waiting.
    Background,
}

/// An index found on disk.
#[derive(Clone, Debug)]
pub struct Found {
    pub layout: Layout,
    pub manifest: Manifest,
}

/// An index as the agents' instructions name it (see [`Indexes::known`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Known {
    pub name: String,
    /// `code` or `docs`.
    pub kind: &'static str,
    /// The indexed directory (canonical).
    pub root: PathBuf,
    /// Where it lies from the working directory, when it is the working
    /// directory, holds it or lies in it.
    pub place: Option<&'static str>,
}

/// The search modes tools and commands take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeArg {
    Hybrid,
    Semantic,
    Lexical,
}

impl ModeArg {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "hybrid" | "auto" | "" => Some(ModeArg::Hybrid),
            "semantic" | "dense" | "vector" => Some(ModeArg::Semantic),
            "lexical" | "bm25" | "keyword" | "keywords" => Some(ModeArg::Lexical),
            _ => None,
        }
    }
}

/// A search.
#[derive(Clone, Debug)]
pub struct SearchArgs {
    pub query: String,
    /// An index name, id or root path. None means every index of the directory.
    pub index: Option<String>,
    /// None means hybrid where the index has vectors and a key is set, else
    /// lexical. Only `Lexical` asked for is an exact search (see
    /// [`lexical`](super::lexical)).
    pub mode: Option<ModeArg>,
    /// Results, by default the mode's in [`limits`].
    pub limit: Option<usize>,
    /// An exact search returns every match, whatever `limit` says.
    pub exhaustive: bool,
    /// An exact search's results start after this cursor (see
    /// [`Cursor`]), the last result of the page before.
    pub after: Option<String>,
    /// A path (relative to `cwd`, or absolute) results must be under.
    pub path_prefix: Option<String>,
    pub language: Option<String>,
    /// An exact search matches any case of the query's ASCII letters.
    pub ignore_case: bool,
    pub cwd: PathBuf,
}

/// A search result.
#[derive(Clone, Debug)]
pub struct Hit {
    /// The index's name.
    pub index: String,
    /// The file as shown, relative to the search's directory when under it.
    pub display: String,
    pub file: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    /// The byte column (from 1) in its first line where the result starts,
    /// set for the matches of an exact search.
    pub column: usize,
    pub scope: String,
    pub symbol: String,
    /// The file changed after the index was built.
    pub changed: bool,
    /// The lines that hold the query, for a match of an exact search.
    pub lines: Option<Lines>,
}

impl Hit {
    /// The file as shown, and the line and column where the result starts.
    /// An exact search lists its matches in the order of these places.
    pub(crate) fn place(&self) -> (&str, usize, usize) {
        (&self.display, self.start_line, self.column)
    }
}

/// The page after the one an exact search listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NextPage {
    /// The cursor that starts it, the place of the last result listed.
    pub cursor: Cursor,
    /// The results it holds, with the limit the search was given.
    pub count: usize,
}

/// What a search found, and how.
#[derive(Clone, Debug, Default)]
pub struct SearchOutput {
    pub hits: Vec<Hit>,
    /// The matches in all, for an exact search, which `hits` may cut.
    pub total: Option<usize>,
    /// The matches an exact search left out because `after` came past them.
    pub before: usize,
    /// The cursor the exact search started after.
    pub after: Option<String>,
    /// The page that follows an exact search's, when the list goes on.
    pub next: Option<NextPage>,
    /// Each index searched, meaning its name, kind and the mode used.
    pub searched: Vec<(String, Kind, Mode)>,
    pub notes: Vec<String>,
    /// Tokens billed for query embeddings.
    pub query_tokens: u64,
}

/// An index's state, for status.
#[derive(Clone, Debug)]
pub struct IndexReport {
    pub id: String,
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub dir: PathBuf,
    /// The manifest, or why it couldn't be read.
    pub manifest: Result<Manifest, String>,
    pub watch: WatchState,
    /// Chunks in the current generation (none means never built).
    pub chunks: Option<u64>,
    pub status_error: Option<String>,
    pub dense: Option<DenseIndexInfo>,
    pub embedding_active: bool,
    pub progress: Option<EmbedProgress>,
    pub pending_changes: u64,
    pub embed_running_here: bool,
    pub last_embed: Option<String>,
    pub has_key: bool,
}

/// A free estimate (a run with a cap of 0 tokens), meaning what embedding the chunks
/// without a vector would take.
#[derive(Clone, Debug)]
pub struct Estimate {
    pub tokens: u64,
    pub texts: u64,
    pub dollars: f64,
    pub model: String,
    /// Chunks with a vector, of all.
    pub embedded: u64,
    pub docs: u64,
}

/// What an embedding run did.
#[derive(Clone, Debug)]
pub struct EmbedSummary {
    pub tokens: u64,
    pub dollars: f64,
    pub new: u64,
    pub embedded: u64,
    pub docs: u64,
    pub cap: u64,
    pub cap_reached: bool,
    pub stopped: bool,
    pub superseded: bool,
    pub not_embedded_tokens: u64,
    pub too_long: u64,
}

/// Embedding plans of `index_create`.
#[derive(Clone, Debug)]
pub enum EmbedPlan {
    /// Lexical only.
    Off,
    /// The index embeds, but there is no key.
    NoKey,
    /// Embedded while the call waited.
    Done(Result<EmbedSummary, String>),
    /// Embedding in a background thread.
    Background,
    /// Left to the caller.
    Deferred,
}

/// What `create` did.
#[derive(Clone, Debug)]
pub struct CreateOutcome {
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub layout: Layout,
    pub manifest: Manifest,
    /// The index existed (same directory and kind), so its settings were updated.
    pub updated: bool,
    pub census: Option<Census>,
    pub record: BuildRecord,
    pub estimate: Option<Result<Estimate, String>>,
    pub embedding: EmbedPlan,
    pub watch: WatchState,
    pub notes: Vec<String>,
}

/// What `create` takes.
#[derive(Clone, Debug)]
pub struct CreateArgs {
    pub path: PathBuf,
    pub cwd: PathBuf,
    /// None means by a census of the files.
    pub kind: Option<Kind>,
    pub name: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub monitor: bool,
    /// None means the existing index's setting, else whether a key is set.
    pub embed: Option<bool>,
    pub max_embed_tokens: Option<u64>,
    /// Embed before returning (else in the background).
    pub wait: bool,
    /// Estimate only, and the caller embeds (the CLI, with progress).
    pub defer_embed: bool,
}

/// `index_manage` actions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Rebuild,
    Embed { max_tokens: Option<u64>, wait: bool },
    Estimate,
    Monitor(bool),
    Remove,
}

/// Arko's indexes, for one process, shared by the tools (through an `Arc`), the
/// watcher threads and the embedding threads.
pub struct Indexes {
    data_root: PathBuf,
    workspace: PathBuf,
    home: Option<PathBuf>,
    /// Where embeddings come from. The Voyage key changes while Arko runs
    /// ([`Indexes::set_voyage_key`]).
    backend: RwLock<Backend>,
    monitor: bool,
    handle: Handle,
    timing: Timing,
    events: Option<Events>,
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    spent: Mutex<(u64, f64)>,
    stopping: AtomicBool,
}

impl std::fmt::Debug for Indexes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Indexes")
            .field("data_root", &self.data_root)
            .field("workspace", &self.workspace)
            .field("backend", &self.backend())
            .field("monitor", &self.monitor)
            .finish_non_exhaustive()
    }
}

impl Indexes {
    /// Arko's indexes, with their data under `data_root` (Arko's data folder),
    /// `workspace` the directory Arko works in, and the Voyage key (none means
    /// lexical indexes and search only). Call it in Arko's tokio runtime. With
    /// `monitor`, every monitored index whose root contains the workspace or lies
    /// in it gets a watcher thread at once (it reconciles what changed while Arko
    /// wasn't running), and without (Arko's one-shot `-p` mode), indexes are
    /// reconciled when first searched.
    pub fn open(
        data_root: &Path,
        workspace: &Path,
        voyage_key: Option<String>,
        monitor: bool,
    ) -> Result<Arc<Self>, String> {
        let this = Self::open_with(
            data_root,
            workspace,
            Backend::voyage(voyage_key),
            monitor,
            Timing::default(),
            None,
        )?;
        this.watch_related();
        Ok(this)
    }

    pub(crate) fn open_with(
        data_root: &Path,
        workspace: &Path,
        backend: Backend,
        monitor: bool,
        timing: Timing,
        events: Option<Events>,
    ) -> Result<Arc<Self>, String> {
        let handle = Handle::try_current()
            .map_err(|_| "the index needs to be opened in a tokio runtime".to_string())?;
        let workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let home = std::env::home_dir().filter(|home| !home.as_os_str().is_empty());
        let this = Arc::new(Self {
            data_root: data_root.to_path_buf(),
            workspace,
            home,
            backend: RwLock::new(backend),
            monitor,
            handle,
            timing,
            events,
            entries: Mutex::new(HashMap::new()),
            spent: Mutex::new((0, 0.0)),
            stopping: AtomicBool::new(false),
        });
        Ok(this)
    }

    /// The index's file rules, leaving out Arko's index folder when it lies in
    /// the tree (its manifests change with every build, which would otherwise
    /// trigger the next).
    fn file_rules(&self, manifest: &Manifest) -> arko_index::FileRules {
        let mut rules = manifest.rules.file_rules();
        let indexes = self.indexes_dir();
        let indexes = indexes.canonicalize().unwrap_or(indexes);
        if let Ok(relative) = indexes.strip_prefix(&manifest.root)
            && !relative.as_os_str().is_empty()
        {
            let mut pattern = String::from("/");
            for c in relative.to_string_lossy().replace('\\', "/").chars() {
                if matches!(c, '\\' | '*' | '?' | '[' | ']' | '!' | '#' | ' ') {
                    pattern.push('\\');
                }
                pattern.push(c);
            }
            pattern.push('/');
            rules.exclude.push(pattern);
        }
        rules
    }

    /// Starts watching the monitored indexes of the workspace (when this
    /// instance monitors), and returns the number started.
    pub(crate) fn watch_related(self: &Arc<Self>) -> usize {
        if !self.monitor {
            return 0;
        }
        let mut started = 0;
        for found in self.related(&self.workspace) {
            if found.manifest.monitor {
                let entry = self.entry(&found.layout);
                self.start_watch(&entry);
                started += 1;
            }
        }
        started
    }

    /// Whether a Voyage key is set.
    pub fn has_key(&self) -> bool {
        self.backend().has_key()
    }

    /// Where embeddings come from now.
    fn backend(&self) -> Backend {
        self.backend
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Sets the Voyage key (none means lexical indexes and search only) for what
    /// starts from now on, meaning searches, builds and embedding runs. With a key,
    /// the watched indexes that lack vectors start embedding, as when their
    /// watchers started.
    pub fn set_voyage_key(self: &Arc<Self>, key: Option<String>) {
        *self
            .backend
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Backend::voyage(key);
        if !self.has_key() {
            return;
        }
        let entries: Vec<Arc<Entry>> = lock(&self.entries).values().cloned().collect();
        for entry in entries {
            let watching = matches!(lock(&entry.live).watch, WatchState::Watching { .. });
            if watching
                && index2::status(&entry.layout.state()).is_ok()
                && Self::embedding_incomplete(&entry)
            {
                self.request_embed(&entry, None, false);
            }
        }
    }

    /// One line for Arko's banner, meaning the indexes of the workspace, e.g.
    /// `index arko (code, monitored)`. None when there are none.
    pub fn summary(&self) -> Option<String> {
        let related = self.related(&self.workspace);
        if related.is_empty() {
            return None;
        }
        let parts: Vec<String> = related
            .iter()
            .map(|found| {
                let monitored = match self.watch_state(&found.layout) {
                    WatchState::Starting | WatchState::Watching { .. } => ", monitored",
                    WatchState::Elsewhere => ", monitored by another Arko",
                    _ => "",
                };
                let lexical = if found.manifest.embed {
                    ""
                } else {
                    ", lexical"
                };
                format!(
                    "{} ({}{lexical}{monitored})",
                    found.manifest.name,
                    found.manifest.kind.as_str()
                )
            })
            .collect();
        let mut line = format!(
            "{} {}",
            if parts.len() == 1 { "index" } else { "indexes" },
            parts.join(", ")
        );
        if !self.backend().has_key() && related.iter().any(|found| found.manifest.embed) {
            line.push_str(&format!(" · semantic search needs {}", embed::KEY_NAME));
        }
        Some(line)
    }

    /// The states of the workspace's indexes, the ones [`Self::summary`]
    /// names, by name (they read a few small files each).
    pub(crate) fn workspace_reports(&self) -> Vec<IndexReport> {
        self.related(&self.workspace)
            .into_iter()
            .map(|found| self.report(found.layout, Ok(found.manifest)))
            .collect()
    }

    /// Tokens billed for embeddings (indexing and queries) in this process, and
    /// their cost in dollars.
    pub fn spent(&self) -> (u64, f64) {
        *lock(&self.spent)
    }

    /// Stops the watcher threads and embedding runs (a run stops after its
    /// current request, and the vectors paid for are kept) and joins the threads,
    /// waiting at most 15 seconds (a build in progress can't be interrupted).
    pub async fn shutdown(&self) {
        let threads = self.stop_all();
        if threads.is_empty() {
            return;
        }
        let join = tokio::task::spawn_blocking(move || {
            for thread in threads {
                let _ = thread.join();
            }
        });
        if tokio::time::timeout(Duration::from_secs(15), join)
            .await
            .is_err()
        {
            log::warn!("index: background work didn't stop within 15 s");
        }
    }

    /// Stops everything and returns the threads to join.
    fn stop_all(&self) -> Vec<JoinHandle<()>> {
        self.stopping.store(true, Ordering::SeqCst);
        let entries: Vec<Arc<Entry>> = lock(&self.entries).values().cloned().collect();
        let mut threads = Vec::new();
        for entry in entries {
            for run in lock(&entry.runs).iter() {
                run.stop("Arko is exiting");
            }
            if let Some(watch) = lock(&entry.watcher).take() {
                watch.stop.store(true, Ordering::SeqCst);
                threads.push(watch.thread);
            }
            if let Some(thread) = lock(&entry.embed).thread.take() {
                threads.push(thread);
            }
        }
        threads
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    fn event(&self, text: &str) {
        match &self.events {
            Some(events) => events(text),
            None => log::info!("index: {text}"),
        }
    }

    fn add_spent(&self, tokens: u64, dollars: f64) {
        let mut spent = lock(&self.spent);
        spent.0 += tokens;
        spent.1 += dollars;
    }

    // ----- finding indexes -----

    pub(crate) fn indexes_dir(&self) -> PathBuf {
        store::indexes_dir(&self.data_root)
    }

    /// Every readable index.
    pub fn all(&self) -> Vec<Found> {
        store::scan(&self.indexes_dir())
            .into_iter()
            .filter_map(|scanned| {
                scanned.manifest.ok().map(|manifest| Found {
                    layout: scanned.layout,
                    manifest,
                })
            })
            .collect()
    }

    /// Every readable index, for the agents' instructions, meaning those of the
    /// working directory (`workspace`) first, then the others, each group by
    /// name, so the same indexes give the same list. When `workspace` is in a
    /// git worktree with no index of its own, falls back to its main repository.
    pub fn known(&self, workspace: &Path) -> Vec<Known> {
        let workspace = canonicalize_best_effort(workspace);
        let place_for = |root: &Path, dir: &Path| {
            if root == dir {
                Some("the working directory")
            } else if dir.starts_with(root) {
                Some("holds the working directory")
            } else if root.starts_with(dir) {
                Some("in the working directory")
            } else {
                None
            }
        };
        let mut known: Vec<Known> = self
            .all()
            .into_iter()
            .map(|found| {
                let root = found.manifest.root;
                let place = place_for(&root, &workspace);
                Known {
                    name: found.manifest.name,
                    kind: found.manifest.kind.as_str(),
                    root,
                    place,
                }
            })
            .collect();
        if known.iter().all(|item| item.place.is_none())
            && let Some(wt) = worktree_of(&workspace)
        {
            let mapped = wt.rebase(&workspace);
            for item in &mut known {
                item.place = place_for(&item.root, &mapped);
            }
        }
        known.sort_by(|a, b| {
            (a.place.is_none(), &a.name, &a.root).cmp(&(b.place.is_none(), &b.name, &b.root))
        });
        known
    }

    /// The indexes whose root contains `dir` or lies in it. When none matches
    /// directly and `dir` is inside a git worktree, falls back to its main
    /// repository.
    pub fn related(&self, dir: &Path) -> Vec<Found> {
        let dir = canonicalize_best_effort(dir);
        let all = self.all();
        let find = |target: &Path| -> Vec<Found> {
            all.iter()
                .filter(|found| {
                    target.starts_with(&found.manifest.root)
                        || found.manifest.root.starts_with(target)
                })
                .cloned()
                .collect()
        };
        let direct = find(&dir);
        if !direct.is_empty() {
            return direct;
        }
        if let Some(wt) = worktree_of(&dir) {
            return find(&wt.rebase(&dir));
        }
        direct
    }

    /// The index `name` names, meaning its name, its id, or a path (relative to `cwd`)
    /// in its root, meaning the root itself, or a directory or file in it, which names
    /// the index with the deepest root that holds it. Ambiguous when that
    /// directory has a code and a docs index.
    pub fn resolve(&self, name: &str, cwd: &Path) -> Result<Found, String> {
        self.resolve_in(name, cwd, true)
    }

    /// [`Self::resolve`], and `inside` false takes only an index's own root as a
    /// path (for removing one).
    pub(crate) fn resolve_in(&self, name: &str, cwd: &Path, inside: bool) -> Result<Found, String> {
        let name = name.trim();
        let all = self.all();
        if let Some(found) = all.iter().find(|found| found.manifest.name == name) {
            return Ok(found.clone());
        }
        if let Some(found) = all.iter().find(|found| found.layout.id() == name) {
            return Ok(found.clone());
        }
        let match_path = |path: &Path| -> Option<Result<Found, String>> {
            // The deepest root that holds the path.
            let root = all
                .iter()
                .map(|found| &found.manifest.root)
                .filter(|root| {
                    if inside {
                        path.starts_with(root)
                    } else {
                        **root == path
                    }
                })
                .max_by_key(|root| root.components().count())
                .cloned();
            let matching: Vec<&Found> = all
                .iter()
                .filter(|found| Some(&found.manifest.root) == root.as_ref())
                .collect();
            match matching.as_slice() {
                [one] => Some(Ok((*one).clone())),
                [] => None,
                many => {
                    let names: Vec<&str> = many
                        .iter()
                        .map(|found| found.manifest.name.as_str())
                        .collect();
                    Some(Err(format!(
                        "{} has several indexes ({}), so name one",
                        many[0].manifest.root.display(),
                        names.join(", ")
                    )))
                }
            }
        };
        if let Ok(path) = store::absolute(Path::new(name), cwd).canonicalize() {
            if let Some(result) = match_path(&path) {
                return result;
            }
            if inside
                && let Some(wt) = worktree_of(&path)
                && let Some(result) = match_path(&wt.rebase(&path))
            {
                return result;
            }
        }
        let names: Vec<&str> = all
            .iter()
            .map(|found| found.manifest.name.as_str())
            .collect();
        Err(if names.is_empty() {
            format!("no index is named {name:?}, and there are no indexes yet (create one)")
        } else {
            format!(
                "no index is named {name:?}. The indexes are {}",
                names.join(", ")
            )
        })
    }

    /// The indexes `index` names (one), or else those of `cwd`.
    pub(crate) fn targets(&self, index: Option<&str>, cwd: &Path) -> Result<Vec<Found>, String> {
        match index.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => Ok(vec![self.resolve(name, cwd)?]),
            None => {
                let related = self.related(cwd);
                if related.is_empty() {
                    Err(self.no_index_message(cwd))
                } else {
                    Ok(related)
                }
            }
        }
    }

    pub(crate) fn no_index_message(&self, cwd: &Path) -> String {
        let all = self.all();
        let mut message = format!("no index covers {}", cwd.display());
        if all.is_empty() {
            message.push_str(": there are no indexes yet (create one with index_create)");
        } else {
            let names: Vec<String> = all
                .iter()
                .map(|found| {
                    format!(
                        "{} ({})",
                        found.manifest.name,
                        found.manifest.root.display()
                    )
                })
                .collect();
            message.push_str(&format!(
                ": create one with index_create, or name one of {}",
                names.join(", ")
            ));
        }
        message
    }

    fn entry(&self, layout: &Layout) -> Arc<Entry> {
        lock(&self.entries)
            .entry(layout.id())
            .or_insert_with(|| {
                Arc::new(Entry {
                    layout: layout.clone(),
                    build_lock: Mutex::new(()),
                    live: Mutex::new(Live::default()),
                    embed: Mutex::new(EmbedSlot::default()),
                    runs: Mutex::new(Vec::new()),
                    watcher: Mutex::new(None),
                })
            })
            .clone()
    }

    pub(crate) fn watch_state(&self, layout: &Layout) -> WatchState {
        let entry = lock(&self.entries).get(&layout.id()).cloned();
        entry
            .map(|entry| lock(&entry.live).watch.clone())
            .unwrap_or_default()
    }

    /// Loads, changes and saves a manifest (nothing when it can't be read).
    fn update_manifest(
        &self,
        layout: &Layout,
        change: impl FnOnce(&mut Manifest),
    ) -> Option<Manifest> {
        let (manifest, saved) = {
            let _guard = lock(&MANIFESTS);
            let mut manifest = store::load_manifest(&layout.dir).ok()?;
            change(&mut manifest);
            let saved = store::save_manifest(&layout.dir, &manifest);
            (manifest, saved)
        };
        // Logged once the lock is free, since every session's updates wait on it.
        if let Err(error) = saved {
            log::warn!("index: {error}");
        }
        Some(manifest)
    }

    // ----- building -----

    fn build(
        &self,
        entry: &Entry,
        manifest: &Manifest,
        caller: Caller,
    ) -> Result<BuildRecord, String> {
        let _guard = lock(&entry.build_lock);
        self.build_locked(entry, manifest, caller)
    }

    /// Builds with the build lock held. It records what the build covers, and the
    /// result in the manifest.
    fn build_locked(
        &self,
        entry: &Entry,
        manifest: &Manifest,
        caller: Caller,
    ) -> Result<BuildRecord, String> {
        let sequence = lock(&entry.live).delivered;
        let result = self.run_build(entry, manifest, caller);
        {
            let mut live = lock(&entry.live);
            live.last_build_end = Some(Instant::now());
            if let Ok(record) = &result {
                live.covered = live.covered.max(sequence);
                live.last_build_secs = Some(record.seconds);
                if live.delivered == sequence {
                    live.dirty = false;
                    live.changes = 0;
                }
            }
        }
        match &result {
            Ok(record) => {
                self.update_manifest(&entry.layout, |m| {
                    m.last_build = Some(record.clone());
                    m.build_error = None;
                });
            }
            Err(error) => {
                self.update_manifest(&entry.layout, |m| {
                    m.build_error = Some(format!("build failed ({error})"));
                });
            }
        }
        result
    }

    fn run_build(
        &self,
        entry: &Entry,
        manifest: &Manifest,
        caller: Caller,
    ) -> Result<BuildRecord, String> {
        let state = entry.layout.state();
        let options = chunk_options(manifest.kind);
        let threads = match caller {
            Caller::Interactive => arko_index::threads(),
            Caller::Background => (arko_index::threads() / 2).max(1),
        };
        // An embedding index is built with its model (named only, since a run embeds
        // the chunks afterwards), which keeps its vectors and reuses the cached
        // ones, and a build without a model would remove them.
        let embedder = if manifest.embed {
            let descriptor = Backend::descriptor(&manifest.model)?;
            Some(self.backend().embedder(&descriptor, &self.handle, None)?)
        } else {
            None
        };
        let deadline = Instant::now()
            + match caller {
                Caller::Interactive => self.timing.busy_wait,
                Caller::Background => Duration::ZERO,
            };
        let began = Instant::now();
        loop {
            let mut settings = BuildOptions::default()
                .with_threads(threads)
                .with_rules(self.file_rules(manifest));
            if let Some(embedder) = &embedder {
                settings = settings.with_dense(
                    DenseBuild::new(embedder.as_ref())
                        .with_max_tokens(Some(manifest.max_embed_tokens))
                        .with_embed_later(true),
                );
            }
            match index2::build(&manifest.root, &state, &options, &settings) {
                Ok(stats) => {
                    return Ok(BuildRecord {
                        at: Some(Utc::now()),
                        chunks: stats.docs,
                        files: stats.report.files,
                        seconds: began.elapsed().as_secs_f64(),
                        summary: stats.report.summary(),
                    });
                }
                Err(error) => {
                    let error = error.to_string();
                    let busy = error.contains("already in use");
                    if busy && Instant::now() < deadline && !self.stopping() {
                        std::thread::sleep(Duration::from_millis(250));
                        continue;
                    }
                    if busy {
                        return Err(format!(
                            "{error} (another Arko process is building this index)"
                        ));
                    }
                    return Err(error);
                }
            }
        }
    }

    // ----- embedding -----

    /// One embedding run of the index's current generation, with a cap of `cap`
    /// tokens (0 means a free estimate). `record` saves the run in the manifest.
    fn run_embed(
        &self,
        entry: &Entry,
        cap: u64,
        record: bool,
        progress: Option<&(dyn Fn(&EmbedProgress) + Sync)>,
        stop: Option<Arc<StopSignal>>,
    ) -> Result<EmbedSummary, String> {
        let state = entry.layout.state();
        let info = index2::dense_info(&state)
            .map_err(|error| error.to_string())?
            .ok_or("the index has no embeddings (it was built lexical only)")?;
        let stop = stop.unwrap_or_default();
        let embedder = self
            .backend()
            .embedder(&info.embedder, &self.handle, Some(stop.clone()))?;
        let model = info.embedder.model.clone();
        let billed = AtomicU64::new(0);
        let on_progress = |update: &EmbedProgress| {
            let before = billed.swap(update.tokens, Ordering::SeqCst);
            if update.tokens > before {
                let tokens = update.tokens - before;
                self.add_spent(tokens, embed::cost(&model, tokens));
            }
            if let Some(progress) = progress {
                progress(update);
            }
        };
        lock(&entry.runs).push(stop.clone());
        // Checked once the run is listed, so an exit that began earlier is
        // seen here, and one that begins later finds the run in the list.
        if self.stopping() {
            stop.stop("Arko is exiting");
        }
        let dense = DenseBuild::new(embedder.as_ref())
            .with_max_tokens(Some(cap))
            .with_progress(&on_progress)
            .with_stop(&stop);
        let result = index2::embed(&state, &dense).map_err(|error| error.to_string());
        lock(&entry.runs).retain(|run| !Arc::ptr_eq(run, &stop));
        let mut tokens = billed.load(Ordering::SeqCst);
        if let Ok(stats) = &result
            && stats.tokens > tokens
        {
            let extra = stats.tokens - tokens;
            self.add_spent(extra, embed::cost(&model, extra));
            tokens = stats.tokens;
        }
        if record && cap > 0 {
            self.update_manifest(&entry.layout, |m| {
                m.embed_tokens += tokens;
                m.embed_cost += embed::cost(&model, tokens);
                match &result {
                    Ok(stats) => {
                        if !stats.superseded && !stats.stopped {
                            m.cap_reached = stats.cap_reached;
                        }
                        m.embed_error = None;
                    }
                    Err(error) => m.embed_error = Some(format!("embedding failed ({error})")),
                }
            });
        }
        let stats = result?;
        Ok(EmbedSummary {
            tokens,
            dollars: embed::cost(&model, tokens),
            new: stats.new,
            embedded: stats.embedded_docs,
            docs: stats.docs,
            cap,
            cap_reached: stats.cap_reached,
            stopped: stats.stopped,
            superseded: stats.superseded,
            not_embedded_tokens: stats.not_embedded_tokens,
            too_long: stats.too_long,
        })
    }

    /// A free estimate of embedding the chunks without a vector.
    fn estimate(&self, entry: &Entry) -> Result<Estimate, String> {
        let state = entry.layout.state();
        if index2::status(&state).is_ok_and(|status| status.embedding) {
            return Err("an embedding run is in progress".into());
        }
        let info = index2::dense_info(&state)
            .map_err(|error| error.to_string())?
            .ok_or("the index has no embeddings (it was built lexical only)")?;
        let summary = self.run_embed(entry, 0, false, None, None)?;
        Ok(Estimate {
            tokens: summary.not_embedded_tokens,
            texts: summary.docs.saturating_sub(summary.embedded),
            dollars: embed::cost(&info.embedder.model, summary.not_embedded_tokens),
            model: info.embedder.model,
            embedded: summary.embedded,
            docs: summary.docs,
        })
    }

    /// Starts (or queues) a background embedding run of the index, and returns whether one
    /// will run. Automatic runs (`explicit` false) don't follow a run that
    /// stopped at its cap.
    fn request_embed(
        self: &Arc<Self>,
        entry: &Arc<Entry>,
        cap: Option<u64>,
        explicit: bool,
    ) -> bool {
        if self.stopping() || !self.backend().has_key() {
            return false;
        }
        let Ok(manifest) = store::load_manifest(&entry.layout.dir) else {
            return false;
        };
        if !manifest.embed || (manifest.cap_reached && !explicit) {
            return false;
        }
        let mut slot = lock(&entry.embed);
        if slot.running {
            slot.again = true;
            if cap.is_some() {
                slot.cap = cap;
            }
            slot.explicit |= explicit;
            return true;
        }
        if let Some(old) = slot.thread.take() {
            let _ = old.join();
        }
        slot.running = true;
        slot.again = false;
        slot.cap = cap;
        slot.explicit = explicit;
        let this = Arc::clone(self);
        let worker = Arc::clone(entry);
        let spawned = std::thread::Builder::new()
            .name("arko-index-embed".into())
            .spawn(move || {
                // The panic hook logs a panic. The slot is freed so that the
                // next request starts a run.
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    this.embed_loop(&worker)
                }));
                if ran.is_err() {
                    let mut slot = lock(&worker.embed);
                    slot.running = false;
                    slot.last = Some(PANICKED_EMBEDDING.into());
                }
            });
        match spawned {
            Ok(thread) => {
                slot.thread = Some(thread);
                true
            }
            Err(error) => {
                slot.running = false;
                log::warn!("index: cannot start an embedding thread: {error}");
                false
            }
        }
    }

    fn embed_loop(&self, entry: &Entry) {
        loop {
            let (cap, explicit) = {
                let mut slot = lock(&entry.embed);
                slot.again = false;
                (slot.cap.take(), std::mem::take(&mut slot.explicit))
            };
            let mut superseded = false;
            let line = match store::load_manifest(&entry.layout.dir) {
                Err(error) => format!("embedding skipped ({error})"),
                Ok(manifest) if manifest.cap_reached && !explicit => format!(
                    "Embedding {} is paused, because the last run stopped at its cap",
                    manifest.name
                ),
                Ok(manifest) => {
                    let cap = cap.unwrap_or(manifest.max_embed_tokens);
                    match self.run_embed(entry, cap, true, None, None) {
                        Ok(summary) => {
                            superseded = summary.superseded;
                            let line = format!("{} {}", manifest.name, summary.line());
                            if summary.new > 0 || summary.cap_reached {
                                self.event(&line);
                            }
                            line
                        }
                        Err(error) => {
                            let line = format!("Embedding {} failed ({error})", manifest.name);
                            self.event(&line);
                            line
                        }
                    }
                }
            };
            let mut slot = lock(&entry.embed);
            slot.last = Some(line);
            if (slot.again || superseded) && !self.stopping() {
                continue;
            }
            slot.running = false;
            break;
        }
    }

    /// Whether the index has chunks without a vector.
    fn embedding_incomplete(entry: &Entry) -> bool {
        index2::dense_info(&entry.layout.state())
            .ok()
            .flatten()
            .is_some_and(|info| !info.complete())
    }

    // ----- monitoring -----

    fn start_watch(self: &Arc<Self>, entry: &Arc<Entry>) {
        if !self.monitor || self.stopping() {
            return;
        }
        let mut watcher = lock(&entry.watcher);
        if watcher
            .as_ref()
            .is_some_and(|watch| !watch.thread.is_finished())
        {
            return;
        }
        if let Some(old) = watcher.take() {
            let _ = old.thread.join();
        }
        let stop = Arc::new(AtomicBool::new(false));
        lock(&entry.live).watch = WatchState::Starting;
        let this = Arc::clone(self);
        let worker = Arc::clone(entry);
        let flag = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name("arko-index-watch".into())
            .spawn(move || {
                // The panic hook logs a panic. The loop's watcher, and its
                // lock, dropped as it unwound, so once the state stops saying
                // watching, the next search reconciles and starts a watcher.
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    this.watch_loop(&worker, &flag)
                }));
                if ran.is_err() {
                    lock(&worker.live).watch = WatchState::Stopped(PANICKED_WATCHER.into());
                }
            });
        match spawned {
            Ok(thread) => *watcher = Some(WatchThread { stop, thread }),
            Err(error) => {
                lock(&entry.live).watch = WatchState::Stopped(error.to_string());
            }
        }
    }

    /// Stops the index's watcher thread and returns it to join.
    fn stop_watch(&self, entry: &Entry) -> Option<JoinHandle<()>> {
        let watch = lock(&entry.watcher).take()?;
        watch.stop.store(true, Ordering::SeqCst);
        Some(watch.thread)
    }

    /// A watcher thread opens the watcher (reconciling the tree), then marks
    /// the index dirty on each change, rebuilds it once the tree is quiet (and
    /// the last build is long enough ago), queues the embedding of what changed,
    /// and acknowledges the changes a successful build covers.
    fn watch_loop(self: &Arc<Self>, entry: &Arc<Entry>, stop: &AtomicBool) {
        let stopped = || stop.load(Ordering::SeqCst) || self.stopping();
        let mut changes: Option<Box<dyn monitor::Changes>> = None;
        let mut acked = 0u64;
        let mut next_look = Instant::now();
        let mut reopen_at = Instant::now();
        let mut name = entry.layout.id();
        let mut busy_reported = false;
        while !stopped() {
            let manifest = match store::load_manifest(&entry.layout.dir) {
                Ok(manifest) => manifest,
                Err(error) => {
                    lock(&entry.live).watch = WatchState::Stopped(error.clone());
                    self.event(&format!("Stopped watching {name} ({error})"));
                    return;
                }
            };
            name.clone_from(&manifest.name);
            if changes.is_none() {
                if Instant::now() < reopen_at {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                let opened = {
                    let _guard = lock(&entry.build_lock);
                    monitor::open_changes(
                        &manifest.root,
                        &entry.layout,
                        &self.file_rules(&manifest),
                        manifest.kind == Kind::Docs,
                        self.timing.force_polling,
                    )
                };
                match opened {
                    Ok(opened) => {
                        let polling = opened.polling();
                        let built = index2::status(&entry.layout.state()).is_ok();
                        {
                            let mut live = lock(&entry.live);
                            live.watch = WatchState::Watching { polling };
                            live.delivered = 0;
                            live.covered = 0;
                            if !built || manifest.build_error.is_some() {
                                live.dirty = true;
                            }
                        }
                        acked = 0;
                        changes = Some(opened);
                        busy_reported = false;
                        self.event(&format!(
                            "Watching {name} at {} ({})",
                            manifest.root.display(),
                            if polling { "polling" } else { "inotify" }
                        ));
                        if built && Self::embedding_incomplete(entry) {
                            self.request_embed(entry, None, false);
                        }
                    }
                    // Another process holds the watcher state, either another Arko
                    // watching it (this one takes over when it exits), or one
                    // reconciling it for a moment.
                    Err(OpenError::Busy) => {
                        lock(&entry.live).watch = WatchState::Elsewhere;
                        if !busy_reported {
                            busy_reported = true;
                            self.event(&format!(
                                "Another Arko process is watching {name}, and this one takes over when it stops"
                            ));
                        }
                        reopen_at = Instant::now() + self.timing.busy_retry;
                        continue;
                    }
                    Err(OpenError::Failed(error)) => {
                        self.event(&format!("The watcher of {name} failed ({error})"));
                        lock(&entry.live).watch = WatchState::Retrying(error);
                        reopen_at = Instant::now() + Duration::from_secs(30);
                        continue;
                    }
                }
            }
            let Some(watcher) = changes.as_mut() else {
                continue;
            };
            let polling = watcher.polling();
            if polling && Instant::now() < next_look {
                std::thread::sleep(
                    next_look
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(200)),
                );
            } else {
                let wait = if polling {
                    Duration::ZERO
                } else {
                    self.timing.poll
                };
                match watcher.poll(wait) {
                    Ok(Some(sequence)) => {
                        let mut live = lock(&entry.live);
                        live.delivered = sequence;
                        live.dirty = true;
                        live.changes += 1;
                        live.last_change = Some(Instant::now());
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.event(&format!(
                            "The watcher of {name} failed ({error}), so it reopens"
                        ));
                        lock(&entry.live).watch = WatchState::Retrying(error);
                        changes = None;
                        reopen_at = Instant::now() + Duration::from_secs(5);
                        continue;
                    }
                }
                if polling {
                    next_look = Instant::now()
                        + self
                            .timing
                            .poll_interval
                            .max(watcher.check_cost().saturating_mul(10));
                }
            }
            let due = {
                let live = lock(&entry.live);
                live.dirty
                    && live
                        .last_change
                        .is_none_or(|at| at.elapsed() >= self.timing.quiet)
                    && live
                        .last_build_end
                        .is_none_or(|at| at.elapsed() >= self.timing.min_interval)
            };
            if due && !stopped() {
                match self.build(entry, &manifest, Caller::Background) {
                    Ok(record) => {
                        self.event(&format!(
                            "Rebuilt {name}, {} ({:.1} s)",
                            chunks_in_files(record.chunks, record.files),
                            record.seconds
                        ));
                        if !stopped() {
                            self.request_embed(entry, None, false);
                        }
                    }
                    Err(error) => self.event(&format!("Rebuilding {name} failed ({error})")),
                }
            }
            let covered = lock(&entry.live).covered;
            if covered > acked
                && let Some(watcher) = changes.as_mut()
            {
                match watcher.ack(covered) {
                    Ok(()) => acked = covered,
                    Err(error) => log::warn!("index: {name}: acknowledging changes: {error}"),
                }
            }
        }
        // Asked to stop, so this process no longer watches it, whatever the
        // last attempt said.
        lock(&entry.live).watch = WatchState::Off;
    }

    /// Brings an index no watcher of this process keeps current up to date. It opens
    /// its watcher state (finding what changed since it was last acknowledged),
    /// rebuilds when anything did, and acknowledges. Skipped when this process
    /// watches it, when another process does, or when it was reconciled recently.
    /// The build, if one ran.
    fn reconcile(&self, entry: &Entry, manifest: &Manifest) -> Result<Option<BuildRecord>, String> {
        let _guard = lock(&entry.build_lock);
        {
            let mut live = lock(&entry.live);
            if live.watch.active() {
                return Ok(None);
            }
            if live
                .last_reconcile
                .is_some_and(|at| at.elapsed() < self.timing.reconcile_interval)
            {
                return Ok(None);
            }
            live.last_reconcile = Some(Instant::now());
        }
        let built = index2::status(&entry.layout.state()).is_ok();
        let opened = monitor::open_changes(
            &manifest.root,
            &entry.layout,
            &self.file_rules(manifest),
            manifest.kind == Kind::Docs,
            self.timing.force_polling,
        );
        match opened {
            Err(OpenError::Busy) => {
                lock(&entry.live).watch = WatchState::Elsewhere;
                if built {
                    Ok(None)
                } else {
                    self.build_locked(entry, manifest, Caller::Interactive)
                        .map(Some)
                }
            }
            Err(OpenError::Failed(error)) => {
                log::warn!("index: {}: watcher: {error}", manifest.name);
                if built && manifest.build_error.is_none() {
                    Ok(None)
                } else {
                    self.build_locked(entry, manifest, Caller::Interactive)
                        .map(Some)
                }
            }
            Ok(mut changes) => {
                {
                    let mut live = lock(&entry.live);
                    if live.watch == WatchState::Elsewhere {
                        live.watch = WatchState::Off;
                    }
                }
                let last = monitor::drain(changes.as_mut())?;
                let record = if last.is_some() || !built || manifest.build_error.is_some() {
                    Some(self.build_locked(entry, manifest, Caller::Interactive)?)
                } else {
                    None
                };
                if let Some(sequence) = last {
                    changes.ack(sequence)?;
                }
                Ok(record)
            }
        }
    }

    /// Before a search, rebuilds a watched index with pending changes when its
    /// builds are quick, or reconciles an unwatched one. Notes go to `notes`.
    fn freshen(self: &Arc<Self>, entry: &Arc<Entry>, manifest: &Manifest, notes: &mut Vec<String>) {
        let (watch, dirty, last_secs) = {
            let live = lock(&entry.live);
            (live.watch.clone(), live.dirty, live.last_build_secs)
        };
        let name = &manifest.name;
        if watch.active() {
            let quick = last_secs.is_some_and(|secs| secs <= self.timing.sync_rebuild_max);
            if dirty && quick {
                match self.build(entry, manifest, Caller::Interactive) {
                    Ok(_) => {
                        self.request_embed(entry, None, false);
                    }
                    Err(error) => notes.push(format!("Rebuilding {name} failed ({error})")),
                }
            }
            return;
        }
        match self.reconcile(entry, manifest) {
            Ok(Some(record)) => {
                notes.push(format!(
                    "{name} was rebuilt before searching, as files changed since it was last built ({:.1} s)",
                    record.seconds
                ));
                if self.monitor {
                    self.request_embed(entry, None, false);
                } else if manifest.embed
                    && self.backend().has_key()
                    && !manifest.cap_reached
                    && Self::embedding_incomplete(entry)
                {
                    match self.run_embed(entry, FRESHEN_EMBED_TOKENS, false, None, None) {
                        Ok(summary) if summary.cap_reached => notes.push(format!(
                            "{name} has vectors for {}/{} chunks, and the rest are found by keywords until an interactive Arko monitors it (or `arko index embed`)",
                            fmt_count(summary.embedded),
                            fmt_count(summary.docs)
                        )),
                        Ok(_) => {}
                        Err(error) => {
                            notes.push(format!("Embedding the changes of {name} failed ({error})"))
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(error) => notes.push(format!("Couldn't check {name} for changes ({error})")),
        }
        // A monitored index outside the workspace has no watcher yet, so once
        // searched, it is kept current too.
        if self.monitor && manifest.monitor {
            self.start_watch(entry);
        }
    }

    // ----- search -----

    /// Searches (blocking).
    pub(crate) fn search_blocking(
        self: &Arc<Self>,
        args: &SearchArgs,
    ) -> Result<SearchOutput, String> {
        let query = args.query.trim();
        if query.is_empty() {
            return Err("the query is empty".into());
        }
        let cwd = canonicalize_best_effort(&args.cwd);
        let cwd_worktree = worktree_of(&cwd);
        // Lexical asked for is an exact search, whose matches are all
        // equally right, so it has no most.
        let exact = args.mode == Some(ModeArg::Lexical);
        let after = args
            .after
            .as_deref()
            .map(str::trim)
            .filter(|after| !after.is_empty());
        if !exact && after.is_some() {
            return Err(
                "after is for mode lexical, whose results hold the query as written, in path order"
                    .into(),
            );
        }
        let after = after.map(Cursor::parse).transpose()?;
        let limit = args.limit.unwrap_or_else(|| limits().of(args.mode)).max(1);
        let limit = match exact {
            true if args.exhaustive => usize::MAX,
            true => limit,
            false if args.exhaustive => MAX_LIMIT,
            false => limit.min(MAX_LIMIT),
        };
        let prefix = args
            .path_prefix
            .as_deref()
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty());
        // Without an index named, the indexes of the path searched (any
        // directory), else of the current one.
        let named = args
            .index
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let place = match (named, prefix) {
            (None, Some(prefix)) => {
                canonicalize_best_effort(&store::absolute(Path::new(prefix), &cwd))
            }
            _ => cwd.clone(),
        };
        let targets = self.targets(named, &place)?;
        let language = args
            .language
            .as_deref()
            .map(str::trim)
            .filter(|language| !language.is_empty());
        let literal = exact.then(|| Literal::new(query, args.ignore_case));
        let mut output = SearchOutput::default();
        let mut lists: Vec<Vec<Hit>> = Vec::new();
        for target in &targets {
            let root = &target.manifest.root;
            let mut filter = Filter::default();
            if let Some(prefix) = prefix {
                match resolve_prefix(prefix, &cwd, cwd_worktree.as_ref(), root, named.is_some()) {
                    Ok(resolved) => {
                        if let Ok(relative) = resolved.strip_prefix(root) {
                            let relative = relative.to_string_lossy().replace('\\', "/");
                            if !relative.is_empty() {
                                filter = filter.with_path_prefix(relative);
                            }
                        }
                    }
                    Err(wanted) if targets.len() == 1 => {
                        return Err(format!(
                            "{} is outside the index {} ({})",
                            wanted.display(),
                            target.manifest.name,
                            root.display()
                        ));
                    }
                    Err(_) => continue,
                }
            }
            if let Some(language) = language {
                filter = filter.with_language(language.to_ascii_lowercase());
            }
            let entry = self.entry(&target.layout);
            self.freshen(&entry, &target.manifest, &mut output.notes);
            let manifest = store::load_manifest(&target.layout.dir)
                .unwrap_or_else(|_| target.manifest.clone());
            let query = Query {
                text: query,
                mode: args.mode,
                limit,
                filter,
                cwd: &cwd,
            };
            lists.push(match &literal {
                Some(literal) => {
                    self.search_exact(&entry, &manifest, &query, literal, &mut output)?
                }
                None => self.search_one(&entry, &manifest, &query, &mut output)?,
            });
        }
        if literal.is_some() {
            let page = page_of(lists.into_iter().flatten().collect(), after.as_ref(), limit);
            output.hits = page.hits;
            output.total = Some(page.total);
            output.before = page.before;
            output.after = after.as_ref().map(Cursor::to_string);
            output.next = page.next;
        } else {
            // Interleaved by rank, so each index's best results come first
            // (and each index keeps its share of a cut list), up to the limit.
            let mut lists: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
            while output.hits.len() < limit {
                let before = output.hits.len();
                for list in &mut lists {
                    if output.hits.len() < limit
                        && let Some(hit) = list.next()
                    {
                        output.hits.push(hit);
                    }
                }
                if output.hits.len() == before {
                    break;
                }
            }
        }
        // A language that matched nothing was often a wrong guess (`rust` in
        // a Python project). A ranked search goes on without it, and says so.
        // An exact search stays with it, so that the pages of one list are all
        // of one language, whatever the files became in between.
        if !exact
            && output.hits.is_empty()
            && let Some(language) = language
        {
            let mut wider = self.search_blocking(&SearchArgs {
                language: None,
                ..args.clone()
            })?;
            if !wider.hits.is_empty() {
                wider.query_tokens += output.query_tokens;
                wider.notes.push(format!(
                    "nothing matched in {language}, so these results are from every language"
                ));
                return Ok(wider);
            }
        }
        if output.total == Some(0) {
            output.notes.push(match language {
                Some(language) => format!(
                    "No matches in {language}. Search without the language filter to see every language."
                ),
                None => format!(
                    "lexical finds the chunks that hold the query as written{}. hybrid finds its words in any order, and rg finds punctuation alone and text inside a longer name",
                    if args.ignore_case {
                        ""
                    } else {
                        " (ignore_case=true matches its ASCII letters in any case)"
                    }
                ),
            });
        }
        Ok(output)
    }

    fn search_one(
        &self,
        entry: &Entry,
        manifest: &Manifest,
        query: &Query<'_>,
        output: &mut SearchOutput,
    ) -> Result<Vec<Hit>, String> {
        let name = &manifest.name;
        let state = entry.layout.state();
        let status = match index2::status(&state) {
            Ok(status) => status,
            Err(error) => {
                output.notes.push(format!(
                    "{name} isn't built yet ({error}), and index_manage action=rebuild builds it"
                ));
                return Ok(Vec::new());
            }
        };
        let info = status.dense.clone();
        let vectors = info.as_ref().map_or(0, |info| info.embedded);
        let mut mode = match query.mode.unwrap_or(ModeArg::Hybrid) {
            ModeArg::Lexical => Mode::Lexical,
            ModeArg::Hybrid => Mode::Hybrid,
            ModeArg::Semantic => Mode::Dense,
        };
        if mode != Mode::Lexical {
            let reason = match &info {
                None if manifest.embed => Some(format!(
                    "{name} has no embeddings (index_manage action=embed adds them)"
                )),
                None => None,
                Some(info) if !self.backend().can_embed(&info.embedder) => Some(format!(
                    "semantic search needs {}, which the user can add with /keys",
                    embed::KEY_NAME
                )),
                Some(_) if vectors == 0 => Some(format!(
                    "{name} has no vectors yet ({})",
                    if status.embedding {
                        "embedding in progress"
                    } else {
                        "embedding pending"
                    }
                )),
                Some(_) => None,
            };
            if let Some(reason) = reason {
                output
                    .notes
                    .push(format!("{reason}, so it searched by keywords"));
            }
            let usable = vectors > 0
                && info
                    .as_ref()
                    .is_some_and(|info| self.backend().can_embed(&info.embedder));
            if !usable {
                mode = Mode::Lexical;
            }
        }
        if let Some(info) = &info
            && mode != Mode::Lexical
            && vectors < info.docs
        {
            let why = if status.embedding {
                "embedding in progress"
            } else if manifest.cap_reached {
                "the last embedding run stopped at its token cap, and index_manage action=embed continues"
            } else {
                "the rest are queued"
            };
            output.notes.push(format!(
                "{name} has vectors for {}/{} chunks ({why}). The others are found by keywords only",
                fmt_count(vectors),
                fmt_count(info.docs)
            ));
        }
        note_pending(entry, name, output);
        let embedder = match (&info, mode) {
            (Some(info), Mode::Dense | Mode::Hybrid) => Some(self.backend().embedder(
                &info.embedder,
                &self.handle,
                None,
            )?),
            _ => None,
        };
        let request = SearchRequest::new(query.text)
            .with_mode(mode)
            .with_limit(query.limit)
            .with_filter(query.filter.clone());
        let results = match hybrid::search(&state, &request, embedder.as_deref()) {
            Ok(results) => results,
            Err(error) if mode != Mode::Lexical => {
                output.notes.push(format!(
                    "Semantic search in {name} failed ({error}), so it searched by keywords"
                ));
                mode = Mode::Lexical;
                let request = SearchRequest::new(query.text)
                    .with_mode(Mode::Lexical)
                    .with_limit(query.limit)
                    .with_filter(query.filter.clone());
                hybrid::search(&state, &request, None).map_err(|error| error.to_string())?
            }
            Err(error) => return Err(format!("{name} couldn't be searched ({error})")),
        };
        if results.query_tokens > 0 {
            output.query_tokens += results.query_tokens;
            let model = info
                .as_ref()
                .map(|info| info.embedder.model.clone())
                .unwrap_or_default();
            self.add_spent(
                results.query_tokens,
                embed::cost(&model, results.query_tokens),
            );
        }
        output.searched.push((name.clone(), manifest.kind, mode));
        let built_at = manifest.last_build.as_ref().and_then(|record| record.at);
        Ok(results
            .hits
            .into_iter()
            .map(|hit| {
                let mut hit = hit_of(manifest, hit.chunk, query.cwd);
                hit.changed = changed_since(&hit.file, built_at);
                hit
            })
            .collect())
    }

    /// The matches of an exact search in one index (see
    /// [`lexical`](super::lexical)), in path and line order, meaning the
    /// candidates arko_index finds by the query's words whose text holds the
    /// query as written, as their file is now.
    fn search_exact(
        &self,
        entry: &Entry,
        manifest: &Manifest,
        query: &Query<'_>,
        literal: &Literal,
        output: &mut SearchOutput,
    ) -> Result<Vec<Hit>, String> {
        let name = &manifest.name;
        let state = entry.layout.state();
        let docs = match index2::status(&state) {
            Ok(status) => status.docs,
            Err(error) => {
                output.notes.push(format!(
                    "{name} isn't built yet ({error}), and index_manage action=rebuild builds it"
                ));
                return Ok(Vec::new());
            }
        };
        note_pending(entry, name, output);
        let candidates = every_candidate(&state, query.text, &query.filter, docs)
            .map_err(|error| format!("{name} couldn't be searched ({error})"))?;
        output
            .searched
            .push((name.clone(), manifest.kind, Mode::Lexical));
        let mut files: std::collections::BTreeMap<String, Vec<Chunk>> = Default::default();
        for hit in candidates {
            files
                .entry(hit.chunk.path.clone())
                .or_default()
                .push(hit.chunk);
        }
        let built_at = manifest.last_build.as_ref().and_then(|record| record.at);
        let mut hits = Vec::new();
        // The files that couldn't be checked, each with why.
        let mut unread: Vec<String> = Vec::new();
        for (path, mut chunks) in files {
            let file = manifest.root.join(&path);
            let text = match std::fs::read_to_string(&file) {
                // (A NUL makes a file binary, as the index reads it.)
                Ok(text) if text.contains('\0') => continue,
                Ok(text) => text,
                // (A file gone, or no longer text, holds nothing.)
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
                    ) =>
                {
                    continue;
                }
                Err(error) => {
                    unread.push(format!("{path} ({error})"));
                    continue;
                }
            };
            // The index's chunks of a file changed since may not fit it any
            // more, so it is chunked again, and all its chunks are checked.
            let changed = changed_since(&file, built_at);
            if changed {
                match arko_index::bench_v1::chunk_str(
                    Path::new(&path),
                    &text,
                    &chunk_options(manifest.kind),
                ) {
                    Ok(now) => {
                        chunks = now
                            .into_iter()
                            .filter(|chunk| query.filter.matches(&chunk.path, &chunk.language))
                            .collect();
                    }
                    Err(error) => {
                        unread.push(format!("{path} ({error})"));
                        continue;
                    }
                }
            }
            chunks.sort_by_key(|chunk| chunk.start_byte);
            let found = literal.matches(&text, &chunks);
            let starts = columns(&text, found.iter().map(|(chunk, _)| chunk.start_byte));
            for ((chunk, lines), column) in found.into_iter().zip(starts) {
                let mut hit = hit_of(manifest, chunk.clone(), query.cwd);
                hit.changed = changed;
                hit.column = column;
                hit.lines = Some(lines);
                hits.push(hit);
            }
        }
        if !unread.is_empty() {
            let count = fmt_counted(unread.len() as u64, "file", "files");
            let shown = unread.len().min(UNREAD_NAMED);
            let mut which = unread[..shown].join(", ");
            if unread.len() > shown {
                which.push_str(&format!(" and {} more", unread.len() - shown));
            }
            output.notes.push(format!(
                "The results and their count leave out {count} of {name} that couldn't be read, {which}"
            ));
        }
        Ok(hits)
    }

    // ----- status -----

    /// The state of the index `name` names, or of every index (blocking).
    pub(crate) fn status_blocking(
        &self,
        name: Option<&str>,
        cwd: &Path,
    ) -> Result<Vec<IndexReport>, String> {
        let scanned = match name.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) if Path::new(name) == Path::new(".") => {
                let related = self.related(cwd);
                if related.is_empty() {
                    return Err(self.no_index_message(cwd));
                }
                related
                    .into_iter()
                    .map(|found| store::Scanned {
                        layout: found.layout,
                        manifest: Ok(found.manifest),
                    })
                    .collect()
            }
            Some(name) => {
                let found = self.resolve(name, cwd)?;
                vec![store::Scanned {
                    layout: found.layout,
                    manifest: Ok(found.manifest),
                }]
            }
            None => store::scan(&self.indexes_dir()),
        };
        Ok(scanned
            .into_iter()
            .map(|scanned| self.report(scanned.layout, scanned.manifest))
            .collect())
    }

    fn report(&self, layout: Layout, manifest: Result<Manifest, String>) -> IndexReport {
        let entry = lock(&self.entries).get(&layout.id()).cloned();
        let (watch, pending, running, last) = match &entry {
            Some(entry) => {
                let live = lock(&entry.live);
                let slot = lock(&entry.embed);
                (
                    live.watch.clone(),
                    if live.dirty { live.changes } else { 0 },
                    slot.running,
                    slot.last.clone(),
                )
            }
            None => (WatchState::Off, 0, false, None),
        };
        // This process learns that another one watches only when it tries to
        // watch or refresh the index, which a status doesn't.
        let watch = if watch == WatchState::Off && monitor::watched_elsewhere(&layout) {
            WatchState::Elsewhere
        } else {
            watch
        };
        let (chunks, status_error, dense, active, progress) = match index2::status(&layout.state())
        {
            Ok(status) => (
                Some(status.docs),
                None,
                status.dense,
                status.embedding,
                status.progress,
            ),
            Err(error) => (None, Some(error.to_string()), None, false, None),
        };
        IndexReport {
            id: layout.id(),
            dir: layout.dir.clone(),
            manifest,
            watch,
            chunks,
            status_error,
            dense,
            embedding_active: active,
            progress,
            pending_changes: pending,
            embed_running_here: running,
            last_embed: last,
            has_key: self.backend().has_key(),
        }
    }

    // ----- create -----

    /// Creates an index (or updates the settings of the index of the same
    /// directory and kind), builds it, estimates and starts its embedding, and
    /// watches it when it is monitored (blocking). For `arko index` and
    /// tests, since the agents' tools take a stop.
    #[cfg(any(feature = "tui", test))]
    pub(crate) fn create_blocking(
        self: &Arc<Self>,
        args: &CreateArgs,
    ) -> Result<CreateOutcome, String> {
        self.create_stopping_with(args, None)
    }

    /// [`create_blocking`](Self::create_blocking), whose embedding, when it
    /// waits for it, stops when `stop` fires (the call was stopped).
    pub(crate) fn create_stopping_with(
        self: &Arc<Self>,
        args: &CreateArgs,
        stop: Option<Arc<StopSignal>>,
    ) -> Result<CreateOutcome, String> {
        let root =
            store::validate_root(&args.path, &args.cwd, self.home.as_deref(), &self.data_root)?;
        let rules = Rules::with(args.include.clone(), args.exclude.clone());
        rules.validate()?;
        let (kind, census) = match args.kind {
            Some(kind) => (kind, None),
            None => {
                let census = census::census(&root, &rules, census::CENSUS_LIMIT)?;
                (census.kind(), Some(census))
            }
        };
        let id = store::index_id(&root, kind);
        let layout = Layout::new(self.indexes_dir().join(&id));
        let existing = store::load_manifest(&layout.dir).ok();
        let others: Vec<Found> = self
            .all()
            .into_iter()
            .filter(|found| found.layout.id() != id)
            .collect();
        let taken = |name: &str| others.iter().any(|found| found.manifest.name == name);
        let name = match &args.name {
            Some(name) => {
                let name = store::validate_name(name)?;
                if taken(&name) {
                    return Err(format!("another index is already named {name:?}"));
                }
                name
            }
            None => match &existing {
                Some(existing) if !existing.name.is_empty() && !taken(&existing.name) => {
                    existing.name.clone()
                }
                _ => {
                    let base = root
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .filter(|name| store::validate_name(name).is_ok())
                        .unwrap_or_else(|| store::slug(&root));
                    let with_kind = format!("{base}-{}", kind.as_str());
                    if !taken(&base) {
                        base
                    } else if !taken(&with_kind) {
                        with_kind
                    } else {
                        id.clone()
                    }
                }
            },
        };
        // Embeddings are on unless asked otherwise (or turned off before). Without
        // a key the index is lexical until one is set, then Arko embeds it.
        let embed = args
            .embed
            .unwrap_or_else(|| existing.as_ref().is_none_or(|manifest| manifest.embed));
        let entry = self.entry(&layout);
        // A watcher with the old settings stops, and it restarts with the new ones.
        if let Some(thread) = self.stop_watch(&entry) {
            let _ = thread.join();
        }
        if existing
            .as_ref()
            .is_some_and(|manifest| manifest.embed && !embed)
        {
            for run in lock(&entry.runs).iter() {
                run.stop("the index no longer embeds");
            }
        }
        let model = self.backend().model_for(kind);
        let manifest = {
            let _guard = lock(&MANIFESTS);
            // Read again, since another session may have saved a build or
            // what an embedding run spent since the read above, which stay.
            let mut manifest = store::load_manifest(&layout.dir).unwrap_or_default();
            manifest.version = store::MANIFEST_VERSION;
            manifest.name = name;
            manifest.root = root.clone();
            manifest.kind = kind;
            manifest.model = model;
            manifest.embed = embed;
            manifest.rules = rules;
            manifest.monitor = args.monitor;
            if let Some(cap) = args.max_embed_tokens {
                manifest.max_embed_tokens = cap;
            }
            manifest.created = manifest.created.or(Some(Utc::now()));
            manifest.build_error = None;
            manifest.embed_error = None;
            manifest.cap_reached = false;
            store::save_manifest(&layout.dir, &manifest)?;
            manifest
        };
        let mut notes = Vec::new();
        let built = {
            let _guard = lock(&entry.build_lock);
            // The watcher's state is brought up to date with the build, so that
            // watching (now or later) starts from it.
            let opened = monitor::open_changes(
                &root,
                &layout,
                &self.file_rules(&manifest),
                kind == Kind::Docs,
                self.timing.force_polling,
            );
            match opened {
                Ok(mut changes) => {
                    let last = monitor::drain(changes.as_mut());
                    let built = self.build_locked(&entry, &manifest, Caller::Interactive);
                    match (&last, &built) {
                        (Ok(Some(sequence)), Ok(_)) => {
                            if let Err(error) = changes.ack(*sequence) {
                                notes.push(format!("the watcher's state failed ({error})"));
                            }
                        }
                        (Err(error), _) => {
                            notes.push(format!("the watcher's state failed ({error})"))
                        }
                        _ => {}
                    }
                    built
                }
                Err(OpenError::Busy) => {
                    lock(&entry.live).watch = WatchState::Elsewhere;
                    notes.push("another Arko process is watching this index".into());
                    self.build_locked(&entry, &manifest, Caller::Interactive)
                }
                Err(OpenError::Failed(error)) => {
                    notes.push(format!("the watcher failed ({error})"));
                    self.build_locked(&entry, &manifest, Caller::Interactive)
                }
            }
        };
        let record = match built {
            Ok(record) => record,
            Err(error) => {
                if existing.is_none() {
                    let _ = std::fs::remove_dir_all(&layout.dir);
                    lock(&self.entries).remove(&id);
                }
                return Err(format!("building the index failed ({error})"));
            }
        };
        let manifest = store::load_manifest(&layout.dir).unwrap_or(manifest);
        let mut estimate = None;
        let embedding = if !manifest.embed {
            EmbedPlan::Off
        } else if !self.backend().has_key() {
            EmbedPlan::NoKey
        } else {
            estimate = Some(self.estimate(&entry));
            if args.defer_embed {
                EmbedPlan::Deferred
            } else if args.wait {
                EmbedPlan::Done(self.run_embed(&entry, manifest.max_embed_tokens, true, None, stop))
            } else if self.request_embed(&entry, None, true) {
                EmbedPlan::Background
            } else {
                EmbedPlan::Done(Err("couldn't start embedding".into()))
            }
        };
        if manifest.monitor {
            if self.monitor {
                self.start_watch(&entry);
            } else {
                notes.push(
                    "it will be monitored while an interactive Arko runs in its directory".into(),
                );
            }
        }
        let watch = lock(&entry.live).watch.clone();
        Ok(CreateOutcome {
            layout,
            manifest,
            updated: existing.is_some(),
            census,
            record,
            estimate,
            embedding,
            watch,
            notes,
        })
    }

    // ----- manage -----

    /// Runs `action` on the index `name` names (blocking), and reports it.
    /// For `arko index` and tests, since the agents' tools take a stop.
    #[cfg(any(feature = "tui", test))]
    pub(crate) fn manage_blocking(
        self: &Arc<Self>,
        name: &str,
        action: &Action,
        cwd: &Path,
    ) -> Result<String, String> {
        self.manage_stopping_with(name, action, cwd, None)
    }

    /// [`manage_blocking`](Self::manage_blocking), whose embedding, when it
    /// waits for it, stops when `stop` fires (the call was stopped).
    pub(crate) fn manage_stopping_with(
        self: &Arc<Self>,
        name: &str,
        action: &Action,
        cwd: &Path,
        stop: Option<Arc<StopSignal>>,
    ) -> Result<String, String> {
        // Removing takes an index's own root, not a path in it.
        let found = self.resolve_in(name, cwd, !matches!(action, Action::Remove))?;
        let entry = self.entry(&found.layout);
        let name = found.manifest.name.clone();
        match action {
            Action::Rebuild => {
                let record = self.build(&entry, &found.manifest, Caller::Interactive)?;
                let mut text = format!(
                    "Rebuilt {name} with {} ({:.1} s). {}.",
                    fmt_counted(record.chunks, "chunk", "chunks"),
                    record.seconds,
                    record.summary
                );
                if self.request_embed(&entry, None, false) {
                    text.push_str(" Embedding the changed chunks in the background.");
                }
                Ok(text)
            }
            Action::Estimate => {
                let estimate = self.estimate(&entry)?;
                Ok(format!("For {name}, {}", estimate.line()))
            }
            Action::Embed { max_tokens, wait } => {
                if !self.backend().has_key() {
                    return Err(format!(
                        "embedding needs {}, which the user can add with /keys",
                        embed::KEY_NAME
                    ));
                }
                let mut lines = Vec::new();
                let (manifest, record) = self.ensure_dense(&found)?;
                if let Some(record) = record {
                    lines.push(format!(
                        "Rebuilt {name} with embeddings enabled ({}).",
                        fmt_counted(record.chunks, "chunk", "chunks")
                    ));
                }
                match self.estimate(&entry) {
                    Ok(estimate) => {
                        lines.push(format!("{}.", crate::ui::sentence(&estimate.line())))
                    }
                    Err(error) => lines.push(format!("No estimate ({error}).")),
                }
                let cap = max_tokens.unwrap_or(manifest.max_embed_tokens);
                if *wait {
                    let summary = self.run_embed(&entry, cap, true, None, stop)?;
                    lines.push(format!("{name} {}.", summary.line()));
                } else if self.request_embed(&entry, Some(cap), true) {
                    lines.push(format!(
                        "Embedding {name} in the background (cap {} tokens); index_status shows progress.",
                        fmt_count(cap)
                    ));
                }
                Ok(lines.join("\n"))
            }
            Action::Monitor(on) => {
                self.update_manifest(&found.layout, |m| m.monitor = *on)
                    .ok_or("the index's manifest can't be read")?;
                if !on {
                    if let Some(thread) = self.stop_watch(&entry) {
                        let _ = thread.join();
                    }
                    return Ok(format!(
                        "{name} is no longer monitored, so it is brought up to date when searched."
                    ));
                }
                if self.monitor {
                    self.start_watch(&entry);
                    Ok(format!(
                        "{name} is monitored, so this Arko watches it now, as will interactive Arko sessions in its directory."
                    ))
                } else {
                    Ok(format!(
                        "{name} is monitored by interactive Arko sessions in its directory."
                    ))
                }
            }
            Action::Remove => {
                self.remove_entry(&entry);
                std::fs::remove_dir_all(&found.layout.dir).map_err(|error| {
                    format!("could not remove {} ({error})", found.layout.dir.display())
                })?;
                Ok(format!(
                    "Removed the index {name} of {}. The indexed files are untouched.",
                    found.manifest.root.display()
                ))
            }
        }
    }

    /// Turns embeddings on for an index built without them (rebuilding it with
    /// its model). Returns the manifest, and the build if one ran.
    pub(crate) fn ensure_dense(
        &self,
        found: &Found,
    ) -> Result<(Manifest, Option<BuildRecord>), String> {
        let entry = self.entry(&found.layout);
        let lexical = index2::dense_info(&entry.layout.state())
            .ok()
            .flatten()
            .is_none();
        if found.manifest.embed && !lexical {
            return Ok((found.manifest.clone(), None));
        }
        let manifest = self
            .update_manifest(&found.layout, |m| {
                m.embed = true;
                m.cap_reached = false;
            })
            .ok_or("the index's manifest can't be read")?;
        let record = self.build(&entry, &manifest, Caller::Interactive)?;
        Ok((manifest, Some(record)))
    }

    /// Stops an index's watcher and runs, joins them, and forgets the index.
    fn remove_entry(&self, entry: &Entry) {
        lock(&entry.embed).again = false;
        for run in lock(&entry.runs).iter() {
            run.stop("the index is being removed");
        }
        if let Some(thread) = self.stop_watch(entry) {
            let _ = thread.join();
        }
        let thread = lock(&entry.embed).thread.take();
        if let Some(thread) = thread {
            // A run may have started meanwhile, and it stops after its current request.
            for run in lock(&entry.runs).iter() {
                run.stop("the index is being removed");
            }
            let _ = thread.join();
        }
        let _guard = lock(&entry.build_lock);
        lock(&self.entries).remove(&entry.layout.id());
    }

    // ----- for the CLI -----

    /// An embedding run in the foreground, with progress (blocking).
    #[cfg(feature = "tui")]
    pub(crate) fn embed_foreground(
        &self,
        found: &Found,
        cap: u64,
        progress: &(dyn Fn(&EmbedProgress) + Sync),
        stop: Arc<StopSignal>,
    ) -> Result<EmbedSummary, String> {
        let entry = self.entry(&found.layout);
        self.run_embed(&entry, cap, true, Some(progress), Some(stop))
    }

    #[cfg(feature = "tui")]
    pub(crate) fn estimate_of(&self, found: &Found) -> Result<Estimate, String> {
        self.estimate(&self.entry(&found.layout))
    }

    /// Reconciles the index with its tree (rebuilding when anything changed),
    /// as a search would.
    #[cfg(feature = "tui")]
    pub(crate) fn update_of(&self, found: &Found) -> Result<Option<BuildRecord>, String> {
        let entry = self.entry(&found.layout);
        lock(&entry.live).last_reconcile = None;
        self.reconcile(&entry, &found.manifest)
    }

    #[cfg(feature = "tui")]
    pub(crate) fn rebuild_of(&self, found: &Found) -> Result<BuildRecord, String> {
        self.build(
            &self.entry(&found.layout),
            &found.manifest,
            Caller::Interactive,
        )
    }

    /// Starts watching `found` (the CLI's `watch`, which needs a monitoring instance).
    #[cfg(feature = "tui")]
    pub(crate) fn watch_index(self: &Arc<Self>, found: &Found) {
        let entry = self.entry(&found.layout);
        self.start_watch(&entry);
    }

    #[cfg(test)]
    pub(crate) fn live_state(&self, layout: &Layout) -> (WatchState, bool, u64, u64) {
        let entry = self.entry(layout);
        let live = lock(&entry.live);
        (live.watch.clone(), live.dirty, live.delivered, live.covered)
    }
}

/// One index's search.
struct Query<'a> {
    text: &'a str,
    mode: Option<ModeArg>,
    limit: usize,
    filter: Filter,
    cwd: &'a Path,
}

/// A page of an exact search's matches.
struct Page {
    hits: Vec<Hit>,
    /// The matches in all, and how many of them come at or before the cursor.
    total: usize,
    before: usize,
    /// The page after it, when the list goes on.
    next: Option<NextPage>,
}

/// The page of `matches` (from every index searched) that starts after the
/// cursor and holds `limit` of them, or more where the page can't end. They
/// are in the order of their places in files, and by index where two indexes
/// have a result at one place. A cursor names a place, so a page doesn't end
/// between those, and may run over the limit by the results of the other
/// indexes at its last place.
fn page_of(mut matches: Vec<Hit>, after: Option<&Cursor>, limit: usize) -> Page {
    matches.sort_by(|a, b| {
        a.place()
            .cmp(&b.place())
            .then_with(|| a.index.cmp(&b.index))
    });
    let total = matches.len();
    let before = after.map_or(0, |cursor| {
        matches.partition_point(|hit| {
            let (path, line, column) = hit.place();
            cursor.reaches(path, line, column)
        })
    });
    let end = page_end(&matches, before, limit);
    let next = (end < total).then(|| {
        let (path, line, column) = matches[end - 1].place();
        NextPage {
            cursor: Cursor::at(path, line, column),
            count: page_end(&matches, end, limit) - end,
        }
    });
    matches.truncate(end);
    Page {
        hits: matches.split_off(before),
        total,
        before,
        next,
    }
}

/// Where the page of `matches` that starts at `start` ends: `limit` further
/// on, and past the results at the place that ends it.
fn page_end(matches: &[Hit], start: usize, limit: usize) -> usize {
    let mut end = start.saturating_add(limit).min(matches.len());
    while end > start && end < matches.len() && matches[end].place() == matches[end - 1].place() {
        end += 1;
    }
    end
}

/// Every chunk of the index at `state` that holds a word of `query` (and
/// passes `filter`), by BM25, when its generation had `docs` chunks as last
/// read. A build may have published a larger one since, so a list that comes
/// back full is asked for again with twice as many.
pub(crate) fn every_candidate(
    state: &Path,
    query: &str,
    filter: &Filter,
    docs: u64,
) -> Result<Vec<hybrid::SearchHit>, arko_index::bench_v1::Error> {
    let mut depth = usize::try_from(docs).unwrap_or(usize::MAX / 4) + 1;
    loop {
        let request = SearchRequest::new(query)
            .with_mode(Mode::Lexical)
            .with_limit(depth)
            .with_filter(filter.clone());
        let results = hybrid::search(state, &request, None)?;
        if results.hits.len() < depth {
            return Ok(results.hits);
        }
        depth = depth.saturating_mul(2);
    }
}

/// Notes that the index's files changed since its last build, when they did.
fn note_pending(entry: &Entry, name: &str, output: &mut SearchOutput) {
    let live = lock(&entry.live);
    if live.dirty && live.changes > 0 {
        output.notes.push(format!(
            "Files of {name} changed since the last build, so a rebuild is pending ({})",
            live.watch.describe()
        ));
    }
}

/// The result of `chunk` of the index `manifest` describes, shown relative to
/// `cwd` when it lies under it.
fn hit_of(manifest: &Manifest, chunk: Chunk, cwd: &Path) -> Hit {
    let file = manifest.root.join(&chunk.path);
    let display = match file.strip_prefix(cwd) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_string_lossy().into_owned(),
        _ => file.to_string_lossy().into_owned(),
    };
    Hit {
        index: manifest.name.clone(),
        changed: false,
        display,
        file,
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        start_byte: chunk.start_byte,
        end_byte: chunk.end_byte,
        column: 1,
        scope: chunk.scope,
        symbol: chunk.symbol_path,
        lines: None,
    }
}

/// How the indexes of `kind` chunk their files.
pub(crate) fn chunk_options(kind: Kind) -> Options {
    Options {
        front_matter_description: kind == Kind::Docs,
        ..Options::default()
    }
}

fn changed_since(file: &Path, built: Option<DateTime<Utc>>) -> bool {
    let Some(built) = built else {
        return false;
    };
    match std::fs::metadata(file).and_then(|meta| meta.modified()) {
        Ok(modified) => DateTime::<Utc>::from(modified) > built,
        Err(_) => true,
    }
}

impl EmbedSummary {
    pub fn line(&self) -> String {
        let mut line = format!(
            "embedded {} for {} tokens ({}), and {}/{} chunks have vectors",
            fmt_counted(self.new, "text", "texts"),
            fmt_count(self.tokens),
            fmt_dollars(self.dollars),
            fmt_count(self.embedded),
            fmt_count(self.docs),
        );
        if self.cap_reached {
            line.push_str(&format!(
                ". The run stopped at the cap of {} tokens (~{} tokens are left, and embedding again with a higher max_tokens continues)",
                fmt_count(self.cap),
                fmt_count(self.not_embedded_tokens)
            ));
        } else if self.stopped {
            line.push_str(". The run stopped before it finished (a later run continues)");
        }
        if self.too_long > 0 {
            line.push_str(&format!(
                ". {} too long to embed (found by keywords)",
                match self.too_long {
                    1 => "1 chunk is".to_string(),
                    many => format!("{} are", fmt_counted(many, "chunk", "chunks")),
                }
            ));
        }
        line
    }
}

impl Estimate {
    pub fn line(&self) -> String {
        if self.tokens == 0 {
            return format!(
                "nothing to embed, as {}/{} chunks have vectors",
                fmt_count(self.embedded),
                fmt_count(self.docs)
            );
        }
        format!(
            "embedding {} would take ~{} tokens (~{} with {}), and {}/{} chunks have vectors",
            fmt_counted(self.texts, "chunk", "chunks"),
            fmt_count(self.tokens),
            fmt_dollars(self.dollars),
            self.model,
            fmt_count(self.embedded),
            fmt_count(self.docs)
        )
    }
}

/// `1234567` → `1,234,567`.
pub fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A count with its thousands apart and its noun, as `1 chunk`, `1,240
/// chunks`.
pub fn fmt_counted(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", fmt_count(n), crate::remote::noun(n, one, many))
}

/// What a build made, as `1 chunk in 1 file`, `1,240 chunks in 85 files`.
pub fn chunks_in_files(chunks: u64, files: u64) -> String {
    format!(
        "{} in {}",
        fmt_counted(chunks, "chunk", "chunks"),
        fmt_counted(files, "file", "files")
    )
}

/// Dollars, as `$0.14`, `$0.0012`, `<$0.0001`, `$0`.
pub fn fmt_dollars(dollars: f64) -> String {
    if dollars <= 0.0 {
        "$0".into()
    } else if dollars < 0.0001 {
        "<$0.0001".into()
    } else if dollars < 0.01 {
        format!("${dollars:.4}")
    } else {
        format!("${dollars:.2}")
    }
}

/// How long ago `at` was, as `just now`, `5 min ago`, `3 h ago`, `1 day ago`,
/// `2 days ago`.
pub fn fmt_ago(at: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - at).num_seconds().max(0);
    match seconds {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        _ => format!(
            "{} ago",
            fmt_counted(seconds as u64 / 86_400, "day", "days")
        ),
    }
}

/// The state of an index's embeddings, in words.
pub fn describe_dense(info: &DenseIndexInfo) -> &'static str {
    match info.state {
        EmbedState::Complete if info.complete() => "complete",
        EmbedState::Complete => "complete (some chunks are too long to embed)",
        EmbedState::Embedding => "incomplete",
        EmbedState::CapReached => "incomplete (a run stopped at its cap)",
    }
}

/// A git linked worktree's checkout root and the main repository root that
/// owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Worktree {
    root: PathBuf,
    main_root: PathBuf,
}

impl Worktree {
    /// Maps a path inside this worktree to the corresponding path in its
    /// main repository.
    fn rebase(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(&self.root) {
            Ok(relative) if relative.as_os_str().is_empty() => self.main_root.clone(),
            Ok(relative) => self.main_root.join(relative),
            Err(_) => self.main_root.clone(),
        }
    }
}

/// Canonicalizes `path` if it exists, or canonicalizes its deepest existing
/// ancestor and appends the remaining components.
fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    let mut tail = Vec::new();
    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        if let Some(name) = cursor.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(mut canon) = parent.canonicalize() {
            for part in tail.into_iter().rev() {
                canon.push(part);
            }
            return canon;
        }
        cursor = parent;
    }
    path.to_path_buf()
}

/// If `path` (or an ancestor) is a git linked worktree (`.git` file pointing
/// into `.../.git/worktrees/<name>`), returns its worktree root and main
/// repository root. Stops at a regular `.git` directory.
fn worktree_of(path: &Path) -> Option<Worktree> {
    let start = if path.is_file() { path.parent()? } else { path };
    let start = canonicalize_best_effort(start);
    for ancestor in start.ancestors() {
        let dot_git = ancestor.join(".git");
        let Ok(meta) = std::fs::metadata(&dot_git) else {
            continue;
        };
        if meta.is_dir() {
            return None;
        }
        if !meta.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let gitdir = content
            .lines()
            .find_map(|line| line.trim().strip_prefix("gitdir:"))?
            .trim();
        if gitdir.is_empty() {
            return None;
        }
        let gitdir = canonicalize_best_effort(&store::absolute(Path::new(gitdir), ancestor));
        let main_root = main_repo_from_worktree_gitdir(&gitdir)?;
        let main_root = canonicalize_best_effort(&main_root);
        return Some(Worktree {
            root: canonicalize_best_effort(ancestor),
            main_root,
        });
    }
    None
}

/// Resolves the main repository root from a worktree's `gitdir`
/// (`<main>/.git/worktrees/<name>`). Uses `commondir` when present, else
/// inspects the path components for `.git/worktrees/<name>`.
fn main_repo_from_worktree_gitdir(gitdir: &Path) -> Option<PathBuf> {
    if let Ok(common) = std::fs::read_to_string(gitdir.join("commondir")) {
        let common = common.trim();
        if !common.is_empty() {
            let common_path = canonicalize_best_effort(&store::absolute(Path::new(common), gitdir));
            if common_path.file_name() == Some(std::ffi::OsStr::new(".git")) {
                return common_path.parent().map(Path::to_path_buf);
            }
        }
    }
    let mut cursor = gitdir.parent();
    while let Some(dir) = cursor {
        if dir.file_name() == Some(std::ffi::OsStr::new("worktrees")) {
            let dot_git = dir.parent()?;
            if dot_git.file_name() == Some(std::ffi::OsStr::new(".git")) {
                return dot_git.parent().map(Path::to_path_buf);
            }
        }
        cursor = dir.parent();
    }
    None
}

/// Resolves a `path_prefix` against `root`, rebasing through a git worktree
/// or against `root` when an index was named explicitly with a relative prefix.
fn resolve_prefix(
    prefix: &str,
    cwd: &Path,
    cwd_worktree: Option<&Worktree>,
    root: &Path,
    named: bool,
) -> Result<PathBuf, PathBuf> {
    let prefix_path = Path::new(prefix);
    let wanted = canonicalize_best_effort(&store::absolute(prefix_path, cwd));
    let in_root = |candidate: &Path| candidate.starts_with(root) || root.starts_with(candidate);
    if in_root(&wanted) {
        return Ok(wanted);
    }
    if let Some(wt) = worktree_of(&wanted).or_else(|| cwd_worktree.cloned())
        && wanted.starts_with(&wt.root)
    {
        let rebased = wt.rebase(&wanted);
        if in_root(&rebased) {
            return Ok(rebased);
        }
    }
    if named && !prefix_path.is_absolute() {
        let rebased = canonicalize_best_effort(&store::absolute(prefix_path, root));
        if in_root(&rebased) {
            return Ok(rebased);
        }
    }
    Err(wanted)
}

// The tests hold a call with Linux's file descriptors.
#[cfg(all(test, target_os = "linux"))]
#[path = "engine_tests.rs"]
mod tests;

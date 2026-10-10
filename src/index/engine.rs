//! [`Indexes`]: fin's indexes in one process. It finds them (the folders under
//! `<data root>/indexes/`), creates, builds, embeds and searches them, keeps the
//! monitored ones current with watcher threads, and counts what embedding costs.
//!
//! Everything here blocks (arko_index works on files and calls the embedder
//! synchronously): async callers go through `spawn_blocking`, background work
//! runs on its own `std::thread`s, which [`Indexes::shutdown`] stops and joins.
//!
//! Freshness:
//! - A monitored index this process watches is rebuilt in the background once
//!   its tree has been quiet for [`Timing::quiet`] (2 s), at most every
//!   [`Timing::min_interval`] (30 s); the chunks that changed are embedded next.
//!   A search that finds changes not yet built rebuilds first when the last build
//!   took at most [`Timing::sync_rebuild_max`] (5 s); otherwise it notes them.
//! - Any other index (fin's one-shot `-p` mode, an index of another tree, or one
//!   not monitored) is reconciled when first searched (and again after
//!   [`Timing::reconcile_interval`]): its watcher state is opened, which finds
//!   what changed since it was last acknowledged, and the index is rebuilt when
//!   anything did. An index another fin process watches is left to it (a
//!   monitoring fin takes over once the other stops).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arko_index::bench_v1::Options;
use arko_index::dense::{DenseBuild, DenseIndexInfo, EmbedProgress, EmbedState, StopSignal};
use arko_index::hybrid::{self, Mode, SearchRequest};
use arko_index::index2::{self, BuildOptions};
use arko_index::vector::Filter;
use chrono::{DateTime, Utc};
use tokio::runtime::Handle;

use super::census::{self, Census};
use super::embed::{self, Backend};
use super::monitor::{self, OpenError};
use super::store::{self, BuildRecord, Kind, Layout, Manifest, Rules};

/// Receives progress lines of background work (the CLI prints them; fin logs them).
pub type Events = Arc<dyn Fn(&str) + Send + Sync>;

/// Tokens a search in fin's one-shot mode may embed to bring the chunks changed
/// since the last build up to date (about $0.02 with voyage-code-4); the rest stay
/// lexical until fin monitors the index or `fin index embed` runs.
pub const FRESHEN_EMBED_TOKENS: u64 = 200_000;
/// The most results a search returns.
pub const MAX_LIMIT: usize = 50;
/// Results by default.
pub const DEFAULT_LIMIT: usize = 10;

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
    /// trees whose look takes long: ten times its duration).
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
    /// Another process (another fin) holds the watcher state; a watcher thread
    /// of this one tries again every [`Timing::busy_retry`].
    Elsewhere,
    /// The watcher failed; the thread retries.
    Failed(String),
}

impl WatchState {
    pub fn describe(&self) -> String {
        match self {
            WatchState::Off => "not watched by this fin".into(),
            WatchState::Starting => "watcher starting".into(),
            WatchState::Watching { polling: false } => "watching (inotify)".into(),
            WatchState::Watching { polling: true } => "watching (polling)".into(),
            WatchState::Elsewhere => "watched by another fin process".into(),
            WatchState::Failed(error) => format!("watcher failed: {error}"),
        }
    }
    pub fn active(&self) -> bool {
        matches!(self, WatchState::Starting | WatchState::Watching { .. })
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
    /// Serializes builds (and watcher opens) in this process; arko_index's own
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
    /// A tool call or command: waits for another process's build.
    Interactive,
    /// A watcher thread: half the threads, no waiting.
    Background,
}

/// An index found on disk.
#[derive(Clone, Debug)]
pub struct Found {
    pub layout: Layout,
    pub manifest: Manifest,
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
    /// An index name, id or root path; none: every index of the directory.
    pub index: Option<String>,
    /// None: hybrid where the index has vectors and a key is set, else lexical.
    pub mode: Option<ModeArg>,
    pub limit: usize,
    /// A path (relative to `cwd`, or absolute) results must be under.
    pub path_prefix: Option<String>,
    pub language: Option<String>,
    pub cwd: PathBuf,
}

/// A search result.
#[derive(Clone, Debug)]
pub struct Hit {
    /// The index's name.
    pub index: String,
    /// The file as shown: relative to the search's directory when under it.
    pub display: String,
    pub file: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub scope: String,
    pub symbol: String,
    /// The file changed after the index was built.
    pub changed: bool,
}

/// What a search found, and how.
#[derive(Clone, Debug, Default)]
pub struct SearchOutput {
    pub hits: Vec<Hit>,
    /// Each index searched: its name, kind and the mode used.
    pub searched: Vec<(String, Kind, Mode)>,
    pub notes: Vec<String>,
    /// Tokens billed for query embeddings.
    pub query_tokens: u64,
}

/// An index's state, for status.
#[derive(Clone, Debug)]
pub struct IndexReport {
    pub id: String,
    pub dir: PathBuf,
    /// The manifest, or why it couldn't be read.
    pub manifest: Result<Manifest, String>,
    pub watch: WatchState,
    /// Chunks in the current generation (none: never built).
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

/// A free estimate (a run with a cap of 0 tokens): what embedding the chunks
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
    pub layout: Layout,
    pub manifest: Manifest,
    /// The index existed (same directory and kind): its settings were updated.
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
    /// None: by a census of the files.
    pub kind: Option<Kind>,
    pub name: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub monitor: bool,
    /// None: the existing index's setting, else whether a key is set.
    pub embed: Option<bool>,
    pub max_embed_tokens: Option<u64>,
    /// Embed before returning (else in the background).
    pub wait: bool,
    /// Estimate only: the caller embeds (the CLI, with progress).
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

/// fin's indexes, for one process: shared by the tools (through an `Arc`), the
/// watcher threads and the embedding threads.
pub struct Indexes {
    data_root: PathBuf,
    workspace: PathBuf,
    home: Option<PathBuf>,
    backend: Backend,
    monitor: bool,
    handle: Handle,
    timing: Timing,
    events: Option<Events>,
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    /// Serializes manifest updates in this process.
    manifests: Mutex<()>,
    spent: Mutex<(u64, f64)>,
    stopping: AtomicBool,
}

impl std::fmt::Debug for Indexes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Indexes")
            .field("data_root", &self.data_root)
            .field("workspace", &self.workspace)
            .field("backend", &self.backend)
            .field("monitor", &self.monitor)
            .finish_non_exhaustive()
    }
}

impl Indexes {
    /// fin's indexes, with their data under `data_root` (fin's data folder),
    /// `workspace` the directory fin works in, and the Voyage key (none:
    /// lexical indexes and search only). Call it in fin's tokio runtime. With
    /// `monitor`, every monitored index whose root contains the workspace or lies
    /// in it gets a watcher thread at once (it reconciles what changed while fin
    /// wasn't running); without (fin's one-shot `-p` mode), indexes are
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
            backend,
            monitor,
            handle,
            timing,
            events,
            entries: Mutex::new(HashMap::new()),
            manifests: Mutex::new(()),
            spent: Mutex::new((0, 0.0)),
            stopping: AtomicBool::new(false),
        });
        Ok(this)
    }

    /// The index's file rules, leaving out fin's index folder when it lies in
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
    /// instance monitors); the number started.
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
        self.backend.has_key()
    }

    /// One line for fin's banner: the indexes of the workspace, e.g.
    /// `index finn (code, monitored)`. None when there are none.
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
                    WatchState::Elsewhere => ", monitored by another fin",
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
        if !self.backend.has_key() && related.iter().any(|found| found.manifest.embed) {
            line.push_str(&format!(" · semantic search needs {}", embed::KEY_NAME));
        }
        Some(line)
    }

    /// Tokens billed for embeddings (indexing and queries) in this process, and
    /// their cost in dollars.
    pub fn spent(&self) -> (u64, f64) {
        *lock(&self.spent)
    }

    /// Stops the watcher threads and embedding runs (a run stops after its
    /// current request; the vectors paid for are kept) and joins the threads,
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
                run.stop("fin is exiting");
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

    /// The indexes whose root contains `dir` or lies in it.
    pub fn related(&self, dir: &Path) -> Vec<Found> {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        self.all()
            .into_iter()
            .filter(|found| {
                dir.starts_with(&found.manifest.root) || found.manifest.root.starts_with(&dir)
            })
            .collect()
    }

    /// The index `name` names: its name, its id, or its root (a path, relative to
    /// `cwd`; ambiguous when the directory has a code and a docs index).
    pub fn resolve(&self, name: &str, cwd: &Path) -> Result<Found, String> {
        let name = name.trim();
        let all = self.all();
        if let Some(found) = all.iter().find(|found| found.manifest.name == name) {
            return Ok(found.clone());
        }
        if let Some(found) = all.iter().find(|found| found.layout.id() == name) {
            return Ok(found.clone());
        }
        if let Ok(path) = store::absolute(Path::new(name), cwd).canonicalize() {
            let matching: Vec<&Found> = all
                .iter()
                .filter(|found| found.manifest.root == path)
                .collect();
            match matching.as_slice() {
                [one] => return Ok((*one).clone()),
                [] => {}
                many => {
                    let names: Vec<&str> = many
                        .iter()
                        .map(|found| found.manifest.name.as_str())
                        .collect();
                    return Err(format!(
                        "{} has several indexes: {}; name one",
                        path.display(),
                        names.join(", ")
                    ));
                }
            }
        }
        let names: Vec<&str> = all
            .iter()
            .map(|found| found.manifest.name.as_str())
            .collect();
        Err(if names.is_empty() {
            format!("no index named {name:?}: there are no indexes yet (create one)")
        } else {
            format!(
                "no index named {name:?}; the indexes are: {}",
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
            message.push_str("; there are no indexes yet: create one with index_create");
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
                "; name one of: {}; or create one with index_create",
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
        lock(&self.entries)
            .get(&layout.id())
            .map(|entry| lock(&entry.live).watch.clone())
            .unwrap_or_default()
    }

    /// Loads, changes and saves a manifest (nothing when it can't be read).
    fn update_manifest(
        &self,
        layout: &Layout,
        change: impl FnOnce(&mut Manifest),
    ) -> Option<Manifest> {
        let _guard = lock(&self.manifests);
        let mut manifest = store::load_manifest(&layout.dir).ok()?;
        change(&mut manifest);
        if let Err(error) = store::save_manifest(&layout.dir, &manifest) {
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

    /// Builds with the build lock held: records what the build covers, and the
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
                    m.last_error = None;
                });
            }
            Err(error) => {
                self.update_manifest(&entry.layout, |m| {
                    m.last_error = Some(format!("build failed: {error}"));
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
        let options = Options {
            front_matter_description: manifest.kind == Kind::Docs,
            ..Options::default()
        };
        let threads = match caller {
            Caller::Interactive => arko_index::threads(),
            Caller::Background => (arko_index::threads() / 2).max(1),
        };
        // An embedding index is built with its model (named only: a run embeds
        // the chunks afterwards), which keeps its vectors and reuses the cached
        // ones; a build without a model would remove them.
        let embedder = if manifest.embed {
            let descriptor = Backend::descriptor(&manifest.model)?;
            Some(self.backend.embedder(&descriptor, &self.handle, None)?)
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
                            "{error} (another fin process is building this index)"
                        ));
                    }
                    return Err(error);
                }
            }
        }
    }

    // ----- embedding -----

    /// One embedding run of the index's current generation, with a cap of `cap`
    /// tokens (0: a free estimate). `record` saves the run in the manifest.
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
        if self.stopping() {
            stop.stop("fin is exiting");
        }
        let embedder = self
            .backend
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
                        if m.last_error
                            .as_deref()
                            .is_some_and(|error| error.starts_with("embedding"))
                        {
                            m.last_error = None;
                        }
                    }
                    Err(error) => m.last_error = Some(format!("embedding failed: {error}")),
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

    /// Starts (or queues) a background embedding run of the index; whether one
    /// will run. Automatic runs (`explicit` false) don't follow a run that
    /// stopped at its cap.
    fn request_embed(
        self: &Arc<Self>,
        entry: &Arc<Entry>,
        cap: Option<u64>,
        explicit: bool,
    ) -> bool {
        if self.stopping() || !self.backend.has_key() {
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
        match std::thread::Builder::new()
            .name("fin-index-embed".into())
            .spawn(move || this.embed_loop(&worker))
        {
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
                Err(error) => format!("embedding skipped: {error}"),
                Ok(manifest) if manifest.cap_reached && !explicit => format!(
                    "{}: embedding paused: the last run stopped at its cap",
                    manifest.name
                ),
                Ok(manifest) => {
                    let cap = cap.unwrap_or(manifest.max_embed_tokens);
                    match self.run_embed(entry, cap, true, None, None) {
                        Ok(summary) => {
                            superseded = summary.superseded;
                            let line = format!("{}: {}", manifest.name, summary.line());
                            if summary.new > 0 || summary.cap_reached {
                                self.event(&line);
                            }
                            line
                        }
                        Err(error) => {
                            let line = format!("{}: embedding failed: {error}", manifest.name);
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
        match std::thread::Builder::new()
            .name("fin-index-watch".into())
            .spawn(move || this.watch_loop(&worker, &flag))
        {
            Ok(thread) => *watcher = Some(WatchThread { stop, thread }),
            Err(error) => {
                lock(&entry.live).watch = WatchState::Failed(error.to_string());
            }
        }
    }

    /// Stops the index's watcher thread and returns it to join.
    fn stop_watch(&self, entry: &Entry) -> Option<JoinHandle<()>> {
        let watch = lock(&entry.watcher).take()?;
        watch.stop.store(true, Ordering::SeqCst);
        Some(watch.thread)
    }

    /// A watcher thread: opens the watcher (reconciling the tree), then marks
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
                    lock(&entry.live).watch = WatchState::Failed(error.clone());
                    self.event(&format!("{name}: stopped watching: {error}"));
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
                            if !built || manifest.last_error.is_some() {
                                live.dirty = true;
                            }
                        }
                        acked = 0;
                        changes = Some(opened);
                        busy_reported = false;
                        self.event(&format!(
                            "{name}: watching {} ({})",
                            manifest.root.display(),
                            if polling { "polling" } else { "inotify" }
                        ));
                        if built && Self::embedding_incomplete(entry) {
                            self.request_embed(entry, None, false);
                        }
                    }
                    // Another process holds the watcher state: another fin
                    // watching it (this one takes over when it exits), or one
                    // reconciling it for a moment.
                    Err(OpenError::Busy) => {
                        lock(&entry.live).watch = WatchState::Elsewhere;
                        if !busy_reported {
                            busy_reported = true;
                            self.event(&format!(
                                "{name}: another fin process is watching it; this one takes over when it stops"
                            ));
                        }
                        reopen_at = Instant::now() + self.timing.busy_retry;
                        continue;
                    }
                    Err(OpenError::Failed(error)) => {
                        self.event(&format!("{name}: watcher failed: {error}"));
                        lock(&entry.live).watch = WatchState::Failed(error);
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
                        self.event(&format!("{name}: watcher failed: {error}; reopening"));
                        lock(&entry.live).watch = WatchState::Failed(error);
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
                            "{name}: rebuilt: {} chunks in {} files ({:.1} s)",
                            fmt_count(record.chunks),
                            fmt_count(record.files),
                            record.seconds
                        ));
                        if !stopped() {
                            self.request_embed(entry, None, false);
                        }
                    }
                    Err(error) => self.event(&format!("{name}: rebuild failed: {error}")),
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
        let mut live = lock(&entry.live);
        if live.watch.active() || live.watch == WatchState::Elsewhere {
            live.watch = WatchState::Off;
        }
    }

    /// Brings an index no watcher of this process keeps current up to date: opens
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
                if built && manifest.last_error.is_none() {
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
                let record = if last.is_some() || !built || manifest.last_error.is_some() {
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

    /// Before a search: rebuilds a watched index with pending changes when its
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
                    Err(error) => notes.push(format!("{name}: rebuild failed: {error}")),
                }
            }
            return;
        }
        match self.reconcile(entry, manifest) {
            Ok(Some(record)) => {
                notes.push(format!(
                    "{name}: files changed since it was last built: rebuilt before searching ({:.1} s)",
                    record.seconds
                ));
                if self.monitor {
                    self.request_embed(entry, None, false);
                } else if manifest.embed
                    && self.backend.has_key()
                    && !manifest.cap_reached
                    && Self::embedding_incomplete(entry)
                {
                    match self.run_embed(entry, FRESHEN_EMBED_TOKENS, false, None, None) {
                        Ok(summary) if summary.cap_reached => notes.push(format!(
                            "{name}: {}/{} chunks have vectors; the rest are found by keywords until an interactive fin monitors it (or `fin index embed`)",
                            fmt_count(summary.embedded),
                            fmt_count(summary.docs)
                        )),
                        Ok(_) => {}
                        Err(error) => {
                            notes.push(format!("{name}: embedding the changes failed: {error}"))
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(error) => notes.push(format!("{name}: couldn't check for changes: {error}")),
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
        let cwd = args.cwd.canonicalize().unwrap_or_else(|_| args.cwd.clone());
        let targets = self.targets(args.index.as_deref(), &cwd)?;
        let limit = args.limit.clamp(1, MAX_LIMIT);
        let prefix = args
            .path_prefix
            .as_deref()
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty());
        let language = args
            .language
            .as_deref()
            .map(str::trim)
            .filter(|language| !language.is_empty());
        let mut output = SearchOutput::default();
        let mut lists: Vec<Vec<Hit>> = Vec::new();
        for target in &targets {
            let root = &target.manifest.root;
            let mut filter = Filter::default();
            if let Some(prefix) = prefix {
                let wanted = store::absolute(Path::new(prefix), &cwd);
                let wanted = wanted.canonicalize().unwrap_or(wanted);
                match wanted.strip_prefix(root) {
                    Ok(relative) => {
                        let relative = relative.to_string_lossy().replace('\\', "/");
                        if !relative.is_empty() {
                            filter = filter.with_path_prefix(relative);
                        }
                    }
                    // The prefix contains the whole index.
                    Err(_) if root.starts_with(&wanted) => {}
                    Err(_) if targets.len() == 1 => {
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
            lists.push(self.search_one(&entry, &manifest, &query, &mut output)?);
        }
        // Interleaved by rank, so each index's best results come first.
        let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
        for rank in 0..longest {
            for list in &lists {
                if let Some(hit) = list.get(rank) {
                    output.hits.push(hit.clone());
                }
            }
        }
        output.hits.truncate(limit);
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
                    "{name}: not built yet ({error}); index_manage action=rebuild builds it"
                ));
                return Ok(Vec::new());
            }
        };
        let info = status.dense.clone();
        let vectors = info.as_ref().map_or(0, |info| info.embedded);
        let explicit = query.mode.is_some();
        let mut mode = match query.mode.unwrap_or(ModeArg::Hybrid) {
            ModeArg::Lexical => Mode::Lexical,
            ModeArg::Hybrid => Mode::Hybrid,
            ModeArg::Semantic => Mode::Dense,
        };
        if mode != Mode::Lexical {
            let reason = match &info {
                None if explicit || manifest.embed => Some(format!(
                    "{name} has no embeddings (index_manage action=embed adds them)"
                )),
                None => None,
                Some(info) if !self.backend.can_embed(&info.embedder) => Some(format!(
                    "semantic search needs {} (fin reads it from its .env; see fin --setup)",
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
                output.notes.push(format!("{reason}: searched by keywords"));
            }
            let usable = vectors > 0
                && info
                    .as_ref()
                    .is_some_and(|info| self.backend.can_embed(&info.embedder));
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
                "the last embedding run stopped at its token cap; index_manage action=embed continues"
            } else {
                "the rest are queued"
            };
            output.notes.push(format!(
                "{name}: {}/{} chunks have vectors ({why}); the others are found by keywords only",
                fmt_count(vectors),
                fmt_count(info.docs)
            ));
        }
        {
            let live = lock(&entry.live);
            if live.dirty && live.changes > 0 {
                output.notes.push(format!(
                    "{name}: files changed since the last build; a rebuild is pending ({})",
                    live.watch.describe()
                ));
            }
        }
        let embedder = match (&info, mode) {
            (Some(info), Mode::Dense | Mode::Hybrid) => {
                Some(self.backend.embedder(&info.embedder, &self.handle, None)?)
            }
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
                    "{name}: semantic search failed ({error}); searched by keywords"
                ));
                mode = Mode::Lexical;
                let request = SearchRequest::new(query.text)
                    .with_mode(Mode::Lexical)
                    .with_limit(query.limit)
                    .with_filter(query.filter.clone());
                hybrid::search(&state, &request, None).map_err(|error| error.to_string())?
            }
            Err(error) => return Err(format!("{name}: {error}")),
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
                let chunk = hit.chunk;
                let file = manifest.root.join(&chunk.path);
                let display = match file.strip_prefix(query.cwd) {
                    Ok(relative) if !relative.as_os_str().is_empty() => {
                        relative.to_string_lossy().into_owned()
                    }
                    _ => file.to_string_lossy().into_owned(),
                };
                Hit {
                    index: name.clone(),
                    changed: changed_since(&file, built_at),
                    display,
                    file,
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    start_byte: chunk.start_byte,
                    end_byte: chunk.end_byte,
                    scope: chunk.scope,
                    symbol: chunk.symbol_path,
                }
            })
            .collect())
    }

    // ----- status -----

    /// The state of the index `name` names, or of every index (blocking).
    pub(crate) fn status_blocking(
        &self,
        name: Option<&str>,
        cwd: &Path,
    ) -> Result<Vec<IndexReport>, String> {
        let scanned = match name.map(str::trim).filter(|name| !name.is_empty()) {
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
            has_key: self.backend.has_key(),
        }
    }

    // ----- create -----

    /// Creates an index (or updates the settings of the index of the same
    /// directory and kind), builds it, estimates and starts its embedding, and
    /// watches it when it is monitored (blocking).
    pub(crate) fn create_blocking(
        self: &Arc<Self>,
        args: &CreateArgs,
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
        // Embeddings are on unless asked otherwise (or turned off before): without
        // a key the index is lexical until one is set, then fin embeds it.
        let embed = args
            .embed
            .unwrap_or_else(|| existing.as_ref().is_none_or(|manifest| manifest.embed));
        let entry = self.entry(&layout);
        // A watcher with the old settings stops; it restarts with the new ones.
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
        let mut manifest = existing.clone().unwrap_or_default();
        manifest.version = store::MANIFEST_VERSION;
        manifest.name = name;
        manifest.root = root.clone();
        manifest.kind = kind;
        manifest.model = self.backend.model_for(kind);
        manifest.embed = embed;
        manifest.rules = rules;
        manifest.monitor = args.monitor;
        manifest.max_embed_tokens = args
            .max_embed_tokens
            .or(existing.as_ref().map(|manifest| manifest.max_embed_tokens))
            .unwrap_or(store::DEFAULT_MAX_EMBED_TOKENS);
        manifest.created = existing
            .as_ref()
            .and_then(|manifest| manifest.created)
            .or(Some(Utc::now()));
        manifest.last_error = None;
        manifest.cap_reached = false;
        {
            let _guard = lock(&self.manifests);
            store::save_manifest(&layout.dir, &manifest)?;
        }
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
                                notes.push(format!("watcher state: {error}"));
                            }
                        }
                        (Err(error), _) => notes.push(format!("watcher state: {error}")),
                        _ => {}
                    }
                    built
                }
                Err(OpenError::Busy) => {
                    lock(&entry.live).watch = WatchState::Elsewhere;
                    notes.push("another fin process is watching this index".into());
                    self.build_locked(&entry, &manifest, Caller::Interactive)
                }
                Err(OpenError::Failed(error)) => {
                    notes.push(format!("the watcher failed: {error}"));
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
                return Err(format!("building the index failed: {error}"));
            }
        };
        let manifest = store::load_manifest(&layout.dir).unwrap_or(manifest);
        let mut estimate = None;
        let embedding = if !manifest.embed {
            EmbedPlan::Off
        } else if !self.backend.has_key() {
            EmbedPlan::NoKey
        } else {
            estimate = Some(self.estimate(&entry));
            if args.defer_embed {
                EmbedPlan::Deferred
            } else if args.wait {
                EmbedPlan::Done(self.run_embed(&entry, manifest.max_embed_tokens, true, None, None))
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
                    "it will be monitored while an interactive fin runs in its directory".into(),
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

    /// Runs `action` on the index `name` names (blocking); a report.
    pub(crate) fn manage_blocking(
        self: &Arc<Self>,
        name: &str,
        action: &Action,
        cwd: &Path,
    ) -> Result<String, String> {
        let found = self.resolve(name, cwd)?;
        let entry = self.entry(&found.layout);
        let name = found.manifest.name.clone();
        match action {
            Action::Rebuild => {
                let record = self.build(&entry, &found.manifest, Caller::Interactive)?;
                let mut text = format!(
                    "Rebuilt {name}: {} chunks ({:.1} s); {}.",
                    fmt_count(record.chunks),
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
                Ok(format!("{name}: {}", estimate.line()))
            }
            Action::Embed { max_tokens, wait } => {
                if !self.backend.has_key() {
                    return Err(format!(
                        "embedding needs {} (fin reads it from its .env; see fin --setup)",
                        embed::KEY_NAME
                    ));
                }
                let mut lines = Vec::new();
                let (manifest, record) = self.ensure_dense(&found)?;
                if let Some(record) = record {
                    lines.push(format!(
                        "Rebuilt {name} with embeddings enabled ({} chunks).",
                        fmt_count(record.chunks)
                    ));
                }
                match self.estimate(&entry) {
                    Ok(estimate) => lines.push(format!("Estimate: {}.", estimate.line())),
                    Err(error) => lines.push(format!("Estimate: {error}.")),
                }
                let cap = max_tokens.unwrap_or(manifest.max_embed_tokens);
                if *wait {
                    let summary = self.run_embed(&entry, cap, true, None, None)?;
                    lines.push(format!("{name}: {}.", summary.line()));
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
                        "{name} is no longer monitored; it is brought up to date when searched."
                    ));
                }
                if self.monitor {
                    self.start_watch(&entry);
                    Ok(format!(
                        "{name} is monitored: this fin watches it now, as will interactive fin sessions in its directory."
                    ))
                } else {
                    Ok(format!(
                        "{name} is monitored by interactive fin sessions in its directory."
                    ))
                }
            }
            Action::Remove => {
                self.remove_entry(&entry);
                std::fs::remove_dir_all(&found.layout.dir)
                    .map_err(|error| format!("{}: {error}", found.layout.dir.display()))?;
                Ok(format!(
                    "Removed the index {name} of {}; the indexed files are untouched.",
                    found.manifest.root.display()
                ))
            }
        }
    }

    /// Turns embeddings on for an index built without them (rebuilding it with
    /// its model); the manifest, and the build if one ran.
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
            // A run may have started meanwhile: it stops after its current request.
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

    pub(crate) fn estimate_of(&self, found: &Found) -> Result<Estimate, String> {
        self.estimate(&self.entry(&found.layout))
    }

    /// Reconciles the index with its tree (rebuilding when anything changed),
    /// as a search would.
    pub(crate) fn update_of(&self, found: &Found) -> Result<Option<BuildRecord>, String> {
        let entry = self.entry(&found.layout);
        lock(&entry.live).last_reconcile = None;
        self.reconcile(&entry, &found.manifest)
    }

    pub(crate) fn rebuild_of(&self, found: &Found) -> Result<BuildRecord, String> {
        self.build(
            &self.entry(&found.layout),
            &found.manifest,
            Caller::Interactive,
        )
    }

    /// Starts watching `found` (the CLI's `watch`; needs a monitoring instance).
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

/// Whether `file` was modified after `built` (or is gone).
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
            "embedded {} texts for {} tokens ({}); {}/{} chunks have vectors",
            fmt_count(self.new),
            fmt_count(self.tokens),
            fmt_dollars(self.dollars),
            fmt_count(self.embedded),
            fmt_count(self.docs),
        );
        if self.cap_reached {
            line.push_str(&format!(
                "; stopped at the cap of {} tokens (~{} tokens left: embed again with a higher max_tokens to continue)",
                fmt_count(self.cap),
                fmt_count(self.not_embedded_tokens)
            ));
        } else if self.stopped {
            line.push_str("; stopped before it finished (a later run continues)");
        }
        if self.too_long > 0 {
            line.push_str(&format!(
                "; {} chunks are too long to embed (found by keywords)",
                fmt_count(self.too_long)
            ));
        }
        line
    }
}

impl Estimate {
    pub fn line(&self) -> String {
        if self.tokens == 0 {
            return format!(
                "nothing to embed: {}/{} chunks have vectors",
                fmt_count(self.embedded),
                fmt_count(self.docs)
            );
        }
        format!(
            "embedding {} chunks would take ~{} tokens (~{} with {}); {}/{} chunks have vectors",
            fmt_count(self.texts),
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

/// Dollars: `$0.14`, `$0.0012`, `<$0.0001`, `$0`.
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

/// How long ago `at` was: `just now`, `5 min ago`, `3 h ago`, `2 days ago`.
pub fn fmt_ago(at: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - at).num_seconds().max(0);
    match seconds {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86_400),
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

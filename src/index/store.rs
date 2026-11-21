//! Where indexes live and what is saved with them, as one folder per index under
//! `<data root>/indexes/`, named after the indexed directory, its kind and a short
//! hash of both, with the manifest (`index.json`), arko_index's index state
//! (`state/`) and the watcher's state (`watch2/` on Linux, `watch/` for the polling
//! watcher). The two states are separate directories, and each takes its own lock.

use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use arko_index::FileRules;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The indexes' folder in Arko's data folder.
pub const INDEXES_DIR: &str = "indexes";
/// The manifest in an index's folder.
pub const MANIFEST_FILE: &str = "index.json";
/// Active-day counter file in `<data root>/indexes/`.
pub const USAGE_FILE: &str = "usage.json";
/// Per-index last-search record in an index's folder.
pub const LAST_SEARCH_FILE: &str = "last_search.json";
/// arko_index's index2 state in an index's folder.
pub const STATE_DIR: &str = "state";
/// The inotify watcher's state (Linux).
#[cfg(any(target_os = "linux", test))]
pub const WATCH2_DIR: &str = "watch2";
/// The polling watcher's state (other systems, or where inotify fails).
pub const WATCH_DIR: &str = "watch";
/// The manifest format this version writes.
pub const MANIFEST_VERSION: u32 = 1;
/// Days of Arko use without a search before a worktree index expires.
pub const WORKTREE_EXPIRE_DAYS: u32 = 30;
/// Grace period in days of Arko use for migrated lexical-only indexes.
pub const MIGRATE_LEXICAL_DAYS: u32 = 30;
/// Threshold in days of use for warning about indexes expiring soon.
pub const EXPIRING_SOON_DAYS: u64 = 3;

/// Serializes active-day counter updates within this process.
static USAGE_LOCK: Mutex<()> = Mutex::new(());
/// Monotonic counter for unique atomic-write temporary file names.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
/// The default cap on the tokens one embedding run may spend (voyage-code-4,
/// $0.60).
pub const DEFAULT_MAX_EMBED_TOKENS: u64 = 5_000_000;
/// Excluded from every index besides what `.gitignore` excludes, meaning lock files and
/// minified or generated assets, which cost embedding tokens and find nothing.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "*.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "npm-shrinkwrap.json",
    "*.min.js",
    "*.min.css",
    "*.map",
];

/// What an index holds, which chooses its embedding model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Source code uses voyage-code-4.
    #[default]
    Code,
    /// Prose (Markdown, reStructuredText, text) uses voyage-4.
    Docs,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Code => "code",
            Kind::Docs => "docs",
        }
    }

    /// `code` or `docs`, and `None` for anything else (`auto` included).
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "code" => Some(Kind::Code),
            "docs" | "doc" | "documentation" => Some(Kind::Docs),
            _ => None,
        }
    }
}

/// Which files an index holds (saved with it, as arko_index's `FileRules`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// Honour `.gitignore`, `.ignore` and `.git/info/exclude` (always on for
    /// indexes Arko creates).
    pub respect_ignore: bool,
    /// Gitignore-syntax patterns. When any, only matching files are indexed.
    pub include: Vec<String>,
    /// Gitignore-syntax patterns of files and folders left out.
    pub exclude: Vec<String>,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            respect_ignore: true,
            include: Vec::new(),
            exclude: DEFAULT_EXCLUDES.iter().map(|p| p.to_string()).collect(),
        }
    }
}

impl Rules {
    /// The default rules plus `include` and `exclude` (duplicates dropped).
    pub fn with(include: Vec<String>, exclude: Vec<String>) -> Self {
        let mut rules = Rules {
            include: Vec::new(),
            ..Rules::default()
        };
        for pattern in include {
            let pattern = pattern.trim().to_string();
            if !pattern.is_empty() && !rules.include.contains(&pattern) {
                rules.include.push(pattern);
            }
        }
        for pattern in exclude {
            let pattern = pattern.trim().to_string();
            if !pattern.is_empty() && !rules.exclude.contains(&pattern) {
                rules.exclude.push(pattern);
            }
        }
        rules
    }

    pub fn file_rules(&self) -> FileRules {
        let mut rules = FileRules::default();
        rules.respect_ignore = self.respect_ignore;
        rules.include = self.include.clone();
        rules.exclude = self.exclude.clone();
        rules
    }

    /// Rejects an invalid pattern before any work.
    pub fn validate(&self) -> Result<(), String> {
        self.file_rules()
            .validate()
            .map_err(|error| format!("invalid include/exclude pattern ({error})"))
    }
}

/// How long an index stays without a search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifetime {
    /// Kept until the user removes it (or approves removing it).
    Keep,
    /// Removed after this many days of Arko use without a search.
    Days(u32),
    /// Automatic index for a linked git worktree, derived from `main`.
    /// Removed when its folder is deleted, its branch is merged and clean,
    /// or after [`WORKTREE_EXPIRE_DAYS`] days of use without a search.
    Worktree { main: String },
}

impl Lifetime {
    /// Parses `"keep"` or a positive number of days (such as `"7"`, `"30"`, `"14d"`).
    pub fn parse(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower == "keep" {
            return Ok(Lifetime::Keep);
        }
        let digits = lower
            .strip_suffix("days")
            .or_else(|| lower.strip_suffix("day"))
            .or_else(|| lower.strip_suffix('d'))
            .unwrap_or(&lower)
            .trim();
        let days: u32 = digits.parse().map_err(|_| {
            format!(
                "{trimmed:?} isn't a valid lifetime (use \"keep\" or a positive number of days)"
            )
        })?;
        if days == 0 {
            return Err("lifetime must be \"keep\" or at least 1 day".into());
        }
        Ok(Lifetime::Days(days))
    }

    pub fn is_keep(&self) -> bool {
        matches!(self, Lifetime::Keep)
    }

    pub fn is_worktree(&self) -> bool {
        matches!(self, Lifetime::Worktree { .. })
    }

    /// Days of use without a search before this index expires (`None` for `Keep`).
    pub fn expire_days(&self) -> Option<u64> {
        match self {
            Lifetime::Keep => None,
            Lifetime::Days(days) => Some(u64::from(*days)),
            Lifetime::Worktree { .. } => Some(u64::from(WORKTREE_EXPIRE_DAYS)),
        }
    }

    /// The main index's name when this is a worktree index.
    pub fn main_name(&self) -> Option<&str> {
        match self {
            Lifetime::Worktree { main } => Some(main.as_str()),
            _ => None,
        }
    }

    /// Short description, such as `"keep"`, `"14 days of use"`, `"worktree of arko"`.
    pub fn describe(&self) -> String {
        match self {
            Lifetime::Keep => "keep".into(),
            Lifetime::Days(1) => "1 day of use".into(),
            Lifetime::Days(days) => format!("{days} days of use"),
            Lifetime::Worktree { main } => format!("worktree of {main}"),
        }
    }
}

/// The last successful build.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildRecord {
    pub at: Option<DateTime<Utc>>,
    /// Chunks in the index.
    pub chunks: u64,
    /// Files indexed.
    pub files: u64,
    pub seconds: f64,
    /// arko_index's one-line report, with files indexed, skipped, ignored.
    pub summary: String,
    /// Vectors copied from other indexes' caches during this build.
    pub borrowed: u64,
}

/// `index.json` holds an index's settings and history. Every field has a default, and
/// unknown fields are ignored, so manifests of other versions load.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub version: u32,
    /// The name tools and commands use (unique among the indexes).
    pub name: String,
    /// The indexed directory (canonical).
    pub root: PathBuf,
    pub kind: Kind,
    /// The embedding model (`voyage-code-4`, `voyage-4`).
    pub model: String,
    /// Whether the index has (or should get) embeddings for semantic search.
    pub embed: bool,
    pub rules: Rules,
    /// Keep the index current while Arko runs.
    pub monitor: bool,
    /// The cap on the tokens of one embedding run.
    pub max_embed_tokens: u64,
    pub created: Option<DateTime<Utc>>,
    pub last_build: Option<BuildRecord>,
    /// Why the last build failed, cleared by a successful one. While it's
    /// set, a watcher or a search rebuilds the index. (Earlier versions kept
    /// it in `last_error`, which held an embedding run's error too.)
    #[serde(alias = "last_error")]
    pub build_error: Option<String>,
    /// Why the last embedding run failed, cleared by a successful one.
    pub embed_error: Option<String>,
    /// Embedding tokens spent on this index, all time (as Voyage billed them).
    pub embed_tokens: u64,
    /// Their cost in dollars.
    pub embed_cost: f64,
    /// The last embedding run stopped at its cap. Automatic runs (after
    /// rebuilds) wait for an explicit one, so a large tree doesn't spend the cap
    /// again and again.
    pub cap_reached: bool,
    /// How long this index stays without a search. `None` only in manifests
    /// written before lifetimes existed, which are migrated on load or sweep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime: Option<Lifetime>,
    /// Folder id of the main index this worktree index belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_index: Option<String>,
    /// Session that created this index, if created from an Arko session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_session: Option<String>,
    /// Agent run that created this index, if created by a tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_run: Option<String>,
    /// Active-day count when this index was created (or migrated).
    #[serde(default)]
    pub created_day: u64,
    /// Active-day count when this index's lifetime was last changed.
    #[serde(default)]
    pub lifetime_changed_day: u64,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            name: String::new(),
            root: PathBuf::new(),
            kind: Kind::Code,
            model: String::new(),
            embed: false,
            rules: Rules::default(),
            monitor: false,
            max_embed_tokens: DEFAULT_MAX_EMBED_TOKENS,
            created: None,
            last_build: None,
            build_error: None,
            embed_error: None,
            embed_tokens: 0,
            embed_cost: 0.0,
            cap_reached: false,
            lifetime: None,
            parent_index: None,
            created_by_session: None,
            created_by_run: None,
            created_day: 0,
            lifetime_changed_day: 0,
        }
    }
}

/// Active-day counter stored in `<data root>/indexes/usage.json`.
/// Increments once per UTC calendar day that Arko is used.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub days: u64,
    pub last_date: String,
}

/// Last-search record stored in `<index dir>/last_search.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastSearch {
    /// Active-day count when this index was last searched.
    pub day: u64,
    /// UTC timestamp when this index was last searched.
    pub at: DateTime<Utc>,
}

/// An index's folder and the paths in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub dir: PathBuf,
}

impl Layout {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
    pub fn state(&self) -> PathBuf {
        self.dir.join(STATE_DIR)
    }
    #[cfg(any(target_os = "linux", test))]
    pub fn watch2(&self) -> PathBuf {
        self.dir.join(WATCH2_DIR)
    }
    pub fn watch(&self) -> PathBuf {
        self.dir.join(WATCH_DIR)
    }
    /// The folder's name, the index's id.
    pub fn id(&self) -> String {
        self.dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

pub fn indexes_dir(data_root: &Path) -> PathBuf {
    data_root.join(INDEXES_DIR)
}

/// The folder name of the index of `root` (canonical) and `kind`, as
/// `<slug>-<kind>-<hash>`, where the hash covers the root and the kind, so the
/// same directory and kind always map to the same folder.
pub fn index_id(root: &Path, kind: Kind) -> String {
    let key = format!("{}\n{}", root.to_string_lossy(), kind.as_str());
    let hash = fnv1a(key.as_bytes());
    format!(
        "{}-{}-{:010x}",
        slug(root),
        kind.as_str(),
        hash & 0xff_ffff_ffff
    )
}

/// The directory's name, lowercased, with runs of other characters than ASCII
/// letters, digits, `_` and `.` as one `-`, at most 40 characters.
pub fn slug(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches(['-', '.']).chars().take(40).collect();
    let out = out.trim_end_matches(['-', '.']).to_string();
    if out.is_empty() { "root".into() } else { out }
}

/// 64-bit FNV-1a, stable across versions and platforms.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Reads a manifest. An unreadable or malformed one is an error message, never
/// a panic.
pub fn load_manifest(dir: &Path) -> Result<Manifest, String> {
    let path = dir.join(MANIFEST_FILE);
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("could not read {} ({error})", path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("could not read {} ({error})", path.display()))?;
    if manifest.root.as_os_str().is_empty() {
        return Err(format!("{} names no root", path.display()));
    }
    Ok(manifest)
}

/// Writes `bytes` to `dir.join(file)` atomically using a unique temporary file.
fn atomic_write(dir: &Path, file: &str, bytes: &[u8]) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("could not create {} ({error})", dir.display()))?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{file}.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)
        .map_err(|error| format!("could not write {} ({error})", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(file)).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not write {} ({error})", dir.join(file).display())
    })
}

/// Replaces a manifest atomically (a temporary file renamed over it). Writers
/// in one process take turns under the engine's manifest lock.
pub fn save_manifest(dir: &Path, manifest: &Manifest) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(manifest).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    atomic_write(dir, MANIFEST_FILE, &bytes)
}

/// Reads `<indexes_dir>/usage.json`, returning `Usage::default()` when missing
/// or unreadable.
pub fn load_usage(indexes_dir: &Path) -> Usage {
    let path = indexes_dir.join(USAGE_FILE);
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Today's UTC date formatted as `YYYY-MM-DD`.
pub fn utc_date_today() -> String {
    Utc::now().format("%Y-%m-%d").to_string()
}

/// Records that Arko was used on UTC calendar date `today` (`YYYY-MM-DD`) and
/// returns the active-day count. Only advances once per calendar day (monotonic
/// on `today > last_date`), so clock skew backwards or repeated opens on the
/// same day don't double-count.
pub fn record_active_day(indexes_dir: &Path, today: &str) -> u64 {
    let _guard = USAGE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut usage = load_usage(indexes_dir);
    if usage.days == 0 {
        usage.days = 1;
        usage.last_date = today.to_string();
        if let Ok(mut bytes) = serde_json::to_vec_pretty(&usage) {
            bytes.push(b'\n');
            let _ = atomic_write(indexes_dir, USAGE_FILE, &bytes);
        }
        return usage.days;
    }
    if today > usage.last_date.as_str() {
        usage.days += 1;
        usage.last_date = today.to_string();
        if let Ok(mut bytes) = serde_json::to_vec_pretty(&usage) {
            bytes.push(b'\n');
            let _ = atomic_write(indexes_dir, USAGE_FILE, &bytes);
        }
    }
    usage.days
}

/// Reads `<dir>/last_search.json`, returning `None` if never searched or unreadable.
pub fn load_last_search(dir: &Path) -> Option<LastSearch> {
    let bytes = std::fs::read(dir.join(LAST_SEARCH_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Writes `<dir>/last_search.json` atomically.
pub fn save_last_search(dir: &Path, record: &LastSearch) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    atomic_write(dir, LAST_SEARCH_FILE, &bytes)
}

/// Total bytes of regular files under `path` (non-following symlinks, errors skipped).
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_file() {
                if let Ok(meta) = entry.metadata() {
                    total = total.saturating_add(meta.len());
                }
            } else if ft.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    total
}

/// Formats byte sizes as `840 B`, `14 KB`, `1.4 MB`, `1.2 GB`.
pub fn fmt_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        if b >= 10.0 * MB {
            format!("{:.0} MB", b / MB)
        } else {
            format!("{:.1} MB", b / MB)
        }
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Whether `root` was deleted: the folder itself does not exist, while its
/// parent directory does. An unmounted drive has a missing parent and returns
/// `false`.
pub fn folder_deleted(root: &Path) -> bool {
    !root.exists() && root.parent().is_some_and(Path::exists)
}

/// One folder under `indexes/`.
#[derive(Debug, Clone)]
pub struct Scanned {
    pub layout: Layout,
    pub manifest: Result<Manifest, String>,
}

/// Every index folder (one with an `index.json`), sorted by name. Folders whose
/// manifest can't be read are listed with the error.
pub fn scan(indexes_dir: &Path) -> Vec<Scanned> {
    let Ok(entries) = std::fs::read_dir(indexes_dir) else {
        return Vec::new();
    };
    let mut out: Vec<Scanned> = entries
        .flatten()
        .filter(|entry| entry.path().join(MANIFEST_FILE).is_file())
        .map(|entry| {
            let layout = Layout::new(entry.path());
            let manifest = load_manifest(&layout.dir);
            Scanned { layout, manifest }
        })
        .collect();
    out.sort_by(|a, b| {
        let name = |s: &Scanned| s.manifest.as_ref().map(|m| m.name.clone()).ok();
        name(a)
            .cmp(&name(b))
            .then_with(|| a.layout.dir.cmp(&b.layout.dir))
    });
    out
}

/// `path` resolved against `cwd` (when relative) with `.` and `..` folded,
/// without touching the file system.
pub fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The directory to index, meaning `path` (relative to `cwd`) canonicalized. Refused for
/// a missing path or a file, the file system root (or a drive root), the home
/// directory and the directories that contain it, and Arko's own index folders.
pub fn validate_root(
    path: &Path,
    cwd: &Path,
    home: Option<&Path>,
    data_root: &Path,
) -> Result<PathBuf, String> {
    let wanted = absolute(path, cwd);
    let root = wanted
        .canonicalize()
        .map_err(|error| format!("cannot index {} ({error})", wanted.display()))?;
    if !root.is_dir() {
        return Err(format!(
            "cannot index {}, which isn't a directory (index the folder that holds it)",
            root.display()
        ));
    }
    if root.parent().is_none() {
        return Err(format!(
            "refusing to index the file system root {}. Index a project folder",
            root.display()
        ));
    }
    if let Some(home) = home {
        let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
        if root == home {
            return Err(format!(
                "refusing to index the home directory {}. Index a project folder in it",
                root.display()
            ));
        }
        if home.starts_with(&root) {
            return Err(format!(
                "refusing to index {}, which contains the home directory. Index a project folder",
                root.display()
            ));
        }
    }
    let indexes = indexes_dir(data_root);
    let indexes = indexes.canonicalize().unwrap_or(indexes);
    if root.starts_with(&indexes) {
        return Err(format!(
            "refusing to index {}, which is inside Arko's index folder",
            root.display()
        ));
    }
    Ok(root)
}

/// A name for an index, trimmed, 1 to 64 characters, no `/`, `\` or control
/// characters.
pub fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("an index name cannot be empty".into());
    }
    if name.chars().count() > 64 {
        return Err("an index name has at most 64 characters".into());
    }
    if name
        .chars()
        .any(|c| c == '/' || c == '\\' || c.is_control())
    {
        return Err(format!(
            "invalid index name {name:?} (no slashes or control characters)"
        ));
    }
    Ok(name.to_string())
}

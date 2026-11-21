//! Tests of the index module, offline and without a Voyage key. Dense and hybrid
//! search run through the same code as Voyage with arko_index's `HashEmbedder`
//! (`Backend::Hash`), and a live Voyage test is `#[ignore]`d.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arko_agents::{TaskContext, ToolRecovery};
use arko_index::hybrid::Mode;
use clap::Parser;
use serde_json::{Value, json};

use super::census::{self, Census, Class};
use super::cli::{IndexCommand, cli_search_text, cli_text, run_command_with, search_text};
use super::embed::{self, Backend};
use super::engine::{
    Action, CreateArgs, CreateOutcome, EmbedPlan, Events, Hit, Indexes, ModeArg, SearchArgs,
    SearchOutput, Timing, WatchState, monitor_words,
};
use super::lexical::SINGLE_QUOTES;
use super::store::{self, Kind, Layout, Lifetime, Manifest, Rules};
use super::tools::{self, TOOL_NAMES};

// ----- fixtures -----

/// Short delays, so monitoring acts within a test's patience.
fn fast(force_polling: bool) -> Timing {
    Timing {
        quiet: Duration::from_millis(200),
        min_interval: Duration::ZERO,
        poll: Duration::from_millis(100),
        poll_interval: Duration::from_millis(200),
        sync_rebuild_max: 5.0,
        reconcile_interval: Duration::ZERO,
        busy_wait: Duration::from_secs(10),
        busy_retry: Duration::from_millis(200),
        git_timeout: Duration::from_secs(5),
        sweep_interval: Duration::from_secs(24 * 60 * 60),
        force_polling,
    }
}

const PATIENCE: Duration = Duration::from_secs(30);

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

const NET_RS: &str = r#"//! Networking helpers.

/// Retries a request with exponential backoff until it succeeds or the
/// attempts run out, and the delay doubles after each failure.
pub fn retry_with_backoff<F: FnMut() -> bool>(mut attempt: F, tries: u32) -> bool {
    let mut delay = 100;
    for _ in 0..tries {
        if attempt() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(delay));
        delay *= 2;
    }
    false
}
"#;

const CONFIG_RS: &str = r#"/// Parses a configuration file of `key = value` lines into pairs.
pub fn parse_config(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}
"#;

struct Fixture {
    runtime: tokio::runtime::Runtime,
    _temp: tempfile::TempDir,
    base: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        Self {
            runtime,
            _temp: temp,
            base,
        }
    }

    fn data(&self) -> PathBuf {
        self.base.join("data")
    }

    /// A small Rust project with an ignored folder.
    fn code_project(&self) -> PathBuf {
        let root = self.base.join("proj");
        write(&root.join(".gitignore"), "ignored/\n");
        write(&root.join("src/lib.rs"), "pub mod config;\npub mod net;\n");
        write(&root.join("src/net.rs"), NET_RS);
        write(&root.join("src/config.rs"), CONFIG_RS);
        write(
            &root.join("ignored/secret.rs"),
            "fn ignoredmarkerfunction() {}\n",
        );
        write(
            &root.join("README.md"),
            "# proj\n\nHelpers for configs and retries.\n",
        );
        root
    }

    /// Mostly prose.
    fn docs_project(&self) -> PathBuf {
        let root = self.base.join("handbook");
        write(
            &root.join("intro.md"),
            "# Introduction\n\nHow to onboard new teammates.\n",
        );
        write(
            &root.join("deploy.md"),
            "# Deploying\n\nShip to production on Tuesdays.\n",
        );
        write(&root.join("faq.mdx"), "# FAQ\n\nQuestions people ask.\n");
        write(
            &root.join("guide.rst"),
            "Guide\n=====\n\nA guide in reStructuredText.\n",
        );
        write(&root.join("conf.py"), "project = 'handbook'\n");
        root
    }

    fn open(
        &self,
        workspace: &Path,
        backend: Backend,
        monitor: bool,
        timing: Timing,
    ) -> Arc<Indexes> {
        let _guard = self.runtime.enter();
        let indexes =
            Indexes::open_with(&self.data(), workspace, backend, monitor, timing, None).unwrap();
        indexes.watch_related();
        indexes
    }

    /// [`open`](Self::open) with the test embedder, whose progress lines go
    /// to `events`.
    fn open_with_events(&self, workspace: &Path, monitor: bool, events: Events) -> Arc<Indexes> {
        let _guard = self.runtime.enter();
        let indexes = Indexes::open_with(
            &self.data(),
            workspace,
            Backend::Hash,
            monitor,
            fast(false),
            Some(events),
        )
        .unwrap();
        indexes.watch_related();
        indexes
    }

    fn shutdown(&self, indexes: &Indexes) {
        self.runtime.block_on(indexes.shutdown());
    }
}

/// Progress lines that panic at the first one starting with `prefix`, and
/// whether that happened. A line is how a test reaches inside a background
/// thread.
fn panicking_once_on(prefix: &'static str) -> (Events, Arc<AtomicBool>) {
    let panicked = Arc::new(AtomicBool::new(false));
    let flag = panicked.clone();
    let events: Events = Arc::new(move |line: &str| {
        if line.starts_with(prefix) && !flag.swap(true, Ordering::SeqCst) {
            panic!("a test panic at {line:?}");
        }
    });
    (events, panicked)
}

fn create_args(path: &Path, kind: Option<Kind>) -> CreateArgs {
    CreateArgs {
        path: path.to_path_buf(),
        cwd: path.to_path_buf(),
        kind,
        name: None,
        include: Vec::new(),
        exclude: Vec::new(),
        monitor: true,
        embed: None,
        max_embed_tokens: None,
        lifetime: Some(Lifetime::Keep),
        created_by_session: None,
        created_by_run: None,
        wait: true,
        defer_embed: false,
    }
}

fn search_args(cwd: &Path, query: &str, mode: Option<ModeArg>) -> SearchArgs {
    SearchArgs {
        query: query.into(),
        index: None,
        mode,
        limit: Some(10),
        exhaustive: false,
        after: None,
        path_prefix: None,
        language: None,
        ignore_case: false,
        cwd: cwd.to_path_buf(),
        created_by_session: None,
        created_by_run: None,
    }
}

fn hit_files(output: &SearchOutput) -> Vec<String> {
    output.hits.iter().map(|hit| hit.display.clone()).collect()
}

fn embedded(indexes: &Indexes, root: &Path) -> (u64, u64) {
    let reports = indexes.status_blocking(Some("proj"), root).unwrap();
    reports[0]
        .dense
        .as_ref()
        .map_or((0, 0), |info| (info.embedded, info.docs))
}

fn params(value: Value) -> HashMap<String, Value> {
    serde_json::from_value(value).unwrap()
}

// ----- storage -----

#[test]
fn names_and_ids() {
    assert_eq!(store::slug(Path::new("/w/My Project!")), "my-project");
    assert_eq!(store::slug(Path::new("/w/arko")), "arko");
    assert_eq!(store::slug(Path::new("/")), "root");
    assert_eq!(store::slug(Path::new("/w/--")), "root");
    assert_eq!(store::slug(Path::new("/w/.hidden")), "hidden");
    let long = "x".repeat(100);
    assert_eq!(store::slug(&Path::new("/w").join(&long)).len(), 40);

    let id = store::index_id(Path::new("/w/arko"), Kind::Code);
    assert_eq!(id, store::index_id(Path::new("/w/arko"), Kind::Code));
    let (prefix, hash) = id.rsplit_once('-').unwrap();
    assert_eq!(prefix, "arko-code");
    assert_eq!(hash.len(), 10);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(id, store::index_id(Path::new("/w/arko"), Kind::Docs));
    assert_ne!(id, store::index_id(Path::new("/x/arko"), Kind::Code));

    assert_eq!(store::validate_name(" arko ").unwrap(), "arko");
    assert!(store::validate_name("").is_err());
    assert!(store::validate_name("a/b").is_err());
    assert!(store::validate_name(&"n".repeat(65)).is_err());

    assert_eq!(
        store::absolute(Path::new("../b/./c"), Path::new("/w/a")),
        PathBuf::from("/w/b/c")
    );
    assert_eq!(Kind::parse("Docs"), Some(Kind::Docs));
    assert_eq!(Kind::parse("code"), Some(Kind::Code));
    assert_eq!(Kind::parse("auto"), None);
    assert_eq!(embed::voyage_model(Kind::Code), "voyage-code-4");
    assert_eq!(embed::voyage_model(Kind::Docs), "voyage-4");
}

#[test]
fn manifests_round_trip_and_bad_ones_are_skipped() {
    let temp = tempfile::tempdir().unwrap();
    let indexes_dir = store::indexes_dir(temp.path());
    let dir = indexes_dir.join("proj-code-0123456789");
    let manifest = Manifest {
        name: "proj".into(),
        root: PathBuf::from("/w/proj"),
        kind: Kind::Docs,
        model: "voyage-4".into(),
        embed: true,
        rules: Rules::with(vec!["*.md".into()], vec!["drafts/".into()]),
        max_embed_tokens: 1234,
        embed_tokens: 99,
        embed_cost: 0.5,
        ..Manifest::default()
    };
    store::save_manifest(&dir, &manifest).unwrap();
    let loaded = store::load_manifest(&dir).unwrap();
    assert_eq!(loaded.name, "proj");
    assert_eq!(loaded.kind, Kind::Docs);
    assert_eq!(loaded.model, "voyage-4");
    assert_eq!(loaded.rules.include, vec!["*.md".to_string()]);
    assert!(loaded.rules.exclude.contains(&"drafts/".to_string()));
    assert!(loaded.rules.respect_ignore);
    assert_eq!(loaded.max_embed_tokens, 1234);
    assert_eq!(loaded.embed_tokens, 99);
    assert_eq!(loaded.version, store::MANIFEST_VERSION);

    // A newer Arko's manifest, with unknown fields and missing ones.
    write(
        &indexes_dir.join("newer/index.json"),
        r#"{"version": 9, "name": "newer", "root": "/w/newer", "future": {"x": 1}}"#,
    );
    let newer = store::load_manifest(&indexes_dir.join("newer")).unwrap();
    assert_eq!(newer.name, "newer");
    assert_eq!(newer.kind, Kind::Code);
    assert_eq!(newer.max_embed_tokens, store::DEFAULT_MAX_EMBED_TOKENS);
    // Garbage, a manifest without a root, a folder without a manifest.
    write(&indexes_dir.join("garbage/index.json"), "{not json");
    write(&indexes_dir.join("rootless/index.json"), r#"{"name": "x"}"#);
    std::fs::create_dir_all(indexes_dir.join("empty")).unwrap();

    let scanned = store::scan(&indexes_dir);
    assert_eq!(scanned.len(), 4, "{scanned:?}");
    let ok: Vec<String> = scanned
        .iter()
        .filter_map(|s| s.manifest.as_ref().ok().map(|m| m.name.clone()))
        .collect();
    assert_eq!(ok, vec!["newer".to_string(), "proj".to_string()]);
    assert_eq!(scanned.iter().filter(|s| s.manifest.is_err()).count(), 2);
    assert!(store::scan(&temp.path().join("missing")).is_empty());
}

/// A manifest from before build and embedding errors were kept apart reads
/// its error as the last build's.
#[test]
fn an_older_manifests_error_reads_as_the_last_builds() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join(store::MANIFEST_FILE),
        r#"{"root": "/home/ada/proj", "last_error": "build failed (the disk is full)"}"#,
    );
    let manifest = store::load_manifest(temp.path()).unwrap();
    assert_eq!(
        manifest.build_error.as_deref(),
        Some("build failed (the disk is full)")
    );
    assert_eq!(manifest.embed_error, None);
}

#[test]
fn layout_paths_are_separate() {
    let layout = Layout::new(PathBuf::from("/d/indexes/proj-code-0123456789"));
    assert_eq!(layout.id(), "proj-code-0123456789");
    let dirs = [layout.state(), layout.watch2(), layout.watch()];
    for (i, a) in dirs.iter().enumerate() {
        assert!(a.starts_with(&layout.dir));
        for b in &dirs[i + 1..] {
            assert!(!a.starts_with(b) && !b.starts_with(a), "{a:?} {b:?}");
        }
    }
}

/// The indexes the agents' instructions name, meaning where each lies from the
/// working directory, the working directory's first, then by name.
#[test]
fn known_indexes_say_where_they_lie_from_the_working_directory() {
    let fixture = Fixture::new();
    let proj = fixture.code_project();
    let handbook = fixture.docs_project();
    let indexes = fixture.open(&proj, Backend::Hash, false, fast(false));
    for root in [&proj, &handbook] {
        indexes.create_blocking(&create_args(root, None)).unwrap();
    }
    let places = |workspace: &Path| {
        indexes
            .known(workspace)
            .into_iter()
            .map(|known| (known.name, known.kind, known.place))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        places(&handbook),
        [
            (
                "handbook".to_string(),
                "docs",
                Some("the working directory")
            ),
            ("proj".to_string(), "code", None),
        ]
    );
    assert_eq!(
        places(&proj.join("src")),
        [
            (
                "proj".to_string(),
                "code",
                Some("holds the working directory")
            ),
            ("handbook".to_string(), "docs", None),
        ]
    );
    assert_eq!(
        places(&fixture.base),
        [
            (
                "handbook".to_string(),
                "docs",
                Some("in the working directory")
            ),
            ("proj".to_string(), "code", Some("in the working directory")),
        ]
    );
    assert_eq!(
        places(&fixture.data()),
        [
            ("handbook".to_string(), "docs", None),
            ("proj".to_string(), "code", None),
        ]
    );
    let roots: Vec<PathBuf> = indexes
        .known(&proj)
        .into_iter()
        .map(|known| known.root)
        .collect();
    assert_eq!(roots, [proj.clone(), handbook.clone()]);
    fixture.shutdown(&indexes);
}

#[test]
fn roots_that_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let home = base.join("home/me");
    let project = home.join("code/proj");
    std::fs::create_dir_all(&project).unwrap();
    let data = home.join(".local/share/arko");
    std::fs::create_dir_all(store::indexes_dir(&data).join("x")).unwrap();
    write(&project.join("main.rs"), "fn main() {}\n");
    let check = |path: &Path| store::validate_root(path, &project, Some(&home), &data);

    let error = check(Path::new("/")).unwrap_err();
    assert!(error.contains("root"), "{error}");
    let error = check(&home).unwrap_err();
    assert!(error.contains("home directory"), "{error}");
    let error = check(&base.join("home")).unwrap_err();
    assert!(error.contains("contains the home directory"), "{error}");
    let error = check(&store::indexes_dir(&data).join("x")).unwrap_err();
    assert!(error.contains("index folder"), "{error}");
    let error = check(&project.join("main.rs")).unwrap_err();
    assert!(error.contains("isn't a directory"), "{error}");
    assert!(check(&project.join("missing")).is_err());

    assert_eq!(check(Path::new(".")).unwrap(), project);
    assert_eq!(check(&home.join("code")).unwrap(), home.join("code"));
    // A `..` path is resolved before it is judged.
    assert!(check(Path::new("../../..")).is_err());
}

#[test]
fn rules_validate_patterns() {
    assert!(
        Rules::with(vec!["src/**".into()], vec!["*.gen.rs".into()])
            .validate()
            .is_ok()
    );
    let rules = Rules::with(Vec::new(), Vec::new());
    assert!(rules.respect_ignore);
    for default in store::DEFAULT_EXCLUDES {
        assert!(rules.exclude.contains(&default.to_string()));
    }
}

// ----- census -----

#[test]
fn census_picks_the_kind() {
    let fixture = Fixture::new();
    let docs = fixture.docs_project();
    let code = fixture.code_project();
    let rules = Rules::with(Vec::new(), Vec::new());

    let census_docs = census::census(&docs, &rules, census::CENSUS_LIMIT).unwrap();
    assert_eq!((census_docs.prose, census_docs.code), (4, 1));
    assert_eq!(census_docs.kind(), Kind::Docs);

    let census_code = census::census(&code, &rules, census::CENSUS_LIMIT).unwrap();
    // The ignored folder isn't counted, and neither is .gitignore as code or prose.
    assert_eq!(
        (census_code.prose, census_code.code),
        (1, 3),
        "{census_code:?}"
    );
    assert_eq!(census_code.kind(), Kind::Code);

    // Ignoring the ignore files counts the ignored folder too.
    let raw = Rules {
        respect_ignore: false,
        ..rules.clone()
    };
    assert_eq!(
        census::census(&code, &raw, census::CENSUS_LIMIT)
            .unwrap()
            .code,
        4
    );
    let limited = census::census(&code, &rules, 2).unwrap();
    assert!(limited.truncated);

    assert_eq!(census::classify(Path::new("a/README.md")), Class::Prose);
    assert_eq!(census::classify(Path::new("a/b.rs")), Class::Code);
    assert_eq!(census::classify(Path::new("a/logo.png")), Class::Other);
    assert_eq!(Census::default().kind(), Kind::Code);
    let even = Census {
        prose: 2,
        code: 1,
        ..Census::default()
    };
    assert_eq!(even.kind(), Kind::Docs);
    let mixed = Census {
        prose: 1,
        code: 1,
        ..Census::default()
    };
    assert_eq!(mixed.kind(), Kind::Code);
}

// ----- building and searching -----

#[test]
fn without_a_key_indexes_are_lexical() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(
        &root,
        Backend::voyage(Some("  ".into())),
        false,
        fast(false),
    );
    assert!(!indexes.has_key());
    assert!(indexes.summary().is_none());

    let outcome = indexes.create_blocking(&create_args(&root, None)).unwrap();
    assert_eq!(outcome.manifest.kind, Kind::Code);
    assert_eq!(outcome.manifest.model, "voyage-code-4");
    assert!(outcome.manifest.embed, "embeddings wait for a key");
    assert!(matches!(outcome.embedding, EmbedPlan::NoKey));
    assert!(outcome.estimate.is_none());
    assert_eq!(outcome.layout.id(), store::index_id(&root, Kind::Code));
    assert!(outcome.record.chunks >= 3);
    let text = tools::format_create(&outcome);
    assert!(text.contains("VOYAGE_API_KEY"), "{text}");
    assert!(text.contains("Kind code"), "{text}");

    let output = indexes
        .search_blocking(&search_args(&root, "exponential backoff", None))
        .unwrap();
    assert_eq!(hit_files(&output)[0], "src/net.rs", "{output:?}");
    assert_eq!(
        output.searched,
        vec![("proj".to_string(), Kind::Code, Mode::Lexical)]
    );
    let text = tools::format_search("exponential backoff", &output, 3);
    assert!(text.contains("src/net.rs:"), "{text}");
    assert!(text.contains("VOYAGE_API_KEY"), "{text}");

    // Ignored files aren't indexed.
    let output = indexes
        .search_blocking(&search_args(&root, "ignoredmarkerfunction", None))
        .unwrap();
    assert!(output.hits.is_empty(), "{output:?}");

    let summary = indexes.summary().unwrap();
    assert!(summary.starts_with("index proj (code"), "{summary}");
    assert!(summary.contains("VOYAGE_API_KEY"), "{summary}");
    assert_eq!(indexes.spent(), (0, 0.0));

    let error = indexes
        .manage_blocking(
            "proj",
            &Action::Embed {
                max_tokens: None,
                wait: true,
            },
            &root,
        )
        .unwrap_err();
    assert!(error.contains("VOYAGE_API_KEY"), "{error}");
    fixture.shutdown(&indexes);
}

/// A Voyage key set while Arko runs (the keys menu) is used from then on,
/// and removing it makes the indexes lexical again.
#[test]
fn the_voyage_key_changes_while_arko_runs() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::voyage(None), false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    assert!(!indexes.has_key());
    assert!(indexes.summary().unwrap().contains("VOYAGE_API_KEY"));
    indexes.set_voyage_key(Some("pa-test".into()));
    assert!(indexes.has_key());
    let summary = indexes.summary().unwrap();
    assert!(!summary.contains("VOYAGE_API_KEY"), "{summary}");
    indexes.set_voyage_key(None);
    assert!(!indexes.has_key());
    fixture.shutdown(&indexes);
}

/// An app reads the workspace's indexes as data, meaning the ones the banner
/// lists, by name, with how they're kept current and their embeddings.
#[test]
fn an_app_reads_the_workspace_indexes_as_data() {
    use crate::details::{IndexKind, IndexWatch, index_facts};

    let fixture = Fixture::new();
    let root = fixture.code_project();
    let handbook = fixture.docs_project();
    write(&root.join("guide/setup.md"), "# Setup\n\nInstall it.\n");
    let indexes = fixture.open(&root, Backend::Hash, true, fast(false));
    assert!(index_facts(&indexes).is_empty());

    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let guide = CreateArgs {
        embed: Some(false),
        monitor: false,
        ..create_args(&root.join("guide"), Some(Kind::Docs))
    };
    indexes.create_blocking(&guide).unwrap();
    // Another tree's index isn't the workspace's.
    indexes
        .create_blocking(&create_args(&handbook, None))
        .unwrap();
    let proj = indexes.resolve("proj", &root).unwrap();
    wait_for("the watcher", || {
        matches!(
            indexes.live_state(&proj.layout).0,
            WatchState::Watching { .. }
        )
    });

    let facts = index_facts(&indexes);
    let names: Vec<&str> = facts.iter().map(|facts| facts.name.as_str()).collect();
    assert_eq!(names, ["guide", "proj"], "{facts:?}");

    let guide = &facts[0];
    assert_eq!(guide.kind, IndexKind::Docs);
    assert_eq!(guide.root, root.join("guide"));
    assert!(!guide.monitor);
    assert_eq!(guide.watch, IndexWatch::Off);
    assert!(!guide.monitored());
    assert_eq!(guide.chunks, Some(1));
    assert_eq!(guide.files, Some(1));
    assert!(guide.embeddings.is_none(), "{guide:?}");

    let proj = &facts[1];
    assert_eq!(proj.kind, IndexKind::Code);
    assert_eq!(proj.kind.as_str(), "code");
    assert_eq!(proj.root, root);
    assert!(proj.monitor);
    assert_eq!(proj.watch, IndexWatch::Here);
    assert!(proj.monitored());
    assert!(proj.built.is_some());
    assert!(proj.files.is_some_and(|files| files >= 3), "{proj:?}");
    assert_eq!(proj.last_error, None);
    let chunks = proj.chunks.unwrap();
    assert!(chunks >= 3, "{proj:?}");
    let embeddings = proj.embeddings.as_ref().unwrap();
    assert_eq!(embeddings.model, embed::HASH_MODEL);
    assert_eq!(embeddings.embedded, chunks);
    assert!(embeddings.complete);
    assert!(!embeddings.cap_reached);
    assert!(!embeddings.needs_key);
    assert!(embeddings.tokens > 0, "{embeddings:?}");

    // The banner names the same indexes.
    let summary = indexes.summary().unwrap();
    assert!(
        summary.starts_with("indexes guide (docs, lexical), proj (code, monitored)"),
        "{summary}"
    );
    fixture.shutdown(&indexes);
}

/// Without a Voyage key, an index that embeds says semantic search waits
/// for one.
#[test]
fn an_app_reads_that_semantic_search_needs_a_key() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::voyage(None), false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let facts = crate::details::index_facts(&indexes);
    assert_eq!(facts.len(), 1, "{facts:?}");
    assert_eq!(facts[0].watch, crate::details::IndexWatch::Off);
    let embeddings = facts[0].embeddings.as_ref().unwrap();
    assert_eq!(embeddings.model, "voyage-code-4");
    assert_eq!(embeddings.embedded, 0);
    assert!(!embeddings.complete);
    assert!(embeddings.needs_key);
    assert_eq!(embeddings.tokens, 0);
    fixture.shutdown(&indexes);
}

#[test]
fn creating_refuses_bad_roots_and_names_uniquely() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::voyage(None), false, fast(false));
    let error = indexes
        .create_blocking(&create_args(Path::new("/"), None))
        .unwrap_err();
    assert!(error.contains("root"), "{error}");
    if let Some(home) = std::env::home_dir() {
        let error = indexes
            .create_blocking(&create_args(&home, None))
            .unwrap_err();
        assert!(error.contains("home directory"), "{error}");
    }
    let bad_rules = CreateArgs {
        exclude: vec!["[".into()],
        ..create_args(&root, Some(Kind::Code))
    };
    assert!(indexes.create_blocking(&bad_rules).is_err());
    assert!(
        indexes.all().is_empty(),
        "nothing is left of failed creations"
    );

    let code = indexes
        .create_blocking(&create_args(&root, Some(Kind::Code)))
        .unwrap();
    assert_eq!(code.manifest.name, "proj");
    assert!(!code.updated);
    // The same tree as docs, so another index, another name.
    let docs = indexes
        .create_blocking(&create_args(&root, Some(Kind::Docs)))
        .unwrap();
    assert_eq!(docs.manifest.name, "proj-docs");
    assert_eq!(docs.manifest.model, "voyage-4");
    // Again, updated in place.
    let again = CreateArgs {
        monitor: false,
        ..create_args(&root, Some(Kind::Code))
    };
    let again = indexes.create_blocking(&again).unwrap();
    assert!(again.updated);
    assert_eq!(again.manifest.name, "proj");
    assert!(!again.manifest.monitor);
    assert_eq!(indexes.all().len(), 2);

    // Two indexes cover the directory, so a search uses both.
    let output = indexes
        .search_blocking(&search_args(&root, "parse configuration", None))
        .unwrap();
    assert_eq!(output.searched.len(), 2);
    let text = tools::format_search("parse configuration", &output, 1);
    assert!(
        text.contains("[proj] ") || text.contains("[proj-docs] "),
        "{text}"
    );
    // A search elsewhere names the indexes there are.
    let elsewhere = fixture.base.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let error = indexes
        .search_blocking(&search_args(&elsewhere, "x", None))
        .unwrap_err();
    assert!(error.contains("proj"), "{error}");
    fixture.shutdown(&indexes);
}

#[test]
fn dense_and_hybrid_search_with_the_test_embedder() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    let outcome = indexes.create_blocking(&create_args(&root, None)).unwrap();
    assert_eq!(outcome.manifest.model, embed::HASH_MODEL);
    let summary = match &outcome.embedding {
        EmbedPlan::Done(Ok(summary)) => summary.clone(),
        other => panic!("{other:?}"),
    };
    assert!(summary.new > 0 && summary.tokens > 0, "{summary:?}");
    assert_eq!(summary.embedded, summary.docs);
    let estimate = outcome.estimate.clone().unwrap().unwrap();
    assert!(
        estimate.tokens > 0 && estimate.embedded == 0,
        "{estimate:?}"
    );
    assert!(indexes.spent().0 >= summary.tokens);
    let manifest = store::load_manifest(&outcome.layout.dir).unwrap();
    assert_eq!(manifest.embed_tokens, summary.tokens);
    assert!(manifest.last_build.is_some() && manifest.build_error.is_none());

    // (Lexical search is exact, so it gets a name.)
    for (mode, expected, query) in [
        (Some(ModeArg::Semantic), Mode::Dense, "retry backoff delay"),
        (Some(ModeArg::Hybrid), Mode::Hybrid, "retry backoff delay"),
        (None, Mode::Hybrid, "retry backoff delay"),
        (Some(ModeArg::Lexical), Mode::Lexical, "retry_with_backoff"),
    ] {
        let output = indexes
            .search_blocking(&search_args(&root, query, mode))
            .unwrap();
        assert_eq!(output.searched[0].2, expected, "{mode:?}");
        assert_eq!(hit_files(&output)[0], "src/net.rs", "{mode:?}: {output:?}");
        let hit = &output.hits[0];
        assert!(hit.start_line >= 1 && hit.end_line >= hit.start_line);
        assert!(output.notes.is_empty(), "{:?}", output.notes);
    }
    let spent = indexes.spent().0;
    indexes
        .search_blocking(&search_args(&root, "parse config", Some(ModeArg::Semantic)))
        .unwrap();
    assert!(indexes.spent().0 > spent, "query tokens count");

    // Filters are a path prefix (relative to the search's directory) and a language.
    let mut args = search_args(&root.join("src"), "retry", Some(ModeArg::Hybrid));
    args.path_prefix = Some("config.rs".into());
    let output = indexes.search_blocking(&args).unwrap();
    assert!(
        output.hits.iter().all(|hit| hit.display == "config.rs"),
        "{output:?}"
    );
    let mut args = search_args(&root, "Helpers for configs", Some(ModeArg::Lexical));
    args.language = Some("Markdown".into());
    let output = indexes.search_blocking(&args).unwrap();
    assert!(!output.hits.is_empty());
    assert!(
        output.hits.iter().all(|hit| hit.display == "README.md"),
        "{output:?}"
    );
    // A ranked search with a language that matches nothing gives the results
    // from every language, and says so.
    for mode in [ModeArg::Hybrid, ModeArg::Semantic] {
        let mut args = search_args(&root, "retry backoff delay", Some(mode));
        args.language = Some("python".into());
        let output = indexes.search_blocking(&args).unwrap();
        assert_eq!(hit_files(&output)[0], "src/net.rs", "{output:?}");
        assert_eq!(
            output.notes,
            ["nothing matched in python, so these results are from every language"]
        );
    }
    // A lexical search stays with the language, and says what to do.
    let mut args = search_args(&root, "retry_with_backoff", Some(ModeArg::Lexical));
    args.language = Some("python".into());
    let output = indexes.search_blocking(&args).unwrap();
    assert!(output.hits.is_empty(), "{output:?}");
    assert_eq!(output.total, Some(0));
    assert_eq!(
        output.notes,
        ["No matches in python. Search without the language filter to see every language."]
    );
    args.language = None;
    assert_eq!(
        hit_files(&indexes.search_blocking(&args).unwrap())[0],
        "src/net.rs"
    );
    // What lexical search finds, and what finds the rest.
    let args = search_args(&root, "zzqqxx", Some(ModeArg::Lexical));
    let output = indexes.search_blocking(&args).unwrap();
    assert!(output.hits.is_empty(), "{output:?}");
    assert_eq!(output.notes.len(), 1, "{output:?}");
    assert!(
        output.notes[0].starts_with(
            "lexical finds the chunks that hold the query as written (ignore_case=true"
        )
    );
    let mut args = search_args(&root, "retry", None);
    args.path_prefix = Some(
        fixture
            .base
            .join("elsewhere")
            .to_string_lossy()
            .into_owned(),
    );
    // Without an index named, the path picks the indexes, and there are none.
    let error = indexes.search_blocking(&args).unwrap_err();
    assert!(error.contains("no index covers"), "{error}");
    args.index = Some("proj".into());
    let error = indexes.search_blocking(&args).unwrap_err();
    assert!(error.contains("outside"), "{error}");

    // A free estimate of a complete index, then the status.
    let text = indexes
        .manage_blocking("proj", &Action::Estimate, &root)
        .unwrap();
    assert!(text.contains("proj"), "{text}");
    let reports = indexes.status_blocking(None, &root).unwrap();
    let text = tools::format_status(&reports, Some(indexes.spent()));
    assert!(text.contains("proj — code, hash-v1"), "{text}");
    assert!(text.contains("embeddings:"), "{text}");
    assert!(text.contains("This session's embeddings"), "{text}");
    fixture.shutdown(&indexes);
}

#[test]
fn a_token_cap_stops_embedding_and_pauses_automatic_runs() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    let args = CreateArgs {
        max_embed_tokens: Some(3),
        ..create_args(&root, Some(Kind::Code))
    };
    let outcome = indexes.create_blocking(&args).unwrap();
    let summary = match &outcome.embedding {
        EmbedPlan::Done(Ok(summary)) => summary.clone(),
        other => panic!("{other:?}"),
    };
    assert!(summary.cap_reached, "{summary:?}");
    assert!(summary.embedded < summary.docs);
    assert!(summary.not_embedded_tokens > 0);
    assert!(
        store::load_manifest(&outcome.layout.dir)
            .unwrap()
            .cap_reached
    );
    let reports = indexes.status_blocking(Some("proj"), &root).unwrap();
    let text = tools::format_status(&reports, None);
    assert!(text.contains("stopped at its cap"), "{text}");

    // An explicit run with a bigger cap finishes and clears the flag.
    let text = indexes
        .manage_blocking(
            "proj",
            &Action::Embed {
                max_tokens: Some(1_000_000),
                wait: true,
            },
            &root,
        )
        .unwrap();
    assert!(text.contains("proj"), "{text}");
    let (done, docs) = embedded(&indexes, &root);
    assert_eq!(done, docs);
    assert!(
        !store::load_manifest(&outcome.layout.dir)
            .unwrap()
            .cap_reached
    );
    fixture.shutdown(&indexes);
}

/// An embedding thread that panics frees its slot, so the next request runs.
#[test]
fn an_embedding_thread_that_panics_lets_the_next_run_start() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let (events, panicked) = panicking_once_on("proj embedded ");
    let indexes = fixture.open_with_events(&root, false, events);
    let args = CreateArgs {
        wait: false,
        ..create_args(&root, Some(Kind::Code))
    };
    let outcome = indexes.create_blocking(&args).unwrap();
    assert!(
        matches!(outcome.embedding, EmbedPlan::Background),
        "{:?}",
        outcome.embedding
    );
    let running_here =
        || indexes.status_blocking(Some("proj"), &root).unwrap()[0].embed_running_here;
    wait_for("the embedding thread to panic", || {
        panicked.load(Ordering::SeqCst)
    });
    wait_for("the slot to say no run is in progress", || !running_here());
    write(
        &root.join("src/more.rs"),
        "/// One more helper.\npub fn one_more() {}\n",
    );
    let text = indexes
        .manage_blocking("proj", &Action::Rebuild, &root)
        .unwrap();
    assert!(text.contains("in the background"), "{text}");
    wait_for("the next run", || {
        let (done, docs) = embedded(&indexes, &root);
        docs > outcome.record.chunks && done == docs
    });
    fixture.shutdown(&indexes);
}

/// An embedding run that failed neither rebuilds the index when a watcher
/// starts nor loses its error.
#[test]
fn a_failed_embedding_run_neither_rebuilds_nor_hides_its_error() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let first = fixture.open(&root, Backend::Hash, false, fast(false));
    let outcome = first.create_blocking(&create_args(&root, None)).unwrap();
    fixture.shutdown(&first);
    let mut manifest = store::load_manifest(&outcome.layout.dir).unwrap();
    manifest.embed_error = Some("embedding failed (Voyage said 429)".into());
    store::save_manifest(&outcome.layout.dir, &manifest).unwrap();
    let built = manifest.last_build.clone().unwrap().at;
    let watching = fixture.open(&root, Backend::Hash, true, fast(false));
    wait_for("the watcher", || {
        matches!(
            watching.live_state(&outcome.layout).0,
            WatchState::Watching { .. }
        )
    });
    // Three of the test timing's quiet periods, after which a rebuild is due.
    std::thread::sleep(Duration::from_millis(600));
    let manifest = store::load_manifest(&outcome.layout.dir).unwrap();
    assert_eq!(manifest.last_build.unwrap().at, built, "it was rebuilt");
    let reports = watching.status_blocking(Some("proj"), &root).unwrap();
    let text = tools::format_status(&reports, None);
    assert!(text.contains("Voyage said 429"), "{text}");
    fixture.shutdown(&watching);
}

// ----- freshness -----

#[test]
fn one_shot_mode_reconciles_when_searching() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    let outcome = indexes.create_blocking(&create_args(&root, None)).unwrap();
    assert!(
        outcome.notes.iter().any(|note| note.contains("monitored")),
        "{:?}",
        outcome.notes
    );
    assert_eq!(indexes.watch_state(&outcome.layout), WatchState::Off);

    write(
        &root.join("src/cache.rs"),
        "/// Evicts the least recently used entry when the cache is full.\npub fn evict_lru() {}\n",
    );
    std::fs::remove_file(root.join("src/config.rs")).unwrap();
    let output = indexes
        .search_blocking(&search_args(&root, "evict least recently used", None))
        .unwrap();
    assert_eq!(hit_files(&output)[0], "src/cache.rs", "{output:?}");
    assert!(
        output
            .notes
            .iter()
            .any(|note| note.contains("rebuilt before searching")),
        "{:?}",
        output.notes
    );
    let output = indexes
        .search_blocking(&search_args(&root, "parse_config", Some(ModeArg::Lexical)))
        .unwrap();
    assert!(
        output.hits.iter().all(|hit| hit.display != "src/config.rs"),
        "{output:?}"
    );
    // The changes were embedded too (one-shot mode embeds them synchronously).
    let (done, docs) = embedded(&indexes, &root);
    assert_eq!(done, docs);
    // Nothing changed, so no rebuild.
    let output = indexes
        .search_blocking(&search_args(&root, "evict", None))
        .unwrap();
    assert!(output.notes.is_empty(), "{:?}", output.notes);
    fixture.shutdown(&indexes);
}

fn monitoring_picks_up_edits(force_polling: bool) {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::Hash, true, fast(force_polling));
    let outcome = indexes.create_blocking(&create_args(&root, None)).unwrap();
    let layout = outcome.layout.clone();
    wait_for("the watcher", || {
        matches!(indexes.live_state(&layout).0, WatchState::Watching { .. })
    });
    assert_eq!(
        indexes.live_state(&layout).0,
        WatchState::Watching {
            polling: force_polling
        }
    );
    let summary = indexes.summary().unwrap();
    assert!(summary.contains("monitored"), "{summary}");
    let built = store::load_manifest(&layout.dir)
        .unwrap()
        .last_build
        .unwrap()
        .at;
    let (_, _, delivered, _) = indexes.live_state(&layout);

    write(
        &root.join("src/queue.rs"),
        "/// A bounded queue that drops the oldest item when full.\npub struct RingQueue;\n",
    );
    wait_for("the background rebuild", || {
        let (_, dirty, now_delivered, covered) = indexes.live_state(&layout);
        now_delivered > delivered && covered >= now_delivered && !dirty
    });
    let rebuilt = store::load_manifest(&layout.dir)
        .unwrap()
        .last_build
        .unwrap()
        .at;
    assert!(rebuilt > built);
    let output = indexes
        .search_blocking(&search_args(&root, "RingQueue", Some(ModeArg::Lexical)))
        .unwrap();
    assert_eq!(hit_files(&output)[0], "src/queue.rs", "{output:?}");
    // The new chunk is embedded in the background.
    wait_for("the background embedding", || {
        let (done, docs) = embedded(&indexes, &root);
        done == docs && docs == outcome.record.chunks + 1
    });

    // A second Arko finds the index watched.
    let other = fixture.open(&root, Backend::Hash, true, fast(force_polling));
    wait_for("the second watcher to give up", || {
        other.watch_state(&layout) == WatchState::Elsewhere
    });
    let output = other
        .search_blocking(&search_args(&root, "bounded queue", None))
        .unwrap();
    assert_eq!(hit_files(&output)[0], "src/queue.rs", "{output:?}");
    let reports = other.status_blocking(Some("proj"), &root).unwrap();
    assert!(
        tools::format_status(&reports, None)
            .contains("monitor: on, watched by another Arko process")
    );

    // When the first Arko exits (shutdown joins its threads), the second takes over.
    let started = Instant::now();
    fixture.shutdown(&indexes);
    assert!(started.elapsed() < Duration::from_secs(15));
    wait_for("the second Arko to take over", || {
        other.watch_state(&layout).active()
    });
    write(&root.join("src/late.rs"), "pub fn arrived_late() {}\n");
    wait_for("the second Arko's rebuild", || {
        let (_, dirty, delivered, covered) = other.live_state(&layout);
        delivered > 0 && covered >= delivered && !dirty
    });
    let output = other
        .search_blocking(&search_args(&root, "arrived_late", Some(ModeArg::Lexical)))
        .unwrap();
    assert_eq!(hit_files(&output)[0], "src/late.rs", "{output:?}");

    // Monitoring off stops the watcher.
    let text = other
        .manage_blocking("proj", &Action::Monitor(false), &root)
        .unwrap();
    assert!(text.contains("no longer monitored"), "{text}");
    wait_for("the watcher to stop", || {
        !other.watch_state(&layout).active()
    });
    assert!(!store::load_manifest(&layout.dir).unwrap().monitor);
    fixture.shutdown(&other);
}

#[test]
fn monitoring_picks_up_edits_native() {
    monitoring_picks_up_edits(false);
}

#[test]
fn monitoring_picks_up_edits_polling() {
    monitoring_picks_up_edits(true);
}

#[test]
fn watching_reconciles_changes_made_while_arko_was_not_running() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let first = fixture.open(&root, Backend::Hash, true, fast(false));
    let outcome = first.create_blocking(&create_args(&root, None)).unwrap();
    let layout = outcome.layout.clone();
    wait_for("the watcher", || first.watch_state(&layout).active());
    fixture.shutdown(&first);

    write(
        &root.join("src/offline.rs"),
        "pub fn written_while_away() {}\n",
    );
    let second = fixture.open(&root, Backend::Hash, true, fast(false));
    wait_for("the reconciling rebuild", || {
        let (watch, dirty, _, _) = second.live_state(&layout);
        matches!(watch, WatchState::Watching { .. })
            && !dirty
            && store::load_manifest(&layout.dir)
                .unwrap()
                .last_build
                .is_some_and(|build| build.chunks > outcome.record.chunks)
    });
    let output = second
        .search_blocking(&search_args(
            &root,
            "written_while_away",
            Some(ModeArg::Lexical),
        ))
        .unwrap();
    assert_eq!(hit_files(&output)[0], "src/offline.rs", "{output:?}");
    fixture.shutdown(&second);
}

/// A watcher thread that panics stops reading as watched, and the next search
/// brings the index up to date and starts a new watcher.
#[test]
fn a_watcher_that_panics_is_replaced_by_the_next_search() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let (events, panicked) = panicking_once_on("Watching ");
    let indexes = fixture.open_with_events(&root, true, events);
    let outcome = indexes.create_blocking(&create_args(&root, None)).unwrap();
    let layout = outcome.layout.clone();
    wait_for("the watcher to panic", || panicked.load(Ordering::SeqCst));
    wait_for("the index to stop reading as watched", || {
        !indexes.watch_state(&layout).active()
    });
    write(
        &root.join("src/after.rs"),
        "pub fn written_after_the_panic() {}\n",
    );
    let output = indexes
        .search_blocking(&search_args(
            &root,
            "written_after_the_panic",
            Some(ModeArg::Lexical),
        ))
        .unwrap();
    assert_eq!(
        hit_files(&output).first().map(String::as_str),
        Some("src/after.rs"),
        "{output:?}"
    );
    wait_for("a new watcher", || {
        matches!(indexes.live_state(&layout).0, WatchState::Watching { .. })
    });
    fixture.shutdown(&indexes);
}

/// A watcher whose state can't be opened fails and tries again, and once
/// monitoring is off it reads off.
#[test]
fn a_failing_watcher_retries_and_stops_as_off() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::Hash, true, fast(true));
    let args = CreateArgs {
        monitor: false,
        ..create_args(&root, None)
    };
    let outcome = indexes.create_blocking(&args).unwrap();
    // The polling watcher's state directory becomes a file, so opening it fails.
    std::fs::remove_dir_all(outcome.layout.watch()).unwrap();
    std::fs::write(outcome.layout.watch(), "not a directory").unwrap();
    indexes
        .manage_blocking("proj", &Action::Monitor(true), &root)
        .unwrap();
    wait_for("the watcher to fail", || {
        matches!(
            indexes.live_state(&outcome.layout).0,
            WatchState::Retrying(_)
        )
    });
    let reports = indexes.status_blocking(Some("proj"), &root).unwrap();
    let text = tools::format_status(&reports, None);
    assert!(text.contains("), trying again"), "{text}");
    indexes
        .manage_blocking("proj", &Action::Monitor(false), &root)
        .unwrap();
    assert_eq!(indexes.live_state(&outcome.layout).0, WatchState::Off);
    fixture.shutdown(&indexes);
}

/// An index made in fin's data folder (Arko's name before) opens, searches
/// and keeps current after the folder moves to Arko's, because nothing saved in it
/// names the data folder.
#[test]
fn an_index_moved_with_fins_data_folder_still_works() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let share = fixture.base.join("share");
    let open = |data: &Path| {
        let _guard = fixture.runtime.enter();
        let indexes =
            Indexes::open_with(data, &root, Backend::Hash, true, fast(false), None).unwrap();
        indexes.watch_related();
        indexes
    };
    let first = open(&share.join("fin"));
    let outcome = first.create_blocking(&create_args(&root, None)).unwrap();
    wait_for("the watcher", || {
        first.watch_state(&outcome.layout).active()
    });
    fixture.shutdown(&first);

    assert_eq!(crate::session::move_fin_folder_in(&share), None);
    let second = open(&share.join("arko"));
    let moved = Layout::new(
        store::indexes_dir(&share.join("arko")).join(outcome.layout.dir.file_name().unwrap()),
    );
    wait_for("the watcher of the moved index", || {
        second.watch_state(&moved).active()
    });
    for (mode, searched, query) in [
        (ModeArg::Semantic, Mode::Dense, "retry backoff delay"),
        (ModeArg::Hybrid, Mode::Hybrid, "retry backoff delay"),
        (ModeArg::Lexical, Mode::Lexical, "retry_with_backoff"),
    ] {
        let output = second
            .search_blocking(&search_args(&root, query, Some(mode)))
            .unwrap();
        assert_eq!(output.searched[0].2, searched, "{mode:?}");
        assert_eq!(hit_files(&output)[0], "src/net.rs", "{mode:?}: {output:?}");
        assert!(output.notes.is_empty(), "{:?}", output.notes);
    }
    write(
        &root.join("src/later.rs"),
        "pub fn written_after_the_move() {}\n",
    );
    wait_for("the moved index to take the new file", || {
        second
            .search_blocking(&search_args(
                &root,
                "written_after_the_move",
                Some(ModeArg::Lexical),
            ))
            .is_ok_and(|output| {
                hit_files(&output).first().map(String::as_str) == Some("src/later.rs")
            })
    });
    fixture.shutdown(&second);
}

#[test]
fn an_index_folder_inside_the_tree_is_left_out() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    // Arko's data folder inside the indexed tree, not ignored. Its manifests
    // change with every build, which must not trigger the next one.
    let data = root.join("state data/arko");
    let indexes = {
        let _guard = fixture.runtime.enter();
        Indexes::open_with(&data, &root, Backend::Hash, true, fast(false), None).unwrap()
    };
    let outcome = indexes.create_blocking(&create_args(&root, None)).unwrap();
    let layout = outcome.layout.clone();
    wait_for("the watcher", || {
        matches!(indexes.live_state(&layout).0, WatchState::Watching { .. })
    });
    wait_for("embedding", || {
        let (done, docs) = embedded(&indexes, &root);
        done == docs && docs > 0
    });
    let built = store::load_manifest(&layout.dir)
        .unwrap()
        .last_build
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let now = store::load_manifest(&layout.dir)
        .unwrap()
        .last_build
        .unwrap();
    assert_eq!(now.at, built.at, "no rebuild without a change");
    assert_eq!(now.chunks, outcome.record.chunks);
    let output = indexes
        .search_blocking(&search_args(
            &root,
            "max_embed_tokens",
            Some(ModeArg::Lexical),
        ))
        .unwrap();
    assert!(output.hits.is_empty(), "{output:?}");
    fixture.shutdown(&indexes);
}

// ----- tools -----

#[test]
fn tool_parameters_are_parsed_leniently() {
    assert_eq!(tools::parse_count("5M"), Some(5_000_000));
    assert_eq!(tools::parse_count("200k"), Some(200_000));
    assert_eq!(tools::parse_count("1.5m"), Some(1_500_000));
    assert_eq!(tools::parse_count("5,000,000"), Some(5_000_000));
    assert_eq!(tools::parse_count("5_000"), Some(5_000));
    assert_eq!(tools::parse_count("0"), Some(0));
    assert_eq!(tools::parse_count("-1"), None);
    assert_eq!(tools::parse_count("lots"), None);

    let detail = |value: Value| tools::parse_detail(&params(json!({ "detail": value })));
    assert_eq!(tools::parse_detail(&params(json!({}))).unwrap(), None);
    assert_eq!(
        detail(json!(" Brief ")).unwrap(),
        Some(tools::Detail::Brief(3))
    );
    assert_eq!(detail(json!("full")).unwrap(), Some(tools::Detail::Full));
    assert!(detail(json!("everything")).is_err());
    // Lexical search lists its matches' lines, and the others show the top
    // results whole, unless detail says.
    assert_eq!(
        tools::detail_for(Some(ModeArg::Lexical), None),
        tools::Detail::Lines
    );
    for mode in [None, Some(ModeArg::Hybrid), Some(ModeArg::Semantic)] {
        assert_eq!(tools::detail_for(mode, None), tools::Detail::Full);
    }
    assert_eq!(
        tools::detail_for(Some(ModeArg::Lexical), Some(tools::Detail::Full)),
        tools::Detail::Full
    );

    let p = params(json!({
        "yes": "Yes", "no": "off", "num": 1, "flag": true, "bad": "maybe", "blank": "",
        "list": "src/**, *.rs,\ndocs/", "array": ["a", "b, c"], "count": "300k",
        "number": 42, "neg": -3
    }));
    assert_eq!(tools::bool_param(&p, "yes").unwrap(), Some(true));
    assert_eq!(tools::bool_param(&p, "no").unwrap(), Some(false));
    assert_eq!(tools::bool_param(&p, "num").unwrap(), Some(true));
    assert_eq!(tools::bool_param(&p, "flag").unwrap(), Some(true));
    assert_eq!(tools::bool_param(&p, "blank").unwrap(), None);
    assert_eq!(tools::bool_param(&p, "missing").unwrap(), None);
    assert!(tools::bool_param(&p, "bad").is_err());
    assert_eq!(
        tools::list_param(&p, "list"),
        vec!["src/**", "*.rs", "docs/"]
    );
    assert_eq!(tools::list_param(&p, "array"), vec!["a", "b", "c"]);
    assert!(tools::list_param(&p, "missing").is_empty());
    assert_eq!(tools::count_param(&p, "count").unwrap(), Some(300_000));
    assert_eq!(tools::count_param(&p, "number").unwrap(), Some(42));
    assert!(tools::count_param(&p, "neg").is_err());
    assert_eq!(tools::str_param(&p, "number").as_deref(), Some("42"));
    assert_eq!(tools::str_param(&p, "blank"), None);

    let cwd = Path::new("/w/proj");
    let search = tools::parse_search(
        &params(json!({"query": " retry logic ", "limit": "500", "mode": "hybrid"})),
        cwd,
    )
    .unwrap();
    assert_eq!(search.query, "retry logic");
    // (The search keeps semantic and hybrid ones to 50.)
    assert_eq!(search.limit, Some(500));
    assert_eq!(search.mode, Some(ModeArg::Hybrid));
    assert_eq!(search.cwd, cwd);
    let search = tools::parse_search(
        &params(
            json!({"query": "q", "limit": 0, "mode": "Semantic", "index": "arko",
                       "path_prefix": "src/", "language": "rust"}),
        ),
        cwd,
    )
    .unwrap();
    assert_eq!(search.limit, Some(1));
    assert_eq!(search.mode, Some(ModeArg::Semantic));
    assert_eq!(search.index.as_deref(), Some("arko"));
    assert_eq!(search.path_prefix.as_deref(), Some("src/"));
    assert_eq!(search.language.as_deref(), Some("rust"));
    let search =
        tools::parse_search(&params(json!({"query": "q", "mode": "lexical"})), cwd).unwrap();
    assert_eq!(search.limit, None);
    assert_eq!(search.mode, Some(ModeArg::Lexical));
    assert!(!search.exhaustive && !search.ignore_case);
    assert_eq!(search.after, None);
    let search = tools::parse_search(
        &params(json!({"query": "q", "mode": "lexical", "after": " src/a.rs:804:5 "})),
        cwd,
    )
    .unwrap();
    assert_eq!(search.after.as_deref(), Some("src/a.rs:804:5"));
    let search = tools::parse_search(
        &params(json!({"query": "q", "mode": "lexical", "limit": "5k",
                       "exhaustive": "yes", "ignore_case": true})),
        cwd,
    )
    .unwrap();
    assert_eq!(search.limit, Some(5_000));
    assert!(search.exhaustive && search.ignore_case);
    // Only `after` is restricted to lexical mode; `ignore_case` and `exhaustive`
    // are accepted in semantic and hybrid modes too.
    for mode in ["hybrid", "semantic"] {
        let error = tools::parse_search(
            &params(json!({"query": "q", "mode": mode, "after": "src/a.rs:804:5"})),
            cwd,
        )
        .unwrap_err();
        assert!(error.to_string().contains("mode lexical"), "{error}");
        assert!(
            tools::parse_search(
                &params(
                    json!({"query": "q", "mode": mode, "ignore_case": true, "exhaustive": true})
                ),
                cwd,
            )
            .is_ok()
        );
    }
    assert!(
        tools::parse_search(
            &params(json!({"query": "q", "mode": "hybrid", "after": ""})),
            cwd,
        )
        .is_ok()
    );
    // Omitting `mode` defaults to hybrid.
    let defaulted = tools::parse_search(&params(json!({"query": "q"})), cwd).unwrap();
    assert_eq!(defaulted.mode, Some(ModeArg::Hybrid));
    assert!(tools::parse_search(&params(json!({"query": "q", "mode": "fuzzy"})), cwd).is_err());
    assert!(tools::parse_search(&params(json!({"query": "  ", "mode": "hybrid"})), cwd).is_err());

    // `lifetime` is required for `index_create`.
    assert!(tools::parse_create(&params(json!({})), cwd).is_err());
    let create = tools::parse_create(&params(json!({"lifetime": "keep"})), cwd).unwrap();
    assert_eq!(create.path, PathBuf::from("."));
    assert_eq!(create.kind, None);
    assert_eq!(create.lifetime, Some(Lifetime::Keep));
    assert!(create.monitor && !create.wait && !create.defer_embed);
    assert_eq!(create.embed, None);
    let create = tools::parse_create(
        &params(json!({"path": "docs", "kind": "docs", "monitor": "false", "embed": false,
                       "max_embed_tokens": "1M", "include": "*.md", "wait": true, "name": "handbook",
                       "lifetime": 14})),
        cwd,
    )
    .unwrap();
    assert_eq!(create.kind, Some(Kind::Docs));
    assert_eq!(create.lifetime, Some(Lifetime::Days(14)));
    assert!(!create.monitor && create.wait);
    assert_eq!(create.embed, Some(false));
    assert_eq!(create.max_embed_tokens, Some(1_000_000));
    assert_eq!(create.include, vec!["*.md"]);
    assert_eq!(create.name.as_deref(), Some("handbook"));
    assert!(
        tools::parse_create(&params(json!({"kind": "poetry", "lifetime": "keep"})), cwd).is_err()
    );
    assert!(tools::parse_create(&params(json!({"lifetime": 0})), cwd).is_err());
    assert!(tools::parse_create(&params(json!({"lifetime": "forever"})), cwd).is_err());

    let action = |value: Value| tools::parse_action(&params(value));
    assert_eq!(
        action(json!({"action": "rebuild"})).unwrap(),
        Action::Rebuild
    );
    assert_eq!(
        action(json!({"action": "Update"})).unwrap(),
        Action::Rebuild
    );
    assert_eq!(
        action(json!({"action": "embed", "max_tokens": "2M", "wait": "yes"})).unwrap(),
        Action::Embed {
            max_tokens: Some(2_000_000),
            wait: true
        }
    );
    assert_eq!(
        action(json!({"action": "estimate"})).unwrap(),
        Action::Estimate
    );
    assert_eq!(
        action(json!({"action": "monitor-on"})).unwrap(),
        Action::Monitor(true)
    );
    assert_eq!(
        action(json!({"action": "monitor off"})).unwrap(),
        Action::Monitor(false)
    );
    assert_eq!(action(json!({"action": "delete"})).unwrap(), Action::Remove);
    assert!(action(json!({"action": "explode"})).is_err());
    assert!(action(json!({})).is_err());
}

#[test]
fn tool_definitions() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::voyage(None), false, fast(false));
    let tools = indexes.tools();
    let names: Vec<String> = tools.iter().map(|tool| tool.definition().name).collect();
    assert_eq!(names, TOOL_NAMES);
    for tool in &tools {
        let definition = tool.definition();
        let read_only = matches!(definition.name.as_str(), "index_search" | "index_status");
        assert_eq!(definition.parallel_safe, read_only, "{}", definition.name);
        assert_eq!(definition.read_only, read_only, "{}", definition.name);
        assert_eq!(
            matches!(tool.recovery_mode(), ToolRecovery::Rerun),
            read_only,
            "{}",
            definition.name
        );
        assert!(definition.description.len() > 80, "{}", definition.name);
        let mut seen = std::collections::HashSet::new();
        for parameter in &definition.parameters {
            assert!(seen.insert(parameter.name.clone()), "{}", parameter.name);
        }
    }
    let search = tools[0].definition();
    assert!(search.description.contains("rg"), "{}", search.description);
    assert!(
        search
            .parameters
            .iter()
            .any(|p| p.name == "query" && p.required)
    );
    fixture.shutdown(&indexes);
}

#[test]
fn tools_create_search_and_remove() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    let tools = indexes.tools();
    let tool = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.definition().name == name)
            .unwrap()
            .clone()
    };
    let context = TaskContext::new(uuid::Uuid::new_v4(), "test", "", &root);
    let run = |name: &str, value: Value| {
        fixture
            .runtime
            .block_on(tool(name).execute(&context, params(value)))
    };

    let error = run("index_search", json!({"query": "retry", "mode": "hybrid"})).unwrap_err();
    assert!(error.to_string().contains("index_create"), "{error}");
    let text = run("index_status", json!({})).unwrap();
    assert!(text.contains("no indexes"), "{text}");

    let text = run(
        "index_create",
        json!({"path": ".", "kind": "auto", "wait": "true", "lifetime": 7}),
    )
    .unwrap();
    assert!(text.contains("Created index proj (code)"), "{text}");
    assert!(text.contains("Estimate:"), "{text}");
    let text = run(
        "index_search",
        json!({"query": "exponential backoff", "limit": "2", "mode": "hybrid"}),
    )
    .unwrap();
    // The top result comes whole, with line numbers.
    let first = text.lines().nth(2).unwrap();
    assert!(first.starts_with("src/net.rs:"), "{text}");
    assert!(text.contains("\n1 | //! Networking helpers.\n"), "{text}");
    assert!(
        text.lines().next().unwrap().contains("(code, hybrid)"),
        "{text}"
    );
    assert!(text.contains("retry_with_backoff"), "{text}");
    let text = run("index_status", json!({"index": "proj"})).unwrap();
    assert!(text.starts_with("proj — code"), "{text}");
    let text = run("index_manage", json!({"action": "rebuild"})).unwrap();
    assert!(text.starts_with("Rebuilt proj"), "{text}");
    let error = run(
        "index_manage",
        json!({"action": "rebuild", "index": "nope"}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("nope"), "{error}");
    let text = run("index_manage", json!({"action": "remove", "index": "proj"})).unwrap();
    assert!(text.contains("proj"), "{text}");
    assert!(indexes.all().is_empty());
    assert!(
        !store::indexes_dir(&fixture.data())
            .join(store::index_id(&root, Kind::Code))
            .exists()
    );
    fixture.shutdown(&indexes);
}

#[test]
fn search_and_status_formatting() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("net.rs");
    std::fs::write(
        &file,
        "    /// Retries.\n\n    pub fn retry() {\n        loop {}\n    }\n",
    )
    .unwrap();
    let hit = Hit {
        index: "proj".into(),
        display: "src/net.rs".into(),
        file: file.clone(),
        start_line: 1,
        end_line: 5,
        start_byte: 0,
        end_byte: 1000,
        column: 1,
        scope: "fn retry".into(),
        symbol: "retry".into(),
        changed: true,
        lines: None,
    };
    let output = SearchOutput {
        hits: vec![hit.clone()],
        searched: vec![("proj".into(), Kind::Code, Mode::Hybrid)],
        notes: vec!["a note".into()],
        query_tokens: 3,
        ..SearchOutput::default()
    };
    assert_eq!(
        tools::format_search("retry", &output, 3),
        [
            "1 result from proj (code, hybrid):",
            "src/net.rs:1-5 · fn retry (file changed since indexed)",
            "    /// Retries.",
            "    pub fn retry() {",
            "        loop {}",
            "note: a note",
        ]
        .join("\n")
    );
    let several = SearchOutput {
        hits: vec![
            hit.clone(),
            Hit {
                index: "docs".into(),
                scope: String::new(),
                changed: false,
                ..hit.clone()
            },
        ],
        searched: vec![
            ("proj".into(), Kind::Code, Mode::Hybrid),
            ("docs".into(), Kind::Docs, Mode::Lexical),
        ],
        ..SearchOutput::default()
    };
    let text = tools::format_search("retry", &several, 0);
    assert_eq!(
        text,
        "2 results from proj (code, hybrid), docs (docs, lexical):\n\
         [proj] src/net.rs:1-5 · fn retry (file changed since indexed)\n\
         [docs] src/net.rs:1-5 · retry"
    );
    // The first line reads back as the indexes searched and the count, as
    // the transcript reads it.
    let names = |names: &[&str]| {
        names
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        tools::read_search_header(text.lines().next().unwrap()),
        Some((names(&["proj", "docs"]), 2))
    );
    let none = SearchOutput {
        searched: vec![("proj".into(), Kind::Code, Mode::Dense)],
        ..SearchOutput::default()
    };
    let text = tools::format_search("zz", &none, 3);
    assert!(
        text.starts_with("No results for \"zz\" in proj (code, semantic)."),
        "{text}"
    );
    assert_eq!(
        tools::read_search_header(text.lines().next().unwrap()),
        Some((names(&["proj"]), 0))
    );
    // A query with " in " and quotes in it doesn't move the indexes.
    let text = tools::format_search("state \"in\" session in x", &several_none(), 3);
    assert_eq!(
        tools::read_search_header(text.lines().next().unwrap()),
        Some((names(&["proj", "docs"]), 0))
    );

    assert_eq!(tools::snippet(&file, 0, 1000, 1), vec!["/// Retries."]);
    assert!(tools::snippet(&file, 50, 10, 3).is_empty());
    assert!(tools::snippet(&temp.path().join("gone"), 0, 10, 3).is_empty());
    let wide = temp.path().join("wide.txt");
    std::fs::write(&wide, "y".repeat(500)).unwrap();
    let line = &tools::snippet(&wide, 0, 500, 1)[0];
    assert!(line.ends_with('…') && line.chars().count() < 200);

    let text = tools::format_status(&[], None);
    assert!(text.contains("no indexes"), "{text}");
}

/// A search of two indexes that found nothing.
fn several_none() -> SearchOutput {
    SearchOutput {
        searched: vec![
            ("proj".into(), Kind::Code, Mode::Hybrid),
            ("docs".into(), Kind::Docs, Mode::Lexical),
        ],
        ..SearchOutput::default()
    }
}

/// The status line for the monitor says what the setting and the watcher
/// mean together. A monitored index this process doesn't watch is brought up
/// to date when an agent searches it, which is not the same as another Arko
/// watching it.
#[test]
fn the_monitor_line_says_who_keeps_the_index_current() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let work = fixture.base.join("work");
    std::fs::create_dir_all(&work).unwrap();
    // The words alone.
    let on = "on, not watching it now (brought up to date when an agent searches it)";
    assert_eq!(monitor_words(true, &WatchState::Off), on);
    assert_eq!(monitor_words(false, &WatchState::Off), "off");
    assert_eq!(
        monitor_words(true, &WatchState::Elsewhere),
        "on, watched by another Arko process"
    );
    assert_eq!(
        monitor_words(true, &WatchState::Watching { polling: true }),
        "on, watching (polling)"
    );
    // An index made by a one-shot Arko, which watches nothing.
    let created = fixture.open(&root, Backend::Hash, false, fast(false));
    let outcome = created.create_blocking(&create_args(&root, None)).unwrap();
    let text = tools::format_create(&outcome);
    assert!(text.contains(&format!("Monitor: {on}.")), "{text}");
    fixture.shutdown(&created);
    // An Arko working elsewhere watches no index outside its workspace, and
    // no other Arko watches this one.
    let indexes = fixture.open(&work, Backend::Hash, true, fast(false));
    let status = |indexes: &Indexes| {
        let reports = indexes.status_blocking(Some("proj"), &work).unwrap();
        let text = tools::format_status(&reports, None);
        text.lines()
            .find(|line| line.trim_start().starts_with("monitor:"))
            .unwrap()
            .to_string()
    };
    assert_eq!(status(&indexes), format!("  monitor: {on}"));
    // Turned off, it says only that.
    indexes
        .manage_blocking("proj", &Action::Monitor(false), &work)
        .unwrap();
    assert_eq!(status(&indexes), "  monitor: off");
    fixture.shutdown(&indexes);
}

/// A status from an Arko that watches nothing, such as `arko index status` in
/// another terminal, says when another Arko watches the index.
#[test]
fn status_says_when_another_arko_watches_the_index() {
    for force_polling in [false, true] {
        let fixture = Fixture::new();
        let root = fixture.code_project();
        let work = fixture.base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let watching = fixture.open(&root, Backend::Hash, true, fast(force_polling));
        let outcome = watching.create_blocking(&create_args(&root, None)).unwrap();
        wait_for("the watcher", || {
            matches!(
                watching.live_state(&outcome.layout).0,
                WatchState::Watching { .. }
            )
        });
        let other = fixture.open(&work, Backend::Hash, true, fast(force_polling));
        let status = |indexes: &Indexes| {
            let reports = indexes.status_blocking(Some("proj"), &work).unwrap();
            tools::format_status(&reports, None)
        };
        assert!(
            status(&other).contains("monitor: on, watched by another Arko process"),
            "{}",
            status(&other)
        );
        // Once the watcher ends, nobody watches it. (A process another test
        // forks may hold the lock's file for a moment, until it runs its
        // program.)
        fixture.shutdown(&watching);
        wait_for("the watcher's lock to be free", || {
            status(&other).contains("monitor: on, not watching it now")
        });
        fixture.shutdown(&other);
    }
}

#[test]
fn status_survives_unreadable_manifests() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    write(
        &store::indexes_dir(&fixture.data()).join("broken/index.json"),
        "[]",
    );
    let indexes = fixture.open(&root, Backend::voyage(None), true, fast(false));
    assert!(indexes.summary().is_none());
    let reports = indexes.status_blocking(None, &root).unwrap();
    assert_eq!(reports.len(), 1);
    let text = tools::format_status(&reports, None);
    assert!(text.starts_with("broken — unreadable"), "{text}");
    fixture.shutdown(&indexes);
}

/// Two sessions in one process (an app runs several) update one index's
/// manifest at once, and neither undoes the other's change.
#[test]
fn two_sessions_in_one_process_keep_each_others_manifest_changes() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let first = fixture.open(&root, Backend::Hash, false, fast(false));
    let args = CreateArgs {
        monitor: false,
        embed: Some(false),
        ..create_args(&root, None)
    };
    let outcome = first.create_blocking(&args).unwrap();
    let second = fixture.open(&root, Backend::Hash, false, fast(false));
    // The second session rebuilds again and again, and each build is saved in
    // the manifest.
    let done = Arc::new(AtomicBool::new(false));
    let rebuilding = {
        let (second, root, done) = (second.clone(), root.clone(), done.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let _ = second.manage_blocking("proj", &Action::Rebuild, &root);
            }
        })
    };
    // Meanwhile the first turns monitoring on and off, and checks each time
    // that its change holds.
    let mut undone = 0;
    for turn in 0..300 {
        let on = turn % 2 == 0;
        let _ = first.manage_blocking("proj", &Action::Monitor(on), &root);
        std::thread::sleep(Duration::from_millis(5));
        let manifest = store::load_manifest(&outcome.layout.dir);
        undone += usize::from(!manifest.is_ok_and(|manifest| manifest.monitor == on));
    }
    done.store(true, Ordering::SeqCst);
    rebuilding.join().unwrap();
    assert_eq!(undone, 0, "changes lost or unreadable");
    fixture.shutdown(&first);
    fixture.shutdown(&second);
}

// ----- the CLI -----

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: IndexCommand,
}

fn cli(fixture: &Fixture, cwd: &Path, args: &[&str]) -> Result<ExitCode, String> {
    let mut argv = vec!["arko-index"];
    argv.extend_from_slice(args);
    let command = Cli::try_parse_from(argv)
        .map_err(|error| error.to_string())?
        .command;
    fixture.runtime.block_on(run_command_with(
        command,
        &fixture.data(),
        cwd,
        Backend::Hash,
        fast(false),
    ))
}

#[test]
fn cli_commands() {
    use clap::CommandFactory;
    Cli::command().debug_assert();

    let fixture = Fixture::new();
    let root = fixture.code_project();
    let path = root.to_string_lossy().into_owned();
    assert_eq!(
        cli(
            &fixture,
            &fixture.base,
            &["create", &path, "--max-embed-tokens", "1M"]
        ),
        Ok(ExitCode::SUCCESS)
    );
    let id = store::index_id(&root, Kind::Code);
    let manifest = store::load_manifest(&store::indexes_dir(&fixture.data()).join(&id)).unwrap();
    assert_eq!(manifest.max_embed_tokens, 1_000_000);
    assert!(manifest.embed_tokens > 0, "the CLI embeds before returning");

    for args in [
        &["list"][..],
        &["status"],
        &["status", "proj"],
        &["search", "retry", "backoff"],
        &[
            "search",
            "-m",
            "lexical",
            "-n",
            "3",
            "--language",
            "rust",
            "parse",
        ],
        &[
            "search",
            "-m",
            "lexical",
            "--after",
            "src/config.rs:1:1",
            "pub",
        ],
        &[
            "search",
            "-m",
            "lexical",
            "--after",
            "src/zzz.rs:9",
            "--exhaustive",
            "pub",
        ],
        &["update"],
        &["update", "--force"],
        &["embed", "--estimate"],
        &["embed", "--max-tokens", "10k"],
        &["monitor", "proj", "off"],
    ] {
        assert_eq!(
            cli(&fixture, &root, args),
            Ok(ExitCode::SUCCESS),
            "{args:?}"
        );
    }
    assert!(
        !store::load_manifest(&store::indexes_dir(&fixture.data()).join(&id))
            .unwrap()
            .monitor
    );
    let error = cli(&fixture, &root, &["search", "-m", "fuzzy", "x"]).unwrap_err();
    assert!(error.contains("hybrid"), "{error}");
    let error = cli(
        &fixture,
        &root,
        &["search", "-m", "lexical", "--after", "config", "pub"],
    )
    .unwrap_err();
    assert!(error.contains("\"config\" isn't a cursor"), "{error}");
    let error = cli(
        &fixture,
        &root,
        &["search", "-m", "hybrid", "--after", "src/a.rs:1:1", "pub"],
    )
    .unwrap_err();
    assert!(error.contains("mode lexical"), "{error}");
    let elsewhere = fixture.base.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let error = cli(&fixture, &elsewhere, &["update"]).unwrap_err();
    assert!(
        error.contains("`arko index create PATH`"),
        "hints name commands: {error}"
    );
    assert!(cli(&fixture, &root, &["monitor", "proj", "maybe"]).is_err());
    assert!(cli(&fixture, &root, &["create", "/"]).is_err());
    assert_eq!(
        cli(&fixture, &root, &["remove", "proj"]),
        Ok(ExitCode::SUCCESS)
    );
    assert!(store::scan(&store::indexes_dir(&fixture.data())).is_empty());
}

#[test]
fn opening_needs_a_runtime_and_reports_spending() {
    let temp = tempfile::tempdir().unwrap();
    let error = Indexes::open(temp.path(), temp.path(), None, false).unwrap_err();
    assert!(error.contains("tokio"), "{error}");
    let fixture = Fixture::new();
    let indexes = {
        let _guard = fixture.runtime.enter();
        Indexes::open(&fixture.data(), &fixture.base, Some("key".into()), true).unwrap()
    };
    assert!(indexes.has_key());
    assert_eq!(indexes.spent(), (0, 0.0));
    assert!(indexes.summary().is_none());
    assert!(
        !format!("{indexes:?}").contains("key\""),
        "the key is not shown"
    );
    fixture.shutdown(&indexes);
    assert!((embed::cost("voyage-code-4", 1_000_000) - 0.12).abs() < 1e-9);
    assert!((embed::cost("voyage-4", 1_000_000) - 0.06).abs() < 1e-9);
    assert_eq!(embed::cost(embed::HASH_MODEL, 1_000_000), 0.0);
}

// ----- Voyage (network) -----

/// Counts tokens with voyage-code-4's tokenizer (downloads it from Hugging
/// Face once), and no key is needed.
#[test]
#[ignore = "downloads the Voyage tokenizer from Hugging Face"]
fn voyage_counts_tokens_offline_of_the_api() {
    let fixture = Fixture::new();
    let descriptor = Backend::descriptor(embed::CODE_MODEL).unwrap();
    let handle = fixture.runtime.handle().clone();
    let embedder = Backend::voyage(None)
        .embedder(&descriptor, &handle, None)
        .unwrap();
    let counts =
        std::thread::spawn(move || embedder.count_tokens(&["fn main() {}", "hello world"]))
            .join()
            .unwrap()
            .unwrap();
    assert_eq!(counts.len(), 2);
    assert!(counts.iter().all(|n| *n > 0));
}

/// A live index with Voyage. Run with `VOYAGE_API_KEY=... cargo test -- --ignored
/// voyage_live` (embeds a few hundred tokens, well under a cent).
#[test]
#[ignore = "calls the Voyage API: needs VOYAGE_API_KEY"]
fn voyage_live() {
    let key = std::env::var("VOYAGE_API_KEY").expect("VOYAGE_API_KEY for this test");
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::voyage(Some(key)), false, fast(false));
    let outcome: CreateOutcome = indexes.create_blocking(&create_args(&root, None)).unwrap();
    match &outcome.embedding {
        EmbedPlan::Done(Ok(summary)) => assert_eq!(summary.embedded, summary.docs),
        other => panic!("{other:?}"),
    }
    let output = indexes
        .search_blocking(&search_args(
            &root,
            "wait longer after each failed attempt",
            Some(ModeArg::Semantic),
        ))
        .unwrap();
    assert_eq!(hit_files(&output)[0], "src/net.rs", "{output:?}");
    let (tokens, dollars) = indexes.spent();
    assert!(tokens > 0 && dollars > 0.0);
    fixture.shutdown(&indexes);
}

// ----- whole definitions and other directories -----

/// A hit on `chunk` of `file`, as a search of index `proj` returns it.
fn hit_on(file: &Path, chunk: &arko_index::bench_v1::Chunk) -> Hit {
    Hit {
        index: "proj".into(),
        display: file.file_name().unwrap().to_string_lossy().into_owned(),
        file: file.to_path_buf(),
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        start_byte: chunk.start_byte,
        end_byte: chunk.end_byte,
        column: 1,
        scope: chunk.scope.clone(),
        symbol: chunk.symbol_path.clone(),
        changed: false,
        lines: None,
    }
}

/// `fn name` with `lines` statements of `width` characters.
fn long_function(name: &str, lines: usize, width: usize) -> String {
    let mut text = format!("/// {name} does a lot.\npub fn {name}() {{\n");
    for line in 0..lines {
        let value = "x".repeat(width.saturating_sub(30));
        text.push_str(&format!("    let v{line} = \"{value}\";\n"));
    }
    text.push_str("}\n\n");
    text
}

#[test]
fn search_shows_whole_definitions() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("work.rs");
    // `big` is split into several chunks, and `small` is one.
    let text = format!(
        "{}{}{}",
        long_function("big", 120, 80),
        "/// Small.\npub fn small() -> u32 {\n    7\n}\n\n",
        long_function("huge", 260, 40),
    );
    std::fs::write(&file, &text).unwrap();
    let options = super::engine::chunk_options(Kind::Code);
    let chunks = arko_index::bench_v1::chunk_file(&file, &options).unwrap();
    let of = |symbol: &str| -> Vec<&arko_index::bench_v1::Chunk> {
        chunks
            .iter()
            .filter(|chunk| chunk.symbol_path == symbol)
            .collect()
    };
    let big = of("big");
    assert!(big.len() > 1, "{chunks:?}");
    let big_start = big[0].start_line;
    // Its closing brace, with the blank line after it not shown.
    let big_end = 123;
    assert_eq!(big.last().unwrap().end_line, big_end + 1);
    let small = of("small");
    assert_eq!(small.len(), 1, "{chunks:?}");
    let huge = of("huge");

    // A hit on the second part of `big` stands for all of it.
    let second = hit_on(&file, big[1]);
    let found = tools::whole_definition(&second, Kind::Code).unwrap();
    assert_eq!((found.start_line, found.end_line), (big_start, big_end + 1));
    assert_eq!(found.matched, (big[1].start_line, big[1].end_line));

    let output = SearchOutput {
        hits: vec![
            second.clone(),
            hit_on(&file, big[0]),
            hit_on(&file, small[0]),
            hit_on(&file, huge[0]),
        ],
        searched: vec![("proj".into(), Kind::Code, Mode::Hybrid)],
        ..SearchOutput::default()
    };
    let text = tools::format_search_detail("work", &output, tools::Detail::Full);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "4 results from proj (code, hybrid):", "{text}");
    assert_eq!(lines[1], "");
    assert_eq!(
        lines[2],
        format!(
            "work.rs:{big_start}-{big_end} · fn big (matched lines {}-{})",
            big[1].start_line, big[1].end_line
        )
    );
    assert_eq!(lines[3], format!("{big_start} | /// big does a lot."));
    assert!(text.contains(&format!("\n{big_end} | }}\n")), "{text}");
    // The first part of `big` was shown already.
    assert!(
        text.contains(&format!(
            "\nwork.rs:{}-{} · fn big: in the lines {big_start}-{big_end} shown above\n",
            big[0].start_line, big[0].end_line
        )),
        "{text}"
    );
    assert!(text.contains("\nwork.rs:"), "{text}");
    assert!(text.contains(" · fn small\n"), "{text}");
    assert!(text.contains(" | pub fn small() -> u32 {\n"), "{text}");
    // `huge` is too long to show whole, so it shows the part that matched.
    let huge_lines = huge.last().unwrap().end_line + 1 - huge[0].start_line;
    assert!(huge_lines > 200);
    assert!(
        text.contains(&format!(
            "-line definition at lines {}-",
            huge[0].start_line
        )) && text.contains("read_file shows the rest"),
        "{text}"
    );

    // Brief, with the locations and snippets of before.
    let brief = tools::format_search_detail("work", &output, tools::Detail::Brief(1));
    assert!(!brief.contains(" | "), "{brief}");
    assert!(
        brief.contains(&format!(
            "\nwork.rs:{}-{} · fn big\n    ",
            big[1].start_line, big[1].end_line
        )),
        "{brief}"
    );

    // Past the budget, results show their snippets, in rank order.
    let many = temp.path().join("many.rs");
    let text: String = (0..30)
        .map(|n| long_function(&format!("f{n}"), 30, 60))
        .collect();
    std::fs::write(&many, text).unwrap();
    let chunks = arko_index::bench_v1::chunk_file(&many, &options).unwrap();
    let output = SearchOutput {
        hits: chunks.iter().map(|chunk| hit_on(&many, chunk)).collect(),
        searched: vec![("proj".into(), Kind::Code, Mode::Lexical)],
        ..SearchOutput::default()
    };
    let text = tools::format_search_detail("f", &output, tools::Detail::Full);
    assert!(text.len() < 30_000, "{}", text.len());
    let whole = text.matches(" | pub fn f").count();
    assert!(whole > 3 && whole < 30, "{whole}");
    let first_snippet = text.find("\n    /// f").expect("snippets after the budget");
    assert!(
        !text[first_snippet..].contains(" | pub fn f"),
        "a whole result after a snippet"
    );

    // The file changed since it was indexed, so it is found by its symbol.
    let mut moved = hit_on(&file, small[0]);
    moved.start_byte += 3;
    let found = tools::whole_definition(&moved, Kind::Code).unwrap();
    assert_eq!(found.start_line, small[0].start_line);
    let mut gone = moved.clone();
    gone.symbol = "nothing_by_that_name".into();
    assert!(tools::whole_definition(&gone, Kind::Code).is_none());
}

#[test]
fn indexes_of_other_directories_are_found_by_any_path_in_them() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let work = fixture.base.join("work");
    std::fs::create_dir_all(&work).unwrap();
    // Made by an Arko that didn't monitor.
    let one_shot = fixture.open(&root, Backend::Hash, false, fast(false));
    let outcome = one_shot.create_blocking(&create_args(&root, None)).unwrap();
    fixture.shutdown(&one_shot);

    // An Arko working elsewhere, so the index isn't its workspace's.
    let indexes = fixture.open(&work, Backend::Hash, true, fast(false));
    assert_eq!(indexes.watch_state(&outcome.layout), WatchState::Off);
    for name in ["../proj", "../proj/src", "../proj/src/net.rs", "proj"] {
        let found = indexes.resolve(name, &work).unwrap();
        assert_eq!(found.manifest.root, root, "{name}");
    }
    let error = indexes.resolve("..", &work).unwrap_err();
    assert!(error.contains("no index is named"), "{error}");
    let error = indexes
        .manage_blocking("../proj/src", &Action::Remove, &work)
        .unwrap_err();
    assert!(error.contains("no index is named"), "{error}");
    let reports = indexes.status_blocking(Some("../proj/src"), &work).unwrap();
    assert_eq!(reports[0].id, outcome.layout.id());

    // No index covers the working directory, so a path_prefix picks one.
    let mut args = search_args(&work, "retry_with_backoff", Some(ModeArg::Lexical));
    let error = indexes.search_blocking(&args).unwrap_err();
    assert!(error.contains("no index covers"), "{error}");
    args.path_prefix = Some("../proj/src".into());
    let output = indexes.search_blocking(&args).unwrap();
    let net = root.join("src/net.rs").to_string_lossy().into_owned();
    assert_eq!(hit_files(&output)[0], net, "{output:?}");
    let mut args = search_args(&work, "retry_with_backoff", Some(ModeArg::Lexical));
    args.index = Some(net.clone());
    let output = indexes.search_blocking(&args).unwrap();
    assert_eq!(hit_files(&output)[0], net, "{output:?}");

    // Indexes outside the workspace are not watched even after being searched,
    // and edits are reconciled on the next search instead.
    assert_eq!(indexes.watch_state(&outcome.layout), WatchState::Off);
    write(
        &root.join("src/later.rs"),
        "pub fn later_outside_edit() {}\n",
    );
    let mut args = search_args(&work, "later_outside_edit", Some(ModeArg::Lexical));
    args.path_prefix = Some("../proj/src".into());
    let output = indexes.search_blocking(&args).unwrap();
    assert_eq!(
        hit_files(&output),
        vec![root.join("src/later.rs").to_string_lossy().into_owned()],
        "{output:?}"
    );
    assert_eq!(indexes.watch_state(&outcome.layout), WatchState::Off);
    fixture.shutdown(&indexes);
}

/// The tools' definitions without the indexes are theirs byte for byte, so a
/// conversation saved with them keeps its tool list in a session without
/// indexes.
#[test]
fn the_tools_definitions_are_known_without_the_indexes() {
    let fixture = Fixture::new();
    let indexes = {
        let _guard = fixture.runtime.enter();
        Indexes::open(&fixture.data(), &fixture.base, None, true).unwrap()
    };
    let json =
        |definition: arko_agents::ToolDefinition| serde_json::to_string(&definition).unwrap();
    assert_eq!(
        indexes
            .tools()
            .into_iter()
            .map(|tool| json(tool.definition()))
            .collect::<Vec<_>>(),
        super::tool_definitions()
            .into_iter()
            .map(json)
            .collect::<Vec<_>>()
    );
    fixture.shutdown(&indexes);
}

/// index_create and index_manage can move to the background, since a build
/// or an embedding run the call waits for takes long. The others can't.
#[test]
fn the_tools_that_can_run_long_can_move_to_the_background() {
    let fixture = Fixture::new();
    let indexes = {
        let _guard = fixture.runtime.enter();
        Indexes::open(&fixture.data(), &fixture.base, None, true).unwrap()
    };
    for tool in indexes.tools() {
        let expected = match tool.name().as_str() {
            "index_create" | "index_manage" => arko_agents::ToolBackground::Detach,
            _ => arko_agents::ToolBackground::Never,
        };
        assert_eq!(tool.background_mode(), expected, "{}", tool.name());
    }
    fixture.shutdown(&indexes);
}

/// A call's stop reaches its embedding run, which goes on on a thread of its
/// own, also once the call is gone. Once the work ends, a later stop doesn't.
#[tokio::test]
async fn a_calls_stop_reaches_its_embedding_run() {
    let source = arko_agents::StopSource::new();
    let (embedding, working) = tools::embedding_stop(&source.signal());
    source.stop("The job was stopped.");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !embedding.is_stopped() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the embedding hears of the stop");
    assert_eq!(embedding.reason().as_deref(), Some("The job was stopped."));
    drop(working);

    let source = arko_agents::StopSource::new();
    let (embedding, working) = tools::embedding_stop(&source.signal());
    drop(working);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    source.stop("Too late.");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!embedding.is_stopped());
}

/// Builds, embedding runs, estimates and censuses count their chunks,
/// files, texts and days one or many.
#[test]
fn index_counts_are_one_or_many() {
    use super::engine::{EmbedSummary, Estimate, chunks_in_files, fmt_ago, fmt_counted};
    assert_eq!(fmt_counted(0, "chunk", "chunks"), "0 chunks");
    assert_eq!(fmt_counted(1, "chunk", "chunks"), "1 chunk");
    assert_eq!(fmt_counted(2, "chunk", "chunks"), "2 chunks");
    assert_eq!(fmt_counted(1_240, "chunk", "chunks"), "1,240 chunks");
    assert_eq!(chunks_in_files(0, 0), "0 chunks in 0 files");
    assert_eq!(chunks_in_files(1, 1), "1 chunk in 1 file");
    assert_eq!(chunks_in_files(2, 2), "2 chunks in 2 files");

    let days = |days: i64| fmt_ago(chrono::Utc::now() - chrono::Duration::days(days));
    assert_eq!(days(1), "1 day ago");
    assert_eq!(days(2), "2 days ago");

    let run = |new, too_long| {
        EmbedSummary {
            tokens: 300,
            dollars: 0.0,
            new,
            borrowed: 0,
            embedded: 4,
            docs: 4,
            cap: 0,
            cap_reached: false,
            stopped: false,
            superseded: false,
            not_embedded_tokens: 0,
            too_long,
        }
        .line()
    };
    assert_eq!(
        run(0, 0),
        "embedded 0 texts for 300 tokens ($0), and 4/4 chunks have vectors"
    );
    assert!(
        run(1, 0).starts_with("embedded 1 text for"),
        "{}",
        run(1, 0)
    );
    assert!(
        run(2, 0).starts_with("embedded 2 texts for"),
        "{}",
        run(2, 0)
    );
    assert!(run(1, 1).ends_with(". 1 chunk is too long to embed (found by keywords)"));
    assert!(run(1, 2).ends_with(". 2 chunks are too long to embed (found by keywords)"));

    let estimate = |texts| {
        Estimate {
            tokens: 300,
            texts,
            dollars: 0.0,
            model: "voyage-code-4".into(),
            embedded: 0,
            docs: 2,
        }
        .line()
    };
    assert!(estimate(1).starts_with("embedding 1 chunk would take"));
    assert!(estimate(2).starts_with("embedding 2 chunks would take"));

    let census = |other| Census {
        prose: 3,
        code: 5,
        other,
        truncated: false,
    };
    assert_eq!(census(0).describe(), "3 prose, 5 source, 0 other files");
    assert_eq!(census(1).describe(), "3 prose, 5 source, 1 other file");
    assert_eq!(census(2).describe(), "3 prose, 5 source, 2 other files");
}

// ----- exact (lexical) search -----

/// A project where `fetch_widget` is called from many functions in many
/// files, beside functions with words that only look like it.
fn callers_project(fixture: &Fixture, files: usize) -> PathBuf {
    let root = fixture.base.join("callers");
    let filler: String = (0..24)
        .map(|line| format!("    let step_{line} = id + {line};\n"))
        .collect();
    for file in 0..files {
        let mut text = String::new();
        for function in 0..4 {
            let n = file * 4 + function;
            let call = match function {
                0 | 1 => {
                    "    fetch_widget(id);\n    let again = fetch_widget(id + 1);\n".to_string()
                }
                2 => format!("    fetch(widget_{n}); // FETCH_WIDGET in capitals\n"),
                _ => "    let widgets = fetch_widgets(id);\n".to_string(),
            };
            text.push_str(&format!(
                "/// Function {n}.\npub fn function_{n}(id: u32) {{\n{filler}{call}}}\n\n"
            ));
        }
        write(&root.join(format!("src/part_{file:02}.rs")), &text);
    }
    write(
        &root.join("src/widget.rs"),
        "/// Fetches a widget.\npub fn fetch_widget(id: u32) -> u32 {\n    id\n}\n",
    );
    write(
        &root.join("README.md"),
        "# Callers\n\nUse `fetch_widget` to fetch one.\n",
    );
    root
}

/// Places in files, each (file, line).
type Places = Vec<(String, usize)>;

/// The `.rs` and `.md` files of `root` and its `src`, in path order.
fn project_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in [root.to_path_buf(), root.join("src")] {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path
                .extension()
                .is_some_and(|ext| ext == "rs" || ext == "md")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// What rg would find of `needle` in `root`'s `.rs` and `.md` files, as the
/// index chunks them, meaning each chunk (file, start line) that holds it,
/// and each line (file, line) where it starts.
fn oracle(root: &Path, needle: &str) -> (Places, Places) {
    let (mut chunks, mut lines) = (Vec::new(), Vec::new());
    for file in project_files(root) {
        let shown = file
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let text = std::fs::read_to_string(&file).unwrap();
        // (The index leaves out a file with a NUL as binary.)
        if text.contains('\0') {
            continue;
        }
        let kind = if shown.ends_with(".md") {
            Kind::Docs
        } else {
            Kind::Code
        };
        for chunk in
            arko_index::bench_v1::chunk_file(&file, &super::engine::chunk_options(kind)).unwrap()
        {
            if text[chunk.start_byte..chunk.end_byte].contains(needle) {
                chunks.push((shown.clone(), chunk.start_line));
            }
        }
        for (at, line) in text.lines().enumerate() {
            if line.contains(needle) {
                lines.push((shown.clone(), at + 1));
            }
        }
    }
    (chunks, lines)
}

/// Each result's (file, start line), and each line it shows (file, line).
fn found(output: &SearchOutput) -> (Places, Places) {
    let mut chunks = Vec::new();
    let mut lines = Vec::new();
    for hit in &output.hits {
        chunks.push((hit.display.clone(), hit.start_line));
        let held = hit.lines.as_ref().expect("an exact match has its lines");
        assert_eq!(held.more, 0, "{hit:?}");
        assert!(!held.shown.is_empty(), "{hit:?}");
        for (line, _) in &held.shown {
            assert!(hit.start_line <= *line && *line <= hit.end_line, "{hit:?}");
            lines.push((hit.display.clone(), *line));
        }
    }
    (chunks, lines)
}

#[test]
fn an_exact_search_counts_every_chunk_that_holds_the_query_as_written() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 6);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let (chunks, lines) = oracle(&root, "fetch_widget");
    // Calls (two a function), `fetch_widgets` (which holds it), the
    // definition and the README.
    assert!(chunks.len() > 10, "{chunks:?}");
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.limit = Some(50);
    let output = indexes.search_blocking(&args).unwrap();
    assert_eq!(output.total, Some(chunks.len()), "{output:?}");
    assert_eq!(output.searched[0].2, Mode::Lexical);
    let (mut got, mut got_lines) = found(&output);
    got.sort();
    got_lines.sort();
    let (mut want, mut want_lines) = (chunks.clone(), lines.clone());
    want.sort();
    want_lines.sort();
    assert_eq!(got, want);
    assert_eq!(got_lines, want_lines);
    // In path and line order.
    let order: Vec<_> = output
        .hits
        .iter()
        .map(|hit| (hit.display.clone(), hit.start_line))
        .collect();
    assert_eq!(order, want);
    // `fetch(widget_)` holds its words, and isn't a match. Neither is the
    // name in capitals, unless the case doesn't matter.
    assert!(
        output
            .hits
            .iter()
            .flat_map(|hit| &hit.lines.as_ref().unwrap().shown)
            .all(|(_, line)| line.contains("fetch_widget")),
        "{output:?}"
    );
    args.ignore_case = true;
    let any_case = indexes.search_blocking(&args).unwrap();
    assert_eq!(any_case.total, Some(oracle_any_case(&root).len()));
    assert!(any_case.total > output.total);
    // A cut list still counts them all.
    args.ignore_case = false;
    args.limit = Some(3);
    let cut = indexes.search_blocking(&args).unwrap();
    assert_eq!(cut.hits.len(), 3);
    assert_eq!(cut.total, Some(chunks.len()));
    // A query nothing holds as written.
    let none = indexes
        .search_blocking(&search_args(&root, "widget fetch", Some(ModeArg::Lexical)))
        .unwrap();
    assert!(none.hits.is_empty(), "{none:?}");
    assert_eq!(none.total, Some(0));
    // Ranked modes count nothing.
    let ranked = indexes
        .search_blocking(&search_args(&root, "fetch_widget", Some(ModeArg::Hybrid)))
        .unwrap();
    assert_eq!(ranked.total, None);
    assert!(ranked.hits.iter().all(|hit| hit.lines.is_none()));
    fixture.shutdown(&indexes);
}

/// The chunks of the callers project that hold `fetch_widget` in any case.
fn oracle_any_case(root: &Path) -> Places {
    let (mut chunks, _) = oracle(root, "fetch_widget");
    chunks.extend(oracle(root, "FETCH_WIDGET").0);
    chunks.sort();
    chunks.dedup();
    chunks
}

#[test]
fn an_exact_search_filters_by_path_and_language() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 3);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let (chunks, _) = oracle(&root, "fetch_widget");
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.limit = Some(50);
    args.path_prefix = Some("src/part_01.rs".into());
    let output = indexes.search_blocking(&args).unwrap();
    let want = chunks
        .iter()
        .filter(|(file, _)| file == "src/part_01.rs")
        .count();
    assert_eq!(output.total, Some(want));
    assert!(
        output
            .hits
            .iter()
            .all(|hit| hit.display == "src/part_01.rs")
    );
    args.path_prefix = None;
    args.language = Some("markdown".into());
    let output = indexes.search_blocking(&args).unwrap();
    assert_eq!(hit_files(&output), ["README.md"], "{output:?}");
    assert_eq!(output.total, Some(1));
    fixture.shutdown(&indexes);
}

#[test]
fn an_exact_search_reads_files_as_they_are_now() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 2);
    // Searches don't bring the index up to date in the test's time.
    let timing = Timing {
        reconcile_interval: Duration::from_secs(3600),
        ..fast(false)
    };
    let indexes = fixture.open(&root, Backend::Hash, false, timing);
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.limit = Some(50);
    let before = indexes.search_blocking(&args).unwrap();
    // Lines go in above the calls, and one more call comes in below them.
    let file = root.join("src/part_00.rs");
    let text = std::fs::read_to_string(&file).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    write(
        &file,
        &format!(
            "//! Moved down.\n//! Further.\n\n{text}pub fn added() {{\n    fetch_widget(9);\n}}\n"
        ),
    );
    // A file gone since holds nothing, and neither does one that became binary.
    std::fs::remove_file(root.join("src/widget.rs")).unwrap();
    write(
        &root.join("src/part_01.rs"),
        "pub fn function_4() {\n    fetch_widget(1);\n}\n\0\n",
    );
    let after = indexes.search_blocking(&args).unwrap();
    assert!(
        after.notes.iter().all(|note| !note.contains("rebuilt")),
        "{:?}",
        after.notes
    );
    let (mut want, mut want_lines) = oracle(&root, "fetch_widget");
    want.sort();
    want_lines.sort();
    let (mut got, mut got_lines) = found(&after);
    got.sort();
    got_lines.sort();
    assert_eq!(got, want);
    assert_eq!(got_lines, want_lines);
    assert_eq!(after.total, Some(want.len()));
    let moved = |output: &SearchOutput| {
        found(output)
            .1
            .into_iter()
            .filter(|(file, _)| file == "src/part_00.rs")
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
    };
    let (was, now) = (moved(&before), moved(&after));
    assert_eq!(now.len(), was.len() + 1, "{now:?}");
    assert!(was.iter().zip(&now).all(|(was, now)| now == &(was + 3)));
    assert!(hit_files(&before).contains(&"src/widget.rs".to_string()));
    assert!(!hit_files(&after).contains(&"src/widget.rs".to_string()));
    assert!(hit_files(&before).contains(&"src/part_01.rs".to_string()));
    assert!(!hit_files(&after).contains(&"src/part_01.rs".to_string()));
    // The changed file's results say so, and only theirs.
    for (output, changed) in [(&before, false), (&after, true)] {
        for hit in &output.hits {
            let expected = changed && hit.display == "src/part_00.rs";
            assert_eq!(hit.changed, expected, "{hit:?}");
        }
    }
    fixture.shutdown(&indexes);
}

#[test]
fn an_exact_search_of_several_indexes_counts_them_all() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 2);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes
        .create_blocking(&create_args(&root, Some(Kind::Code)))
        .unwrap();
    indexes
        .create_blocking(&create_args(&root, Some(Kind::Docs)))
        .unwrap();
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.limit = Some(50);
    let both = indexes.search_blocking(&args).unwrap();
    assert_eq!(both.searched.len(), 2);
    let mut each = 0;
    for name in ["callers", "callers-docs"] {
        args.index = Some(name.into());
        let one = indexes.search_blocking(&args).unwrap();
        assert!(one.total.unwrap() > 0, "{name}: {one:?}");
        each += one.total.unwrap();
    }
    assert_eq!(both.total, Some(each));
    // A cut list is the start of the one list in path order, whichever
    // index each result is from.
    args.index = None;
    args.limit = Some(2);
    let cut = indexes.search_blocking(&args).unwrap();
    let key = |output: &SearchOutput| {
        output
            .hits
            .iter()
            .map(|hit| (hit.index.clone(), hit.display.clone(), hit.start_line))
            .collect::<Vec<_>>()
    };
    assert_eq!(key(&cut), key(&both)[..2]);
    fixture.shutdown(&indexes);
}

#[test]
fn exact_matches_are_grouped_by_file_and_read_back() {
    use super::lexical::Lines;
    let hit = |index: &str, display: &str, lines: (usize, usize), scope: &str| Hit {
        index: index.into(),
        display: display.into(),
        file: PathBuf::from("/w").join(display),
        start_line: lines.0,
        end_line: lines.1,
        start_byte: 0,
        end_byte: 0,
        column: 1,
        scope: scope.into(),
        symbol: String::new(),
        changed: false,
        lines: None,
    };
    let with = |mut hit: Hit, shown: &[(usize, &str)], more: usize| {
        hit.lines = Some(Lines {
            shown: shown
                .iter()
                .map(|(number, line)| (*number, line.to_string()))
                .collect(),
            more,
        });
        hit
    };
    let output = SearchOutput {
        hits: vec![
            with(
                hit("proj", "src/a.rs", (10, 20), "fn one"),
                &[(12, "go();")],
                0,
            ),
            with(
                hit("proj", "src/a.rs", (30, 40), "fn two"),
                &[(31, "go()")],
                2,
            ),
            with(
                hit("proj", "src/b.rs", (1, 5), ""),
                &[(2, "go();"), (4, "go(); go();")],
                0,
            ),
        ],
        total: Some(1_340),
        searched: vec![("proj".into(), Kind::Code, Mode::Lexical)],
        notes: vec!["a note".into()],
        ..SearchOutput::default()
    };
    let text = tools::format_search_detail("go()", &output, tools::Detail::Lines);
    assert_eq!(
        text,
        [
            "3 of 1,340 results from proj (code, lexical):",
            "",
            "src/a.rs",
            "  10-20 · fn one",
            "    12 | go();",
            "  30-40 · fn two",
            "    31 | go()",
            "    and 2 more lines",
            "src/b.rs",
            "  1-5",
            "    2 | go();",
            "    4 | go(); go();",
            "note: a note",
        ]
        .join("\n")
    );
    let proj = vec!["proj".to_string()];
    assert_eq!(
        tools::read_search_header(text.lines().next().unwrap()),
        Some((proj.clone(), 3))
    );
    assert_eq!(
        tools::read_search_hits(&text).unwrap(),
        [
            "3 of 1,340 results from proj (code, lexical)",
            "src/a.rs:10-20 · fn one",
            "src/a.rs:30-40 · fn two",
            "src/b.rs:1-5",
        ]
    );
    // A list that isn't cut gives one number, with its thousands apart.
    let whole = SearchOutput {
        total: Some(3),
        ..output.clone()
    };
    let text = tools::format_search_detail("go()", &whole, tools::Detail::Lines);
    assert!(
        text.starts_with("3 results from proj (code, lexical):\n"),
        "{text}"
    );
    assert_eq!(
        tools::read_search_header("1,240 results from proj (code, lexical):"),
        Some((proj.clone(), 1_240))
    );
    assert_eq!(
        tools::read_search_header("1 of 2 results from proj (code, lexical):"),
        Some((proj, 1))
    );
    // Several indexes name theirs, and the same file in two is two groups.
    let several = SearchOutput {
        hits: vec![
            with(hit("proj", "src/a.rs", (1, 2), "fn x"), &[(1, "go()")], 0),
            with(hit("docs", "src/a.rs", (1, 2), ""), &[(1, "go()")], 0),
        ],
        total: Some(2),
        searched: vec![
            ("proj".into(), Kind::Code, Mode::Lexical),
            ("docs".into(), Kind::Docs, Mode::Lexical),
        ],
        ..SearchOutput::default()
    };
    let text = tools::format_search_detail("go()", &several, tools::Detail::Lines);
    assert_eq!(
        tools::read_search_hits(&text).unwrap(),
        [
            "2 results from proj (code, lexical), docs (docs, lexical)",
            "[proj] src/a.rs:1-2 · fn x",
            "[docs] src/a.rs:1-2",
        ]
    );
    // The results keep the order they come in, so a file that two indexes
    // share comes back in groups where the order takes them.
    let interleaved = SearchOutput {
        hits: vec![
            with(hit("proj", "src/a.rs", (1, 2), "fn x"), &[(1, "go()")], 0),
            with(hit("docs", "src/a.rs", (1, 2), ""), &[(1, "go()")], 0),
            with(hit("proj", "src/a.rs", (5, 6), "fn y"), &[(5, "go()")], 0),
            with(hit("docs", "src/a.rs", (5, 6), ""), &[(5, "go()")], 0),
            with(hit("proj", "src/b.rs", (1, 2), ""), &[(1, "go()")], 0),
        ],
        total: Some(5),
        ..several.clone()
    };
    let text = tools::format_search_detail("go()", &interleaved, tools::Detail::Lines);
    assert_eq!(
        tools::read_search_hits(&text).unwrap()[1..],
        [
            "[proj] src/a.rs:1-2 · fn x",
            "[docs] src/a.rs:1-2",
            "[proj] src/a.rs:5-6 · fn y",
            "[docs] src/a.rs:5-6",
            "[proj] src/b.rs:1-2",
        ]
    );
    // Whole results still read back as before.
    let full = tools::format_search_detail("go()", &output, tools::Detail::Brief(1));
    assert!(full.starts_with("3 of 1,340 results"), "{full}");
    assert_eq!(tools::read_search_hits(&full).unwrap().len(), 4);
}

/// The chunk count read before a search may be of an older, smaller
/// generation. The candidates still come back whole, so the total and an
/// exhaustive list hold the chunks past the count too.
#[test]
fn a_stale_chunk_count_cuts_no_candidates() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 4);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    let outcome = indexes.create_blocking(&create_args(&root, None)).unwrap();
    let state = outcome.layout.state();
    let docs = arko_index::index2::status(&state).unwrap().docs;
    let filter = arko_index::vector::Filter::default();
    let all = super::engine::every_candidate(&state, "fetch_widget", &filter, docs).unwrap();
    assert!(all.len() > 8, "{}", all.len());
    let ids = |hits: &[arko_index::hybrid::SearchHit]| {
        let mut ids: Vec<String> = hits.iter().map(|hit| hit.chunk.id.clone()).collect();
        ids.sort();
        ids
    };
    // Counts far below it, and one that leaves the first list exactly full.
    for stale in [0, 1, 2, all.len() as u64 - 1] {
        let found = super::engine::every_candidate(&state, "fetch_widget", &filter, stale).unwrap();
        assert_eq!(ids(&found), ids(&all), "{stale}");
    }
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.limit = Some(50);
    let output = indexes.search_blocking(&args).unwrap();
    assert_eq!(output.total, Some(oracle(&root, "fetch_widget").0.len()));
    fixture.shutdown(&indexes);
}

#[test]
fn limits_are_set_per_mode() {
    use super::engine::{DEFAULT_LIMITS, Limits, MAX_LIMIT};
    assert_eq!(
        DEFAULT_LIMITS,
        Limits {
            semantic: 10,
            hybrid: 10,
            lexical: 100
        }
    );
    assert_eq!(DEFAULT_LIMITS.of(Some(ModeArg::Lexical)), 100);
    assert_eq!(DEFAULT_LIMITS.of(Some(ModeArg::Semantic)), 10);
    assert_eq!(DEFAULT_LIMITS.of(None), 10);
    let set = DEFAULT_LIMITS
        .with(" lexical=250, semantic = 5,, hybrid=50 ")
        .unwrap();
    assert_eq!(
        set,
        Limits {
            semantic: 5,
            hybrid: 50,
            lexical: 250
        }
    );
    // The modes it leaves out keep theirs.
    assert_eq!(
        DEFAULT_LIMITS.with("bm25=7").unwrap(),
        Limits {
            lexical: 7,
            ..DEFAULT_LIMITS
        }
    );
    assert_eq!(DEFAULT_LIMITS.with("").unwrap(), DEFAULT_LIMITS);
    assert_eq!(
        DEFAULT_LIMITS.with("lexical=100000").unwrap().lexical,
        100_000
    );
    for bad in [
        "lexical",
        "fuzzy=3",
        "=3",
        "semantic=0",
        "hybrid=-1",
        "hybrid=lots",
        &format!("semantic={}", MAX_LIMIT + 1),
    ] {
        assert!(DEFAULT_LIMITS.with(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_lexical_search_lists_100_by_default_and_every_match_when_exhaustive() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 45);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let (mut chunks, mut lines) = oracle(&root, "fetch_widget");
    chunks.sort();
    lines.sort();
    assert!(chunks.len() > 100, "{}", chunks.len());
    let all = chunks.len();

    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.limit = None;
    let output = indexes.search_blocking(&args).unwrap();
    assert_eq!(output.hits.len(), 100);
    assert_eq!(output.total, Some(all));
    let (file, line, column) = oracle_starts(&root, "fetch_widget")[99].clone();
    assert!(output.notes.is_empty(), "{:?}", output.notes);
    assert_eq!(
        next_note(&output),
        Some(format!(
            "exhaustive=true lists all {all}, and path_prefix narrows the search. For the next {}, use after=\"{file}:{line}:{column}\"",
            all - 100
        ))
    );
    // Every match, with every line it starts on.
    args.exhaustive = true;
    let output = indexes.search_blocking(&args).unwrap();
    assert_eq!(output.total, Some(all));
    let (got, mut got_lines) = found(&output);
    got_lines.sort();
    assert_eq!(got, chunks);
    assert_eq!(got_lines, lines);
    assert!(output.notes.is_empty(), "{:?}", output.notes);
    // Exhaustive goes past any limit, and a limit may be past 50.
    args.limit = Some(1);
    assert_eq!(indexes.search_blocking(&args).unwrap().hits.len(), all);
    args.exhaustive = false;
    args.limit = Some(all - 1);
    assert_eq!(indexes.search_blocking(&args).unwrap().hits.len(), all - 1);

    // Semantic and hybrid keep 10 by default, and at most 50.
    for mode in [ModeArg::Semantic, ModeArg::Hybrid] {
        let mut args = search_args(&root, "fetch a widget by its id", Some(mode));
        args.limit = None;
        let output = indexes.search_blocking(&args).unwrap();
        assert_eq!(output.hits.len(), 10, "{mode:?}");
        assert_eq!(output.total, None);
        args.limit = Some(500);
        assert_eq!(
            indexes.search_blocking(&args).unwrap().hits.len(),
            50,
            "{mode:?}"
        );
        args.limit = Some(3);
        assert_eq!(indexes.search_blocking(&args).unwrap().hits.len(), 3);
        // `exhaustive` and `ignore_case` are accepted in ranked modes without erroring.
        args.exhaustive = true;
        args.ignore_case = false;
        assert_eq!(
            indexes.search_blocking(&args).unwrap().hits.len(),
            50,
            "{mode:?}"
        );
        args.exhaustive = false;
        args.ignore_case = true;
        assert_eq!(
            indexes.search_blocking(&args).unwrap().hits.len(),
            3,
            "{mode:?}"
        );
    }

    // The agent's tool says the same.
    let tool = indexes
        .tools()
        .into_iter()
        .find(|tool| tool.definition().name == "index_search")
        .unwrap();
    let context = TaskContext::new(uuid::Uuid::new_v4(), "test", "", &root);
    let run = |value: Value| {
        fixture
            .runtime
            .block_on(tool.execute(&context, params(value)))
            .unwrap()
    };
    let text = run(json!({"query": "fetch_widget", "mode": "lexical"}));
    let first = text.lines().next().unwrap();
    assert_eq!(
        first,
        format!("100 of {all} results from callers (code, lexical):")
    );
    assert!(text.contains("\nsrc/part_00.rs\n  "), "{text}");
    assert!(text.contains(" | fetch_widget(id);\n"), "{text}");
    assert!(text.ends_with(&format!(
        "note: exhaustive=true lists all {all}, and path_prefix narrows the search. For the next {}, use after=\"{file}:{line}:{column}\"",
        all - 100
    )));
    let text = run(json!({"query": "fetch_widget", "mode": "lexical", "exhaustive": true}));
    assert!(
        text.starts_with(&format!("{all} results from callers (code, lexical):")),
        "{text}"
    );
    assert_eq!(tools::read_search_hits(&text).unwrap().len(), all + 1);
    // detail=full shows whole definitions instead.
    let text =
        run(json!({"query": "fetch_widget", "mode": "lexical", "detail": "full", "limit": 2}));
    assert!(text.contains("\n3 | "), "{text}");
    fixture.shutdown(&indexes);
}

/// `arko index search` names its own options where the tool's notes name
/// parameters.
#[test]
fn the_cli_names_its_options_in_the_notes() {
    assert_eq!(
        cli_text("note: exhaustive=true lists all 340, and path_prefix narrows the search"),
        "note: --exhaustive lists all 340, and --path-prefix narrows the search"
    );
    assert_eq!(
        cli_text(
            "note: lexical finds the chunks that hold the query as written (ignore_case=true matches its ASCII letters in any case). hybrid finds its words in any order"
        ),
        "note: lexical finds the chunks that hold the query as written (--ignore-case matches its ASCII letters in any case). --mode hybrid finds its words in any order"
    );
}

/// Only a search's notes name the command's options. A matched line stays
/// as it is in the file.
#[test]
fn the_cli_changes_no_matched_line() {
    let text = [
        "1 of 2 results from proj (code, lexical):",
        "",
        "src/a.rs",
        "  1-3 · fn a",
        r#"    2 | let hint = "exhaustive=true lists all, and path_prefix narrows; hybrid finds; use after="a:1:1"";"#,
        "note: exhaustive=true lists all 2, and path_prefix narrows the search",
    ]
    .join("\n");
    let shown = cli_search_text(&text);
    let lines: Vec<&str> = shown.lines().collect();
    assert_eq!(lines[..5], text.lines().collect::<Vec<_>>()[..5]);
    assert_eq!(
        lines[5],
        "note: --exhaustive lists all 2, and --path-prefix narrows the search"
    );
}

/// A file the search can't read is named, and left out of the count, rather
/// than taken for one that holds nothing.
#[cfg(unix)]
#[test]
fn an_exact_search_names_the_files_it_couldnt_read() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 2);
    // The index isn't brought up to date in the test's time, so it still has
    // the file.
    let timing = Timing {
        reconcile_interval: Duration::from_secs(3600),
        ..fast(false)
    };
    let indexes = fixture.open(&root, Backend::Hash, false, timing);
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.limit = Some(50);
    let all = indexes.search_blocking(&args).unwrap().total.unwrap();
    let file = root.join("src/part_01.rs");
    let in_it = oracle(&root, "fetch_widget")
        .0
        .iter()
        .filter(|(shown, _)| shown == "src/part_01.rs")
        .count();
    assert!(in_it > 0);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    // (Someone who reads every file, such as root, has nothing to test.)
    if std::fs::read(&file).is_err() {
        let output = indexes.search_blocking(&args).unwrap();
        assert_eq!(output.total, Some(all - in_it));
        let note = output
            .notes
            .iter()
            .find(|note| {
                note.starts_with("The results and their count leave out 1 file of callers")
            })
            .unwrap_or_else(|| panic!("{:?}", output.notes));
        assert!(note.contains("src/part_01.rs ("), "{note}");
    }
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    fixture.shutdown(&indexes);
}

// ----- pages of an exact search -----

/// The places where results start, each (file, line, byte column).
type Starts = Vec<(String, usize, usize)>;

/// Where each chunk of `root`'s `.rs` and `.md` files that holds `needle`
/// starts, read from the files themselves, in path order.
fn oracle_starts(root: &Path, needle: &str) -> Starts {
    let mut starts = Vec::new();
    for file in project_files(root) {
        let shown = file
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let text = std::fs::read_to_string(&file).unwrap();
        if text.contains('\0') {
            continue;
        }
        let kind = if shown.ends_with(".md") {
            Kind::Docs
        } else {
            Kind::Code
        };
        for chunk in
            arko_index::bench_v1::chunk_file(&file, &super::engine::chunk_options(kind)).unwrap()
        {
            if text[chunk.start_byte..chunk.end_byte].contains(needle) {
                let line_start = text[..chunk.start_byte].rfind('\n').map_or(0, |at| at + 1);
                starts.push((
                    shown.clone(),
                    chunk.start_line,
                    chunk.start_byte - line_start + 1,
                ));
            }
        }
    }
    starts
}

/// Where each result of `output` starts.
fn starts(output: &SearchOutput) -> Starts {
    output
        .hits
        .iter()
        .map(|hit| (hit.display.clone(), hit.start_line, hit.column))
        .collect()
}

/// A place as a cursor writes it.
fn cursor_at((file, line, column): &(String, usize, usize)) -> String {
    format!("{file}:{line}:{column}")
}

/// The cursor of the page after `output`'s.
fn next_cursor(output: &SearchOutput) -> Option<String> {
    output.next.as_ref().map(|next| next.cursor.to_string())
}

/// The note of a cut list as the tool writes it.
fn next_note(output: &SearchOutput) -> Option<String> {
    let text = tools::format_search_detail("fetch_widget", output, tools::Detail::Lines);
    let note = text.lines().last()?.strip_prefix("note: ")?;
    note.contains(", use after=\"").then(|| note.to_string())
}

/// `args` searched with `after`.
fn search_after(indexes: &Arc<Indexes>, args: &SearchArgs, after: &str) -> SearchOutput {
    let args = SearchArgs {
        after: Some(after.to_string()),
        ..args.clone()
    };
    indexes.search_blocking(&args).unwrap()
}

/// Every page of the search, each from the cursor that ends the note of the
/// page before.
fn all_pages(indexes: &Arc<Indexes>, args: &SearchArgs) -> Vec<SearchOutput> {
    let mut args = args.clone();
    let mut pages = Vec::new();
    loop {
        let page = indexes.search_blocking(&args).unwrap();
        let next = next_cursor(&page);
        pages.push(page);
        match next {
            Some(cursor) => args.after = Some(cursor),
            None => return pages,
        }
        assert!(pages.len() < 2_000, "the pages don't end");
    }
}

#[test]
fn the_pages_of_an_exact_search_join_up_to_the_whole_list() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 45);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let want = oracle_starts(&root, "fetch_widget");
    let all = want.len();
    assert!(all > 100, "{all}");
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.exhaustive = true;
    let whole = indexes.search_blocking(&args).unwrap();
    assert_eq!(starts(&whole), want);
    assert!(next_cursor(&whole).is_none() && whole.before == 0 && whole.after.is_none());
    args.exhaustive = false;
    for limit in [
        None,
        Some(7),
        Some(100),
        Some(all - 1),
        Some(all),
        Some(all + 1),
    ] {
        args.limit = limit;
        let size = limit.unwrap_or(100);
        let pages = all_pages(&indexes, &args);
        assert_eq!(pages.len(), all.div_ceil(size), "{limit:?}");
        let (mut seen, mut cursor) = (0, None);
        for page in &pages {
            let count = size.min(all - seen);
            // Each page is the next of the whole list, so together they have
            // no gap and no repeat.
            assert_eq!(starts(page), want[seen..seen + count], "{limit:?}: {seen}");
            assert_eq!(page.total, Some(all));
            assert_eq!(page.before, seen);
            assert_eq!(page.after, cursor);
            seen += count;
            // The note of a cut page gives the place of its last result.
            cursor = (seen < all).then(|| cursor_at(&want[seen - 1]));
            assert_eq!(next_cursor(page), cursor, "{limit:?}: {seen}");
        }
        assert_eq!(seen, all);
        // A page counts the one after it as it comes.
        for (page, following) in pages.iter().zip(&pages[1..]) {
            let next = page.next.as_ref().unwrap();
            assert_eq!(next.count, following.hits.len(), "{limit:?}");
        }
        assert!(pages.last().unwrap().next.is_none());
    }
    fixture.shutdown(&indexes);
}

#[test]
fn a_cut_list_ends_its_note_with_the_cursor_of_the_next_page() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 5);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let want = oracle_starts(&root, "fetch_widget");
    // (Three in each of five files, the definition and the README.)
    assert_eq!(want.len(), 17);
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.limit = Some(5);
    let note = |left: usize, next: usize, last: usize| {
        format!(
            "exhaustive=true lists all {left}, and path_prefix narrows the search. For the next {next}, use after=\"{}\"",
            cursor_at(&want[last])
        )
    };
    let page = indexes.search_blocking(&args).unwrap();
    assert_eq!(next_note(&page), Some(note(17, 5, 4)));
    assert_eq!((page.before, page.after), (0, None));
    // The note counts what the call lists with exhaustive=true, which is all
    // after the cursor it was given.
    let page = search_after(&indexes, &args, &cursor_at(&want[4]));
    assert_eq!(next_note(&page), Some(note(12, 5, 9)));
    let page = search_after(&indexes, &args, &cursor_at(&want[9]));
    assert_eq!(next_note(&page), Some(note(7, 2, 14)));
    assert_eq!(page.after, Some(cursor_at(&want[9])));
    // The last page has no more to say, whatever it holds.
    let page = search_after(&indexes, &args, &cursor_at(&want[14]));
    assert_eq!(starts(&page), want[15..]);
    assert!(page.next.is_none() && next_note(&page).is_none());
    // The next page is no more than what is left.
    args.limit = Some(10);
    let page = indexes.search_blocking(&args).unwrap();
    assert_eq!(next_note(&page), Some(note(17, 7, 9)));
    // Everything, or a page that fits the rest, makes no cut.
    args.limit = Some(12);
    let page = search_after(&indexes, &args, &cursor_at(&want[4]));
    assert_eq!(starts(&page), want[5..]);
    assert!(page.next.is_none() && next_note(&page).is_none());
    // With exhaustive=true, after lists all that follows it.
    args.exhaustive = true;
    args.limit = Some(1);
    let page = search_after(&indexes, &args, &cursor_at(&want[4]));
    assert_eq!(starts(&page), want[5..]);
    assert_eq!((page.total, page.before), (Some(17), 5));
    assert!(page.next.is_none() && next_note(&page).is_none());
    fixture.shutdown(&indexes);
}

#[test]
fn a_cursor_may_be_copied_with_its_quotes_or_cut_to_a_line() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 6);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let want = oracle_starts(&root, "fetch_widget");
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.exhaustive = true;
    let (file, line, column) = want[7].clone();
    for after in [
        format!("{file}:{line}:{column}"),
        format!("\"{file}:{line}:{column}\""),
        format!("after=\"{file}:{line}:{column}\""),
        format!("  {file}:{line}:{column} "),
        // Every result that starts on the line is before it.
        format!("{file}:{line}"),
    ] {
        let page = search_after(&indexes, &args, &after);
        assert_eq!(starts(&page), want[8..], "{after}");
        assert_eq!(page.before, 8, "{after}");
    }
    // The line above a result, at any column, leaves it in.
    let (file, line, _) = want[8].clone();
    assert!(line > 1, "{line}");
    let page = search_after(&indexes, &args, &format!("{file}:{}:9999", line - 1));
    assert_eq!(starts(&page), want[8..]);
    // Before every result, past the last, and in a file that sorts after all.
    let page = search_after(&indexes, &args, "A:1:1");
    assert_eq!((starts(&page), page.before), (want.clone(), 0));
    let last = cursor_at(want.last().unwrap());
    for after in [last.as_str(), "zz.rs:1", "src/widget.rs:9999"] {
        let page = search_after(&indexes, &args, after);
        assert!(page.hits.is_empty(), "{after}");
        assert_eq!((page.total, page.before), (Some(want.len()), want.len()));
        assert!(page.notes.is_empty(), "{:?}", page.notes);
    }
    // A blank one is none.
    let page = search_after(&indexes, &args, "  ");
    assert_eq!((starts(&page), page.before, page.after), (want, 0, None));
    fixture.shutdown(&indexes);
}

#[test]
fn results_that_start_on_one_line_page_by_their_columns() {
    let fixture = Fixture::new();
    let root = fixture.base.join("long");
    // A line of one call after another, which the chunker splits into many
    // chunks that start on that line.
    let calls = "fetch_widget(1), ".repeat(16_000);
    write(
        &root.join("src/a.rs"),
        "pub fn first() {\n    fetch_widget(0);\n}\n",
    );
    write(
        &root.join("src/long.rs"),
        &format!("let calls = [{calls}];\n"),
    );
    write(
        &root.join("src/z.rs"),
        "pub fn last() {\n    fetch_widget(2);\n}\n",
    );
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.exhaustive = true;
    let whole = indexes.search_blocking(&args).unwrap();
    let on_line: Vec<&Hit> = whole
        .hits
        .iter()
        .filter(|hit| hit.display == "src/long.rs")
        .collect();
    assert!(on_line.len() > 20, "{}", on_line.len());
    for hit in &on_line {
        assert_eq!(hit.start_line, 1);
        // The line is the file's first, so the column is the byte it starts.
        assert_eq!(hit.column, hit.start_byte + 1, "{hit:?}");
    }
    assert!(on_line.windows(2).all(|two| two[0].column < two[1].column));
    assert_eq!(whole.hits.len(), on_line.len() + 2);
    args.exhaustive = false;
    for limit in [1, 3, 4, 10] {
        args.limit = Some(limit);
        let pages = all_pages(&indexes, &args);
        let joined: Starts = pages.iter().flat_map(starts).collect();
        assert_eq!(joined, starts(&whole), "limit {limit}");
        assert_eq!(
            pages.len(),
            whole.hits.len().div_ceil(limit),
            "limit {limit}"
        );
        // A page that ends in the long line has a column of it for a cursor.
        for page in &pages[..pages.len() - 1] {
            assert_eq!(
                next_cursor(page),
                Some(cursor_at(&starts(page).pop().unwrap()))
            );
        }
    }
    fixture.shutdown(&indexes);
}

#[test]
fn a_page_starts_after_the_cursor_however_the_files_changed() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 8);
    // Searches don't bring the index up to date in the test's time.
    let timing = Timing {
        reconcile_interval: Duration::from_secs(3600),
        ..fast(false)
    };
    let indexes = fixture.open(&root, Backend::Hash, false, timing);
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.limit = Some(10);
    let first = indexes.search_blocking(&args).unwrap();
    let cursor = next_cursor(&first).unwrap();
    let last = starts(&first).pop().unwrap();
    assert_eq!(cursor, cursor_at(&last));
    let file = last.0.clone();
    assert_eq!(file, "src/part_02.rs", "the cursor is in the third file");

    std::thread::sleep(Duration::from_millis(20));
    let append = |name: &str, text: &str| {
        let path = root.join(name);
        let old = std::fs::read_to_string(&path).unwrap();
        write(&path, &format!("{old}{text}"));
    };
    // New results below the cursor, in its file and in a file that was
    // listed, and a new result in an earlier file, which the cursor is past.
    append(&file, "pub fn added() {\n    fetch_widget(9);\n}\n");
    append(
        "src/part_00.rs",
        "pub fn also_added() {\n    fetch_widget(8);\n}\n",
    );
    // A later file gone, and one that became binary.
    std::fs::remove_file(root.join("src/part_04.rs")).unwrap();
    write(&root.join("src/part_05.rs"), "fetch_widget(1);\n\0\n");
    let now = oracle_starts(&root, "fetch_widget");
    let after: Starts = now.iter().filter(|start| **start > last).cloned().collect();
    assert!(after.iter().any(|(name, _, _)| *name == file), "{after:?}");
    args.exhaustive = true;
    let page = search_after(&indexes, &args, &cursor);
    assert_eq!(starts(&page), after);
    assert_eq!(page.total, Some(now.len()));
    assert_eq!(page.before, now.len() - after.len());
    // Pages of the list as it is now go on after the cursor too.
    args.exhaustive = false;
    args.after = Some(cursor.clone());
    let pages = all_pages(&indexes, &args);
    let joined: Starts = pages.iter().flat_map(starts).collect();
    assert_eq!(joined, after);

    // Lines come in above the cursor's place, so its file's results are further
    // down, and the page still starts after the place of the cursor, in the
    // file as it is. (A result that moved past the cursor shows again.)
    let top = std::fs::read_to_string(root.join(&file)).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    write(&root.join(&file), &format!("//! One.\n//! Two.\n\n{top}"));
    let now = oracle_starts(&root, "fetch_widget");
    let after: Starts = now.iter().filter(|start| **start > last).cloned().collect();
    args.exhaustive = true;
    args.after = None;
    let page = search_after(&indexes, &args, &cursor);
    assert_eq!(starts(&page), after);
    // The result the cursor was made of now starts three lines further down.
    assert!(
        after.contains(&(file.clone(), last.1 + 3, last.2)),
        "{after:?}"
    );
    fixture.shutdown(&indexes);
}

#[test]
fn pages_keep_to_the_path_and_language_filters() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 20);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let want = oracle_starts(&root, "fetch_widget");
    let source: Starts = want
        .iter()
        .filter(|(file, _, _)| file.starts_with("src/"))
        .cloned()
        .collect();
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    let joined = |pages: &[SearchOutput]| -> Starts { pages.iter().flat_map(starts).collect() };

    // A directory.
    args.path_prefix = Some("src".into());
    args.limit = Some(9);
    let pages = all_pages(&indexes, &args);
    assert_eq!(joined(&pages), source);
    assert!(pages.iter().all(|page| page.total == Some(source.len())));
    assert_eq!(pages.len(), source.len().div_ceil(9));
    // A file, a result at a time.
    args.path_prefix = Some("src/part_03.rs".into());
    args.limit = Some(1);
    let pages = all_pages(&indexes, &args);
    let in_file: Starts = want
        .iter()
        .filter(|(file, _, _)| file == "src/part_03.rs")
        .cloned()
        .collect();
    assert_eq!(in_file.len(), 3);
    assert_eq!(joined(&pages), in_file);
    assert_eq!(pages.len(), 3);
    assert!(pages.iter().all(|page| page.total == Some(3)));
    // A language.
    args.path_prefix = None;
    args.language = Some("rust".into());
    args.limit = Some(25);
    let pages = all_pages(&indexes, &args);
    assert_eq!(joined(&pages), source);
    assert!(pages.iter().all(|page| page.total == Some(source.len())));
    // A cursor from a search without the filter, in a file it leaves out.
    args.language = Some("markdown".into());
    args.limit = None;
    let page = search_after(&indexes, &args, "A:1:1");
    assert_eq!(hit_files(&page), ["README.md"]);
    let readme = cursor_at(&want[0]);
    // A filtered list that ended has no results after its last, and doesn't go
    // on to the other languages as a list with no match at all does.
    let page = search_after(&indexes, &args, &readme);
    assert!(page.hits.is_empty(), "{page:?}");
    assert_eq!((page.total, page.before), (Some(1), 1));
    assert!(page.notes.is_empty(), "{:?}", page.notes);
    // A language that matches nothing stays empty from any cursor, and says
    // so, where a ranked search would list every language.
    args.language = Some("python".into());
    for after in ["A:1:1", readme.as_str()] {
        let page = search_after(&indexes, &args, after);
        assert!(page.hits.is_empty(), "{page:?}");
        assert_eq!((page.total, page.before), (Some(0), 0));
        assert_eq!(
            page.notes,
            ["No matches in python. Search without the language filter to see every language."]
        );
    }
    fixture.shutdown(&indexes);
}

/// The matches of a language may be gone by the time its next page is asked
/// for. The page is of that language still, and so is empty, rather than
/// the matches of the others.
#[test]
fn a_page_of_a_language_stays_in_it_when_its_matches_are_gone() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 4);
    write(
        &root.join("src/zz_data.py"),
        "def go():\n    fetch_widget(1)\n",
    );
    // Searches don't bring the index up to date in the test's time.
    let timing = Timing {
        reconcile_interval: Duration::from_secs(3600),
        ..fast(false)
    };
    let indexes = fixture.open(&root, Backend::Hash, false, timing);
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.language = Some("rust".into());
    args.limit = Some(5);
    let first = indexes.search_blocking(&args).unwrap();
    assert!(hit_files(&first).iter().all(|file| file.ends_with(".rs")));
    let cursor = next_cursor(&first).unwrap();
    // The Rust files that held the matches go, and a Python one is left.
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            std::fs::remove_file(path).unwrap();
        }
    }
    let page = search_after(&indexes, &args, &cursor);
    assert!(page.hits.is_empty(), "{page:?}");
    assert_eq!((page.total, page.before), (Some(0), 0));
    assert_eq!(
        page.notes,
        ["No matches in rust. Search without the language filter to see every language."]
    );
    // Without the language, the Python match is there to be found.
    args.language = None;
    let page = indexes.search_blocking(&args).unwrap();
    assert!(hit_files(&page).contains(&"src/zz_data.py".to_string()));
    fixture.shutdown(&indexes);
}

#[test]
fn the_pages_of_several_indexes_are_one_list_in_path_order() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 3);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes
        .create_blocking(&create_args(&root, Some(Kind::Code)))
        .unwrap();
    indexes
        .create_blocking(&create_args(&root, Some(Kind::Docs)))
        .unwrap();
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.exhaustive = true;
    let whole = indexes.search_blocking(&args).unwrap();
    assert_eq!(whole.searched.len(), 2);
    let list = |output: &SearchOutput| -> Vec<(String, String, usize, usize)> {
        output
            .hits
            .iter()
            .map(|hit| {
                (
                    hit.index.clone(),
                    hit.display.clone(),
                    hit.start_line,
                    hit.column,
                )
            })
            .collect()
    };
    let rendered = |output: &SearchOutput| -> Vec<String> {
        let text = tools::format_search_detail("fetch_widget", output, tools::Detail::Lines);
        tools::read_search_hits(&text).unwrap()[1..].to_vec()
    };
    // By file and place, and by index where both have a result at one.
    let mut sorted = list(&whole);
    sorted.sort_by(|a, b| (&a.1, a.2, a.3, &a.0).cmp(&(&b.1, b.2, b.3, &b.0)));
    assert_eq!(list(&whole), sorted);
    let shared = |hits: &[Hit]| hits.windows(2).any(|two| two[0].place() == two[1].place());
    assert!(
        shared(&whole.hits),
        "an index has a result where the other does"
    );
    args.exhaustive = false;
    for limit in [1, 2, 3, 5] {
        args.limit = Some(limit);
        let pages = all_pages(&indexes, &args);
        let joined: Vec<_> = pages.iter().flat_map(list).collect();
        assert_eq!(joined, list(&whole), "limit {limit}");
        // The pages as the tool writes them, with the results of each, read
        // back in the order of the whole list as it is written.
        let joined: Vec<String> = pages.iter().flat_map(rendered).collect();
        assert_eq!(joined, rendered(&whole), "limit {limit}");
        for page in &pages {
            assert_eq!(page.total, Some(whole.hits.len()));
            // A cursor is a place, so a page ends after all the results at its
            // last one, and may run over the limit by those of other indexes.
            let over = page.hits.get(limit..).unwrap_or_default();
            let last = page.hits[limit.min(page.hits.len()) - 1].place();
            assert!(
                over.iter().all(|hit| hit.place() == last),
                "{limit}: {over:?}"
            );
        }
    }
    fixture.shutdown(&indexes);
}

#[test]
fn a_cursor_that_is_not_one_is_refused() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 2);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    for bad in [
        "nonsense",
        "src/part_00.rs",
        "src/part_00.rs:",
        "src/part_00.rs:line",
        "src/part_00.rs:0",
        "src/part_00.rs:3:0",
        ":4:1",
        "\"\"",
    ] {
        args.after = Some(bad.into());
        let error = indexes.search_blocking(&args).unwrap_err();
        assert!(
            error.starts_with(&format!(
                "{bad:?} isn't a cursor. Use the one that ends the note"
            )),
            "{bad}: {error}"
        );
        assert!(error.ends_with("such as src/a.rs:804:5"), "{error}");
    }
    // Only an exact search has a list to page through.
    for mode in [ModeArg::Hybrid, ModeArg::Semantic] {
        let mut args = search_args(&root, "fetch a widget", Some(mode));
        args.after = Some("src/part_00.rs:1:1".into());
        let error = indexes.search_blocking(&args).unwrap_err();
        assert!(error.contains("is for mode lexical"), "{error}");
        assert!(error.starts_with("after"), "{error}");
        // (Blank leaves it out.)
        args.after = Some(" ".into());
        assert!(indexes.search_blocking(&args).is_ok());
        // `ignore_case` and `exhaustive` do not error outside lexical mode.
        args.after = None;
        args.ignore_case = true;
        args.exhaustive = true;
        assert!(indexes.search_blocking(&args).is_ok());
    }
    fixture.shutdown(&indexes);
}

#[test]
fn a_page_after_a_cursor_is_headed_by_its_place_in_the_list() {
    let hit = |line: usize| Hit {
        index: "proj".into(),
        display: "src/a.rs".into(),
        file: PathBuf::from("/w/src/a.rs"),
        start_line: line,
        end_line: line + 1,
        start_byte: 0,
        end_byte: 0,
        column: 1,
        scope: String::new(),
        symbol: String::new(),
        changed: false,
        lines: Some(super::lexical::Lines {
            shown: vec![(line, "go()".into())],
            more: 0,
        }),
    };
    let page = |first: usize, count: usize, total: usize, after: Option<&str>| SearchOutput {
        hits: (0..count).map(|at| hit(first + at)).collect(),
        total: Some(total),
        before: first.saturating_sub(1),
        after: after.map(str::to_string),
        searched: vec![("proj".into(), Kind::Code, Mode::Lexical)],
        ..SearchOutput::default()
    };
    let header = |output: &SearchOutput| {
        let text = tools::format_search_detail("go()", output, tools::Detail::Lines);
        text.lines().next().unwrap().to_string()
    };
    let proj = vec!["proj".to_string()];
    for (output, text, hits) in [
        // The first page, after nothing, and after a cursor before every result.
        (page(1, 100, 340, None), "100 of 340 results", 100),
        (page(1, 100, 340, Some("A:1:1")), "100 of 340 results", 100),
        (
            page(101, 100, 340, Some("src/a.rs:100:1")),
            "101-200 of 340 results",
            100,
        ),
        (
            page(301, 40, 340, Some("src/a.rs:300:1")),
            "301-340 of 340 results",
            40,
        ),
        (
            page(6, 1, 340, Some("src/a.rs:5:1")),
            "6-6 of 340 results",
            1,
        ),
        (
            page(1_101, 3, 5_000, Some("src/a.rs:1:1")),
            "1,101-1,103 of 5,000 results",
            3,
        ),
        // A list that isn't cut, and the whole of one that follows a cursor.
        (page(1, 3, 3, None), "3 results", 3),
        (page(3, 2, 4, Some("src/a.rs:2:1")), "3-4 of 4 results", 2),
    ] {
        let line = header(&output);
        assert_eq!(line, format!("{text} from proj (code, lexical):"));
        assert_eq!(tools::read_search_header(&line), Some((proj.clone(), hits)));
    }
    // A page that follows the last result has none.
    let past = SearchOutput {
        hits: Vec::new(),
        total: Some(1_340),
        before: 1_340,
        after: Some("src/a.rs:804:5".into()),
        searched: vec![("proj".into(), Kind::Code, Mode::Lexical)],
        ..SearchOutput::default()
    };
    let text = tools::format_search_detail("go()", &past, tools::Detail::Lines);
    assert_eq!(
        text,
        "No results after \"src/a.rs:804:5\" from proj (code, lexical). 1,340 in all."
    );
    assert_eq!(
        tools::read_search_header(text.lines().next().unwrap()),
        Some((proj, 0))
    );
    assert_eq!(tools::read_search_hits(&text), None);
    // A path, and an index, that hold the words around them are read as
    // what they are.
    for (path, index) in [
        ("src/note from draft.rs", "proj"),
        ("src/a.rs:1:1 from proj (code, lexical). 5 in all.", "proj"),
        ("C:\\w\\x\\\"q.rs", "proj"),
        ("src/a.rs", "notes from May. Draft (v2)"),
        ("src/note from draft.rs", "from. a (b, c)"),
    ] {
        let cursor = super::lexical::Cursor::at(path, 1, 1).to_string();
        let output = SearchOutput {
            after: Some(cursor),
            searched: vec![
                (index.into(), Kind::Code, Mode::Lexical),
                ("other".into(), Kind::Docs, Mode::Lexical),
            ],
            ..past.clone()
        };
        let text = tools::format_search_detail("go()", &output, tools::Detail::Lines);
        assert_eq!(
            tools::read_search_header(&text),
            Some((vec![index.to_string(), "other".to_string()], 0)),
            "{text}"
        );
    }
    // A search with no match at all says so, as before.
    let none = SearchOutput {
        total: Some(0),
        after: Some("src/a.rs:804:5".into()),
        ..past
    };
    let text = tools::format_search_detail("go()", &none, tools::Detail::Lines);
    assert!(
        text.starts_with("No results for \"go()\" in proj"),
        "{text}"
    );
    // The results of a page read back as a list's do.
    let text = tools::format_search_detail(
        "go()",
        &page(101, 2, 340, Some("src/a.rs:100:1")),
        tools::Detail::Lines,
    );
    assert_eq!(
        tools::read_search_hits(&text).unwrap(),
        [
            "101-102 of 340 results from proj (code, lexical)",
            "src/a.rs:101-102",
            "src/a.rs:102-103",
        ]
    );
}

#[test]
fn the_search_tool_pages_by_the_cursor_of_a_cut_list() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 6);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let tool = indexes
        .tools()
        .into_iter()
        .find(|tool| tool.definition().name == "index_search")
        .unwrap();
    let context = TaskContext::new(uuid::Uuid::new_v4(), "test", "", &root);
    let run = |value: Value| {
        fixture
            .runtime
            .block_on(tool.execute(&context, params(value)))
    };
    let all = oracle_starts(&root, "fetch_widget").len();
    assert_eq!(all, 20);
    let mut got: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut call = json!({"query": "fetch_widget", "mode": "lexical", "limit": 8});
        if let Some(cursor) = &after {
            call["after"] = json!(cursor);
        }
        let text = run(call).unwrap();
        pages += 1;
        let hits = tools::read_search_hits(&text).unwrap();
        // The first line, and then each result as `file:lines`.
        let first = pages * 8 - 7;
        let count = hits.len() - 1;
        let header = match pages {
            1 => format!("8 of {all} results from callers (code, lexical)"),
            _ => format!(
                "{first}-{} of {all} results from callers (code, lexical)",
                first + count - 1
            ),
        };
        assert_eq!(hits[0], header);
        got.extend(hits[1..].iter().cloned());
        let note = text.lines().last().unwrap();
        match note.rsplit_once(", use after=\"") {
            Some((_, cursor)) => after = Some(cursor.strip_suffix('"').unwrap().to_string()),
            None => {
                assert!(!text.contains("note: "), "{text}");
                break;
            }
        }
    }
    assert_eq!(pages, 3);
    // The pages hold every result once, in the order of the list.
    let text =
        run(json!({"query": "fetch_widget", "mode": "lexical", "exhaustive": true})).unwrap();
    assert_eq!(got, tools::read_search_hits(&text).unwrap()[1..]);
    assert_eq!(got.len(), all);
    // A cursor that isn't one, and one for a ranked search, say what is wrong.
    let error =
        run(json!({"query": "fetch_widget", "mode": "lexical", "after": "part_00"})).unwrap_err();
    assert!(
        error.to_string().contains("\"part_00\" isn't a cursor"),
        "{error}"
    );
    let error = run(json!({"query": "fetch_widget", "mode": "hybrid", "after": "src/a.rs:1:1"}))
        .unwrap_err();
    assert!(error.to_string().contains("mode lexical"), "{error}");
    let definition = tool.definition();
    assert!(
        definition.description.contains("page with after"),
        "{}",
        definition.description
    );
    let after = definition
        .parameters
        .iter()
        .find(|parameter| parameter.name == "after")
        .unwrap();
    assert!(!after.required && after.description.starts_with("Lexical mode only."));
    fixture.shutdown(&indexes);
}

/// Two indexes of the same files list each result twice, once from each. The
/// default listing is in the order of the whole list, and the pages of it
/// join up to that.
#[test]
fn the_listing_of_indexes_that_share_files_keeps_the_order_of_the_list() {
    let fixture = Fixture::new();
    let root = callers_project(&fixture, 3);
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes
        .create_blocking(&create_args(&root, Some(Kind::Code)))
        .unwrap();
    let mut docs = create_args(&root, Some(Kind::Docs));
    docs.include = vec!["src/".into()];
    indexes.create_blocking(&docs).unwrap();
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.exhaustive = true;
    let whole = indexes.search_blocking(&args).unwrap();
    let in_both = whole
        .hits
        .windows(2)
        .filter(|two| two[0].place() == two[1].place() && two[0].index != two[1].index)
        .count();
    // (A file has several results, and each is in both indexes.)
    assert!(in_both >= 6, "{in_both}");
    let rendered = |output: &SearchOutput| -> Vec<String> {
        let text = tools::format_search_detail("fetch_widget", output, tools::Detail::Lines);
        tools::read_search_hits(&text).unwrap()[1..].to_vec()
    };
    let want = rendered(&whole);
    // The listing is in the order of the hits, so a file in two indexes is
    // listed in turns.
    let order: Vec<(String, usize)> = whole
        .hits
        .iter()
        .map(|hit| (hit.index.clone(), hit.start_line))
        .collect();
    let listed: Vec<(String, usize)> = want
        .iter()
        .map(|line| {
            let (index, rest) = line.strip_prefix('[').unwrap().split_once("] ").unwrap();
            let (_, lines) = rest.rsplit_once(':').unwrap();
            let lines = lines.split(' ').next().unwrap();
            (
                index.to_string(),
                lines.split('-').next().unwrap().parse().unwrap(),
            )
        })
        .collect();
    assert_eq!(listed, order);
    args.exhaustive = false;
    for limit in [1, 2, 4, 7] {
        args.limit = Some(limit);
        let pages = all_pages(&indexes, &args);
        let joined: Vec<String> = pages.iter().flat_map(rendered).collect();
        assert_eq!(joined, want, "limit {limit}");
        // A page may run over the limit by the other index's results at its
        // last place, and no more. The note counts the next page as it comes.
        assert!(pages.iter().all(|page| page.hits.len() <= limit + 1));
        if limit <= 2 {
            assert!(pages.iter().any(|page| page.hits.len() > limit));
        }
        for (page, following) in pages.iter().zip(&pages[1..]) {
            let note = next_note(page).unwrap();
            let count = following.hits.len();
            assert!(
                note.contains(&format!(". For the next {count}, use after=\"")),
                "{note}"
            );
        }
    }
    fixture.shutdown(&indexes);
}

/// Files named like what a cursor is written with, and what a shell or a
/// parameter would change. A page after a cursor of one starts after it.
#[test]
fn pages_go_on_through_files_with_hard_names() {
    let fixture = Fixture::new();
    let root = fixture.base.join("names");
    let names = [
        "after=a.rs",
        "\"q.rs",
        " lead.rs",
        "100%.rs",
        "50%25.rs",
        "a b.rs",
        "notes:12.rs",
        "tab\there.rs",
        "it's.rs",
        "$(printf x).rs",
        "z.rs",
    ];
    for name in names {
        write(
            &root.join(name),
            "pub fn one() {\n    fetch_widget(1);\n}\n\npub fn two() {\n    fetch_widget(2);\n}\n",
        );
    }
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.create_blocking(&create_args(&root, None)).unwrap();
    let mut args = search_args(&root, "fetch_widget", Some(ModeArg::Lexical));
    args.exhaustive = true;
    let whole = indexes.search_blocking(&args).unwrap();
    let files: std::collections::BTreeSet<String> = starts(&whole)
        .into_iter()
        .map(|(file, _, _)| file)
        .collect();
    assert_eq!(files, names.iter().map(|name| name.to_string()).collect());
    args.exhaustive = false;
    for limit in [1, 2, 3] {
        args.limit = Some(limit);
        let pages = all_pages(&indexes, &args);
        let joined: Starts = pages.iter().flat_map(starts).collect();
        assert_eq!(joined, starts(&whole), "limit {limit}");
        assert_eq!(pages.len(), whole.hits.len().div_ceil(limit));
        // Each page starts where the one before ended, whatever it is called.
        for (before, page) in pages.iter().zip(&pages[1..]) {
            let last = starts(before).pop().unwrap();
            assert!(starts(page)[0] > last, "{last:?}");
            assert_eq!(page.after, next_cursor(before));
        }
    }
    fixture.shutdown(&indexes);
}

// ----- the command's note for the next page -----

/// A cut list of one result per file, and the command's text of it.
fn command_page(path: &str) -> (SearchOutput, String) {
    let hit = Hit {
        index: "proj".into(),
        display: path.into(),
        file: PathBuf::from("/w").join(path),
        start_line: 1,
        end_line: 2,
        start_byte: 0,
        end_byte: 0,
        column: 1,
        scope: String::new(),
        symbol: String::new(),
        changed: false,
        lines: Some(super::lexical::Lines {
            shown: vec![(1, "go()".into())],
            more: 0,
        }),
    };
    let output = SearchOutput {
        hits: vec![hit],
        total: Some(340),
        next: Some(super::engine::NextPage {
            cursor: super::lexical::Cursor::at(path, 1, 1),
            count: 100,
        }),
        searched: vec![("proj".into(), Kind::Code, Mode::Lexical)],
        ..SearchOutput::default()
    };
    let text = search_text("go()", &output, tools::Detail::Lines);
    (output, text)
}

#[test]
fn the_command_gives_the_next_page_as_a_word_for_the_shell() {
    let (_, text) = command_page("src/a.rs");
    assert_eq!(
        text.lines().last().unwrap(),
        "note: --exhaustive lists all 340, and --path-prefix narrows the search. For the next 100, use --after 'src/a.rs:1:1'"
    );
    // A name that holds the words of the tools' hints, or what a shell acts on.
    // The note keeps it as the cursor writes it, in single quotes.
    for path in [
        "src/exhaustive=true.rs",
        "src/path_prefix narrows.rs",
        "src/after=\"x\".rs",
        "src/$(printf expanded).rs",
        "src/`printf expanded`.rs",
        "src/$HOME.rs",
        "src/it's.rs",
        "src/'; echo hi; '.rs",
        "src/a\\b*?[x].rs",
        "-x.rs",
        "src/a\u{2019}$(Write-Output expanded)\u{2018}.rs",
        "src/\u{201A}\u{201B}'.rs",
    ] {
        let (output, text) = command_page(path);
        let cursor = output.next.as_ref().unwrap().cursor.to_string();
        let note = text.lines().last().unwrap();
        let word = tools::shell_word(&cursor, false);
        assert!(note.ends_with(&format!("use --after {word}")), "{note}");
        // The word holds no quote delimiter of POSIX sh or PowerShell but its
        // wrapping quotes, in either shell's form, and says the same path.
        for powershell in [false, true] {
            let word = tools::shell_word(&cursor, powershell);
            let inside = word
                .strip_prefix('\'')
                .and_then(|word| word.strip_suffix('\''));
            assert!(
                inside.is_some_and(|inside| !inside.contains(SINGLE_QUOTES)),
                "{word}"
            );
            if powershell {
                assert_eq!(powershell_string(&word), Some(cursor.clone()), "{word}");
            }
        }
        assert_eq!(
            super::lexical::Cursor::parse(&cursor).unwrap(),
            output.next.as_ref().unwrap().cursor
        );
        // The tool's note holds the same cursor between double quotes.
        let tool = tools::format_search_detail("go()", &output, tools::Detail::Lines);
        assert!(tool.ends_with(&format!("use after=\"{cursor}\"")), "{tool}");
    }
}

/// What PowerShell reads of `word` as a string in single quotes, as its
/// tokenizer scans one. Any of its single quote characters begins and ends the
/// string, and two in a row stand for one. None where the word is not one
/// string, so that the shell reads on past it.
fn powershell_string(word: &str) -> Option<String> {
    let mut chars = word.chars().peekable();
    if !SINGLE_QUOTES.contains(&chars.next()?) {
        return None;
    }
    let mut text = String::new();
    loop {
        let c = chars.next()?;
        if SINGLE_QUOTES.contains(&c) {
            if !chars
                .peek()
                .is_some_and(|next| SINGLE_QUOTES.contains(next))
            {
                // The end of the string, which must be the end of the word.
                return chars.next().is_none().then_some(text);
            }
            text.push(chars.next()?);
        } else {
            text.push(c);
        }
    }
}

/// A file named with curly quotes can't end the single quotes of the word
/// that PowerShell is given for it. A model of its scanning stands in for it.
#[test]
fn a_powershell_word_is_one_string_whatever_quotes_the_text_holds() {
    // The model sees what a cursor written without them would do.
    let broken = "'a\u{2019}$(Write-Output expanded)\u{2018}.rs:1:1'";
    assert_eq!(powershell_string(broken), None);
    for text in [
        "src/a.rs:1:1",
        "it's",
        "a\u{2019}$(Write-Output expanded)\u{2018}.rs:1:1",
        "\u{2018}\u{2019}\u{201A}\u{201B}'",
        "\u{2019}",
        "''",
        "a $(b) `c` \"d\" ; e",
        "",
    ] {
        let word = tools::shell_word(text, true);
        assert_eq!(powershell_string(&word).as_deref(), Some(text), "{word}");
    }
    // A cursor holds none of them, so its word needs no doubling.
    for path in [
        "a\u{2019}$(Write-Output expanded)\u{2018}.rs",
        "\u{201A}\u{201B}'.rs",
        "plain.rs",
    ] {
        let cursor = super::lexical::Cursor::at(path, 1, 1).to_string();
        let word = tools::shell_word(&cursor, true);
        assert_eq!(word, format!("'{cursor}'"));
        assert_eq!(powershell_string(&word), Some(cursor));
    }
}

/// What a shell reads of `shell_word` is the text, whatever the text holds.
#[cfg(unix)]
#[test]
fn a_shell_word_is_read_back_as_the_text_it_quotes() {
    let hostile = [
        "src/a.rs:1:1",
        "src/$(printf expanded).rs:1:1",
        "src/`printf expanded`.rs:1:1",
        "$HOME ${HOME} $((1+1))",
        "it's",
        "'",
        "''",
        "a'b'c",
        "\"double\" and 'single'",
        "back\\slash \\n \\'",
        "; rm -rf / #",
        "a && b || c | d > e < f",
        "*?[a-z]{1,2}~",
        "!history !!",
        "  spaces  ",
        "tab\there",
        "new\nline",
        "-x.rs:1:1",
        "é日本 \u{1F600}",
        "a\u{2019}$(Write-Output expanded)\u{2018}.rs",
        "",
    ];
    for text in hostile {
        let word = tools::shell_word(text, false);
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {word}"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{word}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), text, "{word}");
        // And it reaches a program as one argument.
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("set -- {word}; printf %s \"$#:$1\""))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), format!("1:{text}"));
    }
    // PowerShell doubles the quote.
    assert_eq!(tools::shell_word("it's", true), "'it''s'");
    assert_eq!(tools::shell_word("a$(b)", true), "'a$(b)'");
}

#[test]
fn the_command_takes_a_cursor_that_starts_with_a_dash() {
    for args in [
        &["search", "-m", "lexical", "--after", "-x.rs:1:1", "go"][..],
        &["search", "-m", "lexical", "--after=-x.rs:1:1", "go"],
    ] {
        let mut argv = vec!["arko-index"];
        argv.extend_from_slice(args);
        let command = Cli::try_parse_from(argv).unwrap().command;
        let IndexCommand::Search { after, .. } = command else {
            panic!("a search");
        };
        assert_eq!(after.as_deref(), Some("-x.rs:1:1"));
    }
}

#[test]
fn worktree_resolves_main_repo_index_and_rebases_path_prefix() {
    let fixture = Fixture::new();
    let main_repo = fixture.code_project();
    let other_repo = fixture.docs_project();
    let indexes = fixture.open(&main_repo, Backend::Hash, false, fast(false));
    indexes
        .create_blocking(&create_args(&main_repo, None))
        .unwrap();
    indexes
        .create_blocking(&create_args(&other_repo, None))
        .unwrap();

    // Set up a git linked worktree structure pointing to `main_repo/.git/worktrees/feat`.
    let wt_gitdir = main_repo.join(".git/worktrees/feat");
    std::fs::create_dir_all(&wt_gitdir).unwrap();
    std::fs::write(wt_gitdir.join("commondir"), "../..\n").unwrap();
    std::fs::write(wt_gitdir.join("HEAD"), "ref: refs/heads/feat/my-branch\n").unwrap();
    let worktree = fixture.base.join("wt/feat");
    write(&worktree.join("src/net.rs"), NET_RS);
    write(&worktree.join("src/config.rs"), CONFIG_RS);
    write(
        &worktree.join("src/branch_only.rs"),
        "pub fn branch_only_marker() -> u32 { 42 }\n",
    );
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", wt_gitdir.display()),
    )
    .unwrap();

    // Before first search, `known` recognizes the main repository from the worktree and subdirs,
    // and never lists worktree indexes.
    let known = indexes.known(&worktree);
    assert_eq!(known[0].name, "proj");
    assert_eq!(known[0].place, Some("the working directory"));
    let sub_known = indexes.known(&worktree.join("src"));
    assert_eq!(sub_known[0].name, "proj");
    assert_eq!(sub_known[0].place, Some("holds the working directory"));

    // Before first search, `index_status` with `index: "."` resolves to the main repo index.
    let reports = indexes.status_blocking(Some("."), &worktree).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].manifest.as_ref().unwrap().name, "proj");

    // Searching from the worktree with no `index` and a relative `path_prefix` automatically
    // creates the worktree's own index (`proj@feat-my-branch`) and searches the worktree's own files.
    let mut args = search_args(&worktree, "retry_with_backoff", Some(ModeArg::Lexical));
    args.path_prefix = Some("src/net.rs".into());
    let output = indexes.search_blocking(&args).unwrap();
    assert_eq!(output.searched[0].0, "proj@feat-my-branch", "{output:?}");
    assert_eq!(hit_files(&output), vec!["src/net.rs"], "{output:?}");
    assert_eq!(output.hits[0].file, worktree.join("src/net.rs"));

    // Branch-only code in the worktree is searchable immediately (with hybrid/dense search!).
    let branch_args = search_args(&worktree, "branch_only_marker", Some(ModeArg::Hybrid));
    let branch_out = indexes.search_blocking(&branch_args).unwrap();
    assert_eq!(branch_out.searched[0].0, "proj@feat-my-branch");
    assert_eq!(branch_out.searched[0].2, Mode::Hybrid);
    assert_eq!(
        hit_files(&branch_out)[0],
        "src/branch_only.rs",
        "{branch_out:?}"
    );

    // Searching from a worktree subdirectory resolves `path_prefix` against the worktree's own index.
    let mut sub_args = search_args(
        &worktree.join("src"),
        "retry_with_backoff",
        Some(ModeArg::Lexical),
    );
    sub_args.path_prefix = Some("net.rs".into());
    let sub_output = indexes.search_blocking(&sub_args).unwrap();
    assert_eq!(hit_files(&sub_output), vec!["net.rs"], "{sub_output:?}");
    assert_eq!(sub_output.hits[0].file, worktree.join("src/net.rs"));

    // `known` still never lists the worktree index!
    let known_after = indexes.known(&worktree);
    assert!(
        known_after.iter().all(|k| !k.name.contains('@')),
        "{known_after:?}"
    );
    let known_main = indexes.known(&main_repo);
    assert!(
        known_main.iter().all(|k| !k.name.contains('@')),
        "{known_main:?}"
    );

    // Naming a DIFFERENT index explicitly from a worktree rebases a relative `path_prefix`
    // against that named index's root instead of failing with "outside the index".
    let mut cross = search_args(&worktree, "production", Some(ModeArg::Hybrid));
    cross.index = Some("handbook".into());
    cross.path_prefix = Some("deploy.md".into());
    let cross_out = indexes.search_blocking(&cross).unwrap();
    assert!(
        !cross_out.hits.is_empty(),
        "expected hits in handbook/deploy.md, got {cross_out:?}"
    );
    assert!(
        cross_out
            .hits
            .iter()
            .all(|hit| hit.file.ends_with("deploy.md")),
        "{cross_out:?}"
    );

    // An escaping relative `path_prefix` on a named index still fails cleanly.
    let mut escape = search_args(&worktree, "retry", Some(ModeArg::Hybrid));
    escape.index = Some("handbook".into());
    escape.path_prefix = Some("../proj/src".into());
    let err = indexes.search_blocking(&escape).unwrap_err();
    assert!(err.contains("is outside the index"), "{err}");

    fixture.shutdown(&indexes);
}

// ----- lifetimes, borrowed embeddings, worktree indexes & sweep -----

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.test")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.test")
        .args(args)
        .status()
        .expect("git command");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

fn init_git_repo(dir: &Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.name", "Ada"]);
    git(dir, &["config", "user.email", "ada@example.test"]);
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "Initial commit"]);
}

/// Building a second index or worktree index borrows cached vectors from donor
/// indexes with the same model so unchanged chunks have dense vectors at once,
/// and after a merge back to main, main borrows the branch's new vectors too.
#[test]
fn borrowed_embeddings_work_both_ways_and_surface_counts() {
    let fixture = Fixture::new();
    let main_repo = fixture.code_project();
    let indexes = fixture.open(&main_repo, Backend::Hash, false, fast(false));
    let main_outcome = indexes
        .create_blocking(&create_args(&main_repo, Some(Kind::Code)))
        .unwrap();
    let main_chunks = main_outcome.record.chunks;
    assert!(main_chunks >= 2);

    // Clone the tree with one extra file and build with `defer_embed: true` so
    // `index2::build` runs with `embed_later: true` + `borrow_caches` (no embed pass yet!).
    let branch_repo = fixture.base.join("branch-copy");
    write(&branch_repo.join("src/net.rs"), NET_RS);
    write(&branch_repo.join("src/config.rs"), CONFIG_RS);
    write(
        &branch_repo.join("src/extra.rs"),
        "/// Extra feature added on the branch.\npub fn extra_branch_fn() -> usize { 99 }\n",
    );
    let branch_args = CreateArgs {
        name: Some("branch-copy".into()),
        lifetime: Some(Lifetime::Days(7)),
        wait: false,
        defer_embed: true,
        ..create_args(&branch_repo, Some(Kind::Code))
    };
    let branch_outcome = indexes.create_blocking(&branch_args).unwrap();
    // Both copied files (`src/net.rs` and `src/config.rs`, 2 chunks) were borrowed during build!
    assert_eq!(
        branch_outcome.record.borrowed, 2,
        "expected both unchanged chunks borrowed during build, got {:?}",
        branch_outcome.record
    );
    let create_text = tools::format_create(&branch_outcome);
    assert!(
        create_text.contains("Borrowed ") && create_text.contains("cached vector"),
        "{create_text}"
    );
    assert!(
        create_text.contains("Lifetime: 7 days of use"),
        "{create_text}"
    );

    // Even BEFORE running `embed` on branch-copy, both unchanged chunks ALREADY have
    // vectors (hybrid search works immediately!).
    let before_embed = indexes
        .status_blocking(Some("branch-copy"), &branch_repo)
        .unwrap();
    let dense = before_embed[0].dense.as_ref().unwrap();
    assert_eq!(dense.embedded, 2, "{dense:?}");
    assert_eq!(dense.docs, 3, "{dense:?}");

    // Now embed the remaining 1 chunk on branch-copy.
    let embed_msg = indexes
        .manage_blocking(
            "branch-copy",
            &Action::Embed {
                max_tokens: None,
                wait: true,
            },
            &branch_repo,
        )
        .unwrap();
    assert!(embed_msg.contains("embedded 1 text"), "{embed_msg}");

    // Simulate merging the branch back into main: copy `src/extra.rs` into main_repo
    // and rebuild main. Main should borrow the vector for `extra.rs` from branch-copy!
    write(
        &main_repo.join("src/extra.rs"),
        "/// Extra feature added on the branch.\npub fn extra_branch_fn() -> usize { 99 }\n",
    );
    let rebuild_msg = indexes
        .manage_blocking("proj", &Action::Rebuild, &main_repo)
        .unwrap();
    assert!(
        rebuild_msg.contains("Borrowed 1 cached vector"),
        "expected main to borrow merged chunk vector from branch, got: {rebuild_msg}"
    );
    let main_after = indexes.status_blocking(Some("proj"), &main_repo).unwrap();
    let main_dense = main_after[0].dense.as_ref().unwrap();
    assert_eq!(main_dense.embedded, main_dense.docs, "{main_dense:?}");

    // EmbedSummary::line() reports borrowed vectors cleanly when non-zero.
    let borrowed_only = super::engine::EmbedSummary {
        tokens: 0,
        dollars: 0.0,
        new: 0,
        borrowed: 5,
        embedded: 5,
        docs: 5,
        cap: 1000,
        cap_reached: false,
        stopped: false,
        superseded: false,
        not_embedded_tokens: 0,
        too_long: 0,
    }
    .line();
    assert_eq!(
        borrowed_only,
        "borrowed 5 cached vectors, and 5/5 chunks have vectors"
    );

    let borrowed_and_new = super::engine::EmbedSummary {
        tokens: 120,
        dollars: 0.0,
        new: 2,
        borrowed: 3,
        embedded: 5,
        docs: 5,
        cap: 1000,
        cap_reached: false,
        stopped: false,
        superseded: false,
        not_embedded_tokens: 0,
        too_long: 0,
    }
    .line();
    assert_eq!(
        borrowed_and_new,
        "borrowed 3 cached vectors and embedded 2 texts for 120 tokens ($0), and 5/5 chunks have vectors"
    );

    fixture.shutdown(&indexes);
}

/// Worktree indexes are auto-created on first search via cwd, `index` path, or
/// `path_prefix` path, inherit settings, support detached HEAD names, never run
/// a watcher, reconcile edits on search, and handle concurrent first searches
/// without deadlocking or racing.
#[test]
fn worktree_indexes_auto_create_reconcile_never_watch_and_handle_concurrency() {
    let fixture = Fixture::new();
    let main_repo = fixture.code_project();
    init_git_repo(&main_repo);
    let wt_a = fixture.base.join("wt/feature-a");
    let wt_detached = fixture.base.join("wt/detached-wt");
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-b",
            "feat/alpha",
            wt_a.to_str().unwrap(),
            "HEAD",
        ],
    );
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "--detach",
            wt_detached.to_str().unwrap(),
            "HEAD",
        ],
    );

    // Open monitoring instance on main_repo.
    let indexes = fixture.open(&main_repo, Backend::Hash, true, fast(false));
    let main_outcome = indexes
        .create_blocking(&create_args(&main_repo, Some(Kind::Code)))
        .unwrap();

    // 1) Concurrent first searches in `wt_a` (4 threads) all succeed cleanly
    // and create a single worktree index `proj@feat-alpha`.
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let indexes = Arc::clone(&indexes);
            let wt_a = wt_a.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let args = search_args(&wt_a, "retry_with_backoff", Some(ModeArg::Hybrid));
                indexes.search_blocking(&args).unwrap()
            })
        })
        .collect();
    for handle in handles {
        let out = handle.join().unwrap();
        assert_eq!(out.searched[0].0, "proj@feat-alpha");
        assert_eq!(out.searched[0].2, Mode::Hybrid);
    }

    // Worktree index inherited main's settings, has `Worktree` lifetime & `parent_index`,
    // and is NEVER watched even though `indexes` has `monitor: true`!
    let wt_found = indexes.resolve("proj@feat-alpha", &main_repo).unwrap();
    assert_eq!(
        wt_found.manifest.lifetime,
        Some(Lifetime::Worktree {
            main: "proj".into()
        })
    );
    assert_eq!(
        wt_found.manifest.parent_index.as_deref(),
        Some(main_outcome.layout.id().as_str())
    );
    assert!(!wt_found.manifest.monitor);
    assert_eq!(indexes.watch_state(&wt_found.layout), WatchState::Off);
    assert!(
        indexes
            .manage_blocking("proj@feat-alpha", &Action::Monitor(true), &main_repo)
            .is_err(),
        "worktree indexes must refuse monitor_on"
    );

    // Editing a file in `wt_a` is reconciled on next search without a watcher.
    write(
        &wt_a.join("src/wt_edit.rs"),
        "pub fn reconciled_worktree_edit() -> bool { true }\n",
    );
    let edited = indexes
        .search_blocking(&search_args(
            &wt_a,
            "reconciled_worktree_edit",
            Some(ModeArg::Lexical),
        ))
        .unwrap();
    assert_eq!(hit_files(&edited), vec!["src/wt_edit.rs"]);
    assert_eq!(indexes.watch_state(&wt_found.layout), WatchState::Off);

    // 2) Searching from `main_repo` with `path_prefix` pointing into `wt_detached`
    // auto-creates a worktree index named `proj@<7-char-sha>`.
    let mut from_main = search_args(&main_repo, "parse_config", Some(ModeArg::Hybrid));
    from_main.path_prefix = Some(wt_detached.join("src").to_string_lossy().into_owned());
    let detached_out = indexes.search_blocking(&from_main).unwrap();
    let detached_name = &detached_out.searched[0].0;
    assert!(
        detached_name.starts_with("proj@") && detached_name.len() == "proj@".len() + 7,
        "expected short SHA worktree name, got {detached_name}"
    );
    assert!(detached_out.hits[0].file.starts_with(&wt_detached));

    // 3) Searching by passing `index: <worktree path>` also resolves the worktree index.
    let mut by_index = search_args(&main_repo, "retry_with_backoff", Some(ModeArg::Lexical));
    by_index.index = Some(wt_a.to_string_lossy().into_owned());
    let by_index_out = indexes.search_blocking(&by_index).unwrap();
    assert_eq!(by_index_out.searched[0].0, "proj@feat-alpha");

    // 4) `remove_worktree_indexes` removes the worktree index at once without touching main.
    let removed = indexes.remove_worktree_indexes(&wt_a);
    assert_eq!(removed, 1);
    assert!(indexes.resolve("proj@feat-alpha", &main_repo).is_err());
    assert!(indexes.resolve("proj", &main_repo).is_ok());

    fixture.shutdown(&indexes);
}

/// Active-day counter advances at most once per UTC calendar day, ignores
/// clock skew backwards, and counts only active days (holidays expire nothing).
#[test]
fn active_day_counter_and_last_search_throttling() {
    let fixture = Fixture::new();
    let idx_dir = store::indexes_dir(&fixture.data());

    assert_eq!(store::record_active_day(&idx_dir, "2026-11-01"), 1);
    // Repeated same day stays 1.
    assert_eq!(store::record_active_day(&idx_dir, "2026-11-01"), 1);
    // Clock skew backwards stays 1.
    assert_eq!(store::record_active_day(&idx_dir, "2026-10-31"), 1);
    // Next active day after a 20-day holiday only increments by 1!
    assert_eq!(store::record_active_day(&idx_dir, "2026-11-21"), 2);
    assert_eq!(store::record_active_day(&idx_dir, "2026-11-21"), 2);
    assert_eq!(store::load_usage(&idx_dir).days, 2);

    // Last-search file is throttled within an hour unless the active day advances.
    let root = fixture.code_project();
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));
    indexes.set_active_day(10);
    let outcome = indexes
        .create_blocking(&create_args(&root, Some(Kind::Code)))
        .unwrap();
    assert!(store::load_last_search(&outcome.layout.dir).is_none());

    indexes
        .search_blocking(&search_args(&root, "retry", Some(ModeArg::Lexical)))
        .unwrap();
    let first_ls = store::load_last_search(&outcome.layout.dir).unwrap();
    assert_eq!(first_ls.day, 10);

    // Second search within the same hour on day 10 does not rewrite `last_search.json`.
    std::thread::sleep(Duration::from_millis(15));
    indexes
        .search_blocking(&search_args(&root, "config", Some(ModeArg::Lexical)))
        .unwrap();
    let second_ls = store::load_last_search(&outcome.layout.dir).unwrap();
    assert_eq!(
        second_ls.at, first_ls.at,
        "should be throttled within 1 hour"
    );

    // Advancing the active day updates `last_search.json` immediately on next search.
    indexes.set_active_day(11);
    indexes
        .search_blocking(&search_args(&root, "config", Some(ModeArg::Lexical)))
        .unwrap();
    let third_ls = store::load_last_search(&outcome.layout.dir).unwrap();
    assert_eq!(third_ls.day, 11);

    fixture.shutdown(&indexes);
}

/// Removing a kept index via `index_manage` asks the user for approval (and
/// refuses when no one can answer), while temporary/worktree indexes remove
/// directly. `index_create` records creator metadata, and `action=lifetime`
/// updates lifetime.
#[test]
fn removing_kept_index_asks_user_approval_and_temporary_removes_directly() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let docs = fixture.docs_project();
    let indexes = fixture.open(&root, Backend::Hash, false, fast(false));

    // 1) Without approvals (`can_answer() == false`), removing a kept index is refused.
    let no_ask_tools = indexes.tools_for(
        Some("sess-1".into()),
        Some(crate::approvals::Approvals::none()),
    );
    let run_with =
        |tools: &[Arc<dyn arko_agents::Tool>], ctx: &TaskContext<'_>, name: &str, value: Value| {
            let tool = tools
                .iter()
                .find(|t| t.definition().name == name)
                .unwrap()
                .clone();
            fixture.runtime.block_on(tool.execute(ctx, params(value)))
        };

    let task_id = uuid::Uuid::new_v4();
    let ctx = TaskContext::new(task_id, "main", "", &root);
    run_with(
        &no_ask_tools,
        &ctx,
        "index_create",
        json!({"path": ".", "lifetime": "keep", "embed": false}),
    )
    .unwrap();
    let created = indexes.resolve("proj", &root).unwrap();
    assert_eq!(created.manifest.lifetime, Some(Lifetime::Keep));
    assert_eq!(
        created.manifest.created_by_session.as_deref(),
        Some("sess-1")
    );
    assert_eq!(
        created.manifest.created_by_run.as_deref(),
        Some(task_id.to_string().as_str())
    );

    let err = run_with(
        &no_ask_tools,
        &ctx,
        "index_manage",
        json!({"action": "remove", "index": "proj"}),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("approval") && err.to_string().contains("arko index remove proj"),
        "{err}"
    );
    assert!(indexes.resolve("proj", &root).is_ok(), "kept index stays");

    // 2) A temporary index (`lifetime: 7`) removes directly without approval.
    run_with(
        &no_ask_tools,
        &ctx,
        "index_create",
        json!({"path": docs.to_string_lossy(), "lifetime": 7, "embed": false}),
    )
    .unwrap();
    let removed = run_with(
        &no_ask_tools,
        &ctx,
        "index_manage",
        json!({"action": "remove", "index": "handbook"}),
    )
    .unwrap();
    assert!(removed.contains("Removed the index handbook"), "{removed}");
    assert!(indexes.resolve("handbook", &root).is_err());

    // 3) With interactive Approvals wired to a Feed: rejecting keeps it; approving removes it.
    let feed = crate::remote::Feed::new("main");
    let approvals = crate::approvals::Approvals::new(Some(feed.clone()), true, None);
    let ask_tools = indexes.tools_for(Some("sess-1".into()), Some(approvals));
    let manage = ask_tools
        .iter()
        .find(|t| t.definition().name == "index_manage")
        .unwrap()
        .clone();

    // Reject first.
    let rejected_text = fixture.runtime.block_on(async {
        let feed_answer = feed.clone();
        let responder = tokio::spawn(async move {
            let req = loop {
                let list = feed_answer.approvals();
                if let Some(first) = list.into_iter().next() {
                    break first;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            assert_eq!(req.approval.kind_word(), "Index");
            assert_eq!(req.approval.name(), "proj");
            assert_eq!(req.approval.title(), "Remove the kept index proj?");
            feed_answer.answer_approvals(&crate::approvals::ApprovalAnswers {
                choices: vec![crate::approvals::ApprovalChoice {
                    id: req.id,
                    approve: false,
                    option: false,
                }],
                words: Some("still using it".into()),
            });
        });
        let res = manage
            .execute(&ctx, params(json!({"action": "remove", "index": "proj"})))
            .await
            .unwrap();
        responder.await.unwrap();
        res
    });
    assert!(
        rejected_text.contains("chose not to remove") && rejected_text.contains("still using it"),
        "{rejected_text}"
    );
    assert!(indexes.resolve("proj", &root).is_ok());

    // Approve next.
    let approved_text = fixture.runtime.block_on(async {
        let feed_answer = feed.clone();
        let responder = tokio::spawn(async move {
            let req = loop {
                let list = feed_answer.approvals();
                if let Some(first) = list.into_iter().next() {
                    break first;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            feed_answer.answer_approvals(&crate::approvals::ApprovalAnswers {
                choices: vec![crate::approvals::ApprovalChoice {
                    id: req.id,
                    approve: true,
                    option: false,
                }],
                words: Some("go ahead".into()),
            });
        });
        let res = manage
            .execute(&ctx, params(json!({"action": "remove", "index": "proj"})))
            .await
            .unwrap();
        responder.await.unwrap();
        res
    });
    assert!(
        approved_text.contains("Removed the index proj") && approved_text.contains("go ahead"),
        "{approved_text}"
    );
    assert!(indexes.resolve("proj", &root).is_err());

    fixture.shutdown(&indexes);
}

/// Prompt discovery (`known`) orders covering cwd -> kept -> temporary by most
/// recent search, hides worktree indexes; banner (`summary`) names kept indexes
/// and counts temporary ones; watcher scoping watches ONLY covering cwd and
/// kept indexes inside cwd.
#[test]
fn discovery_ordering_banner_summary_and_watcher_scoping() {
    let fixture = Fixture::new();
    let workspace = fixture.base.join("ws");
    let kept_in_ws = workspace.join("zeta-kept");
    let temp_old = workspace.join("alpha-temp-old");
    let temp_recent = workspace.join("beta-temp-recent");
    let outside_kept = fixture.base.join("outside-kept");
    for dir in [&kept_in_ws, &temp_old, &temp_recent, &outside_kept] {
        write(&dir.join("src/lib.rs"), "pub fn hello() {}\n");
    }
    init_git_repo(&kept_in_ws);
    let wt_dir = workspace.join("wt/branch1");
    git(
        &kept_in_ws,
        &[
            "worktree",
            "add",
            "-b",
            "feat/one",
            wt_dir.to_str().unwrap(),
            "HEAD",
        ],
    );

    let indexes = fixture.open(&workspace, Backend::Hash, true, fast(false));
    indexes.set_active_day(10);

    // Create kept + temporary indexes inside workspace, and one kept outside workspace.
    let kept_out = indexes
        .create_blocking(&CreateArgs {
            name: Some("zeta-kept".into()),
            lifetime: Some(Lifetime::Keep),
            embed: Some(false),
            ..create_args(&kept_in_ws, Some(Kind::Code))
        })
        .unwrap();
    let old_out = indexes
        .create_blocking(&CreateArgs {
            name: Some("alpha-temp-old".into()),
            lifetime: Some(Lifetime::Days(14)),
            embed: Some(false),
            ..create_args(&temp_old, Some(Kind::Code))
        })
        .unwrap();
    let recent_out = indexes
        .create_blocking(&CreateArgs {
            name: Some("beta-temp-recent".into()),
            lifetime: Some(Lifetime::Days(14)),
            embed: Some(false),
            ..create_args(&temp_recent, Some(Kind::Code))
        })
        .unwrap();
    let outside_out = indexes
        .create_blocking(&CreateArgs {
            name: Some("outside-kept".into()),
            lifetime: Some(Lifetime::Keep),
            embed: Some(false),
            ..create_args(&outside_kept, Some(Kind::Code))
        })
        .unwrap();

    // Search `temp_old` on day 10, `temp_recent` on day 12, and auto-create a worktree index.
    indexes
        .search_blocking(&search_args(&temp_old, "hello", Some(ModeArg::Lexical)))
        .unwrap();
    indexes.set_active_day(12);
    indexes
        .search_blocking(&search_args(&temp_recent, "hello", Some(ModeArg::Lexical)))
        .unwrap();
    indexes
        .search_blocking(&search_args(&wt_dir, "hello", Some(ModeArg::Lexical)))
        .unwrap();

    // `known(&workspace)` lists kept BEFORE temporary even though "alpha" < "zeta",
    // then temporary by most recent search (`beta-temp-recent` before `alpha-temp-old`),
    // then outside, and NEVER lists the worktree index!
    let known: Vec<String> = indexes
        .known(&workspace)
        .into_iter()
        .map(|k| k.name)
        .collect();
    assert_eq!(
        known,
        vec![
            "zeta-kept",
            "beta-temp-recent",
            "alpha-temp-old",
            "outside-kept"
        ]
    );

    // Banner `summary()` names kept workspace index and counts the rest (2 temp + 1 worktree = 3).
    wait_for("kept watcher", || {
        indexes.watch_state(&kept_out.layout).active()
    });
    let summary = indexes.summary().unwrap();
    assert!(
        summary.contains("index zeta-kept (code, lexical, monitored) (+3 temporary)"),
        "{summary}"
    );

    // Watcher scoping: ONLY `kept_in_ws` is watched! Temporary inside ws, outside kept,
    // and worktree index are NOT watched.
    assert!(indexes.watch_state(&kept_out.layout).active());
    assert_eq!(indexes.watch_state(&old_out.layout), WatchState::Off);
    assert_eq!(indexes.watch_state(&recent_out.layout), WatchState::Off);
    assert_eq!(indexes.watch_state(&outside_out.layout), WatchState::Off);
    let wt_found = indexes.resolve("zeta-kept@feat-one", &workspace).unwrap();
    assert_eq!(indexes.watch_state(&wt_found.layout), WatchState::Off);

    // Promoting `alpha-temp-old` to `Keep` starts watching it; demoting `zeta-kept` stops watching it!
    indexes
        .manage_blocking(
            "alpha-temp-old",
            &Action::SetLifetime(Lifetime::Keep),
            &workspace,
        )
        .unwrap();
    wait_for("promoted watcher", || {
        indexes.watch_state(&old_out.layout).active()
    });
    indexes
        .manage_blocking(
            "zeta-kept",
            &Action::SetLifetime(Lifetime::Days(7)),
            &workspace,
        )
        .unwrap();
    assert_eq!(indexes.watch_state(&kept_out.layout), WatchState::Off);

    // When NO workspace indexes are kept, banner counts temporary ones cleanly.
    indexes
        .manage_blocking(
            "alpha-temp-old",
            &Action::SetLifetime(Lifetime::Days(7)),
            &workspace,
        )
        .unwrap();
    let temp_only = indexes.summary().unwrap();
    assert_eq!(temp_only, "4 temporary indexes");

    fixture.shutdown(&indexes);
}

/// `index_status` scopes visibility to cwd + current session (with `index: "all"`
/// showing hidden ones), groups worktree indexes indented under their main index,
/// shows a top expiring-soon warning line, and reports lifetime, last search,
/// disk size, and health flags (`folder missing`, `branch merged`).
#[test]
fn index_status_visibility_grouping_warnings_and_health_flags() {
    let fixture = Fixture::new();
    let main_repo = fixture.code_project();
    init_git_repo(&main_repo);
    let wt_merged = fixture.base.join("wt/merged-branch");
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-b",
            "merged-branch",
            wt_merged.to_str().unwrap(),
            "HEAD",
        ],
    );
    let session_other = fixture.base.join("created-by-session");
    let hidden_other = fixture.base.join("hidden-other");
    let missing_drive = fixture.base.join("unmounted-drive/sub/gone-repo");
    write(&session_other.join("lib.rs"), "pub fn s() {}\n");
    write(&hidden_other.join("lib.rs"), "pub fn h() {}\n");
    write(&missing_drive.join("lib.rs"), "pub fn m() {}\n");

    let indexes = fixture.open(&main_repo, Backend::Hash, false, fast(false));
    indexes.set_active_day(10);

    // Create main (kept), session_other (14d, created by sess-A), hidden_other (kept, other session),
    // and missing_drive (kept, whose parent dir will be deleted to simulate unmounted drive).
    indexes
        .create_blocking(&CreateArgs {
            name: Some("proj".into()),
            lifetime: Some(Lifetime::Keep),
            ..create_args(&main_repo, Some(Kind::Code))
        })
        .unwrap();
    indexes
        .create_blocking(&CreateArgs {
            name: Some("task-idx".into()),
            lifetime: Some(Lifetime::Days(5)),
            embed: Some(false),
            created_by_session: Some("sess-A".into()),
            ..create_args(&session_other, Some(Kind::Code))
        })
        .unwrap();
    indexes
        .create_blocking(&CreateArgs {
            name: Some("hidden-idx".into()),
            lifetime: Some(Lifetime::Keep),
            embed: Some(false),
            created_by_session: Some("sess-B".into()),
            ..create_args(&hidden_other, Some(Kind::Code))
        })
        .unwrap();
    indexes
        .create_blocking(&CreateArgs {
            name: Some("drive-idx".into()),
            lifetime: Some(Lifetime::Keep),
            embed: Some(false),
            created_by_session: Some("sess-A".into()),
            ..create_args(&missing_drive, Some(Kind::Code))
        })
        .unwrap();
    // Auto-create worktree index (its HEAD == main HEAD and clean -> branch merged!).
    indexes
        .search_blocking(&search_args(&wt_merged, "retry", Some(ModeArg::Hybrid)))
        .unwrap();

    // Remove the whole "unmounted-drive" directory (parent gone -> folder missing).
    std::fs::remove_dir_all(fixture.base.join("unmounted-drive")).unwrap();

    // Advance to day 13 so `task-idx` (5d created on day 10) has 2d left (<= 3d warning!).
    indexes.set_active_day(13);

    let tools = indexes.tools_for(Some("sess-A".into()), None);
    let status_tool = tools
        .iter()
        .find(|t| t.definition().name == "index_status")
        .unwrap()
        .clone();
    let ctx = TaskContext::new(uuid::Uuid::new_v4(), "main", "", &main_repo);

    // Default `index_status` shows visible indexes (proj + indented proj@merged-branch,
    // task-idx, drive-idx) and hides `hidden-idx`.
    let visible = fixture
        .runtime
        .block_on(status_tool.execute(&ctx, params(json!({}))))
        .unwrap();
    // Top warning line for task-idx (2d left).
    assert!(
        visible.starts_with(
            "task-idx (2d left) expires within 3 days of use unless searched or kept.\n"
        ),
        "{visible}"
    );
    // Worktree index is indented under `proj` with `(branch merged)` flag!
    assert!(
        visible.contains("\n  proj@merged-branch — code") && visible.contains("(branch merged)"),
        "{visible}"
    );
    // `drive-idx` shows `(folder missing)`.
    assert!(
        visible.contains("drive-idx — code") && visible.contains("(folder missing)"),
        "{visible}"
    );
    // Shows lifetime, last search, and size.
    assert!(
        visible.contains("lifetime: 5d (2d left), never searched"),
        "{visible}"
    );
    assert!(
        visible.contains("lifetime: worktree (27d left), searched 3 active days ago"),
        "{visible}"
    );
    assert!(
        visible.contains("KB") || visible.contains(" B"),
        "{visible}"
    );
    // Hides `hidden-idx` and reports 1 hidden index.
    assert!(!visible.contains("hidden-idx"), "{visible}");
    assert!(
        visible.contains("1 other index outside this directory (call index_status with index=\"all\" to list them)"),
        "{visible}"
    );

    // `index_status(index: "all")` shows `hidden-idx` too with no hidden footer.
    let all_text = fixture
        .runtime
        .block_on(status_tool.execute(&ctx, params(json!({"index": "all"}))))
        .unwrap();
    assert!(all_text.contains("hidden-idx"), "{all_text}");
    assert!(!all_text.contains("outside this directory"), "{all_text}");

    // Searching `task-idx` resets its countdown so the top warning disappears!
    indexes
        .search_blocking(&search_args(&session_other, "s", Some(ModeArg::Lexical)))
        .unwrap();
    let after_search = fixture
        .runtime
        .block_on(status_tool.execute(&ctx, params(json!({}))))
        .unwrap();
    assert!(
        !after_search.contains("expires within 3 days"),
        "{after_search}"
    );
    assert!(
        after_search.contains("lifetime: 5d (5d left), searched today"),
        "{after_search}"
    );

    fixture.shutdown(&indexes);
}

/// Legacy manifests without `lifetime` migrate to Worktree (when in a linked
/// worktree with an indexed main repo), Keep (when embedded), or Days(30) with
/// full grace (when lexical-only).
#[test]
fn migration_classifies_legacy_manifests_correctly() {
    let fixture = Fixture::new();
    let main_repo = fixture.code_project();
    init_git_repo(&main_repo);
    let wt_dir = fixture.base.join("wt/legacy-wt");
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-b",
            "legacy-branch",
            wt_dir.to_str().unwrap(),
            "HEAD",
        ],
    );
    // Make an unmerged commit in wt_dir so sweep migrates it without removing it as merged.
    write(&wt_dir.join("src/unmerged.rs"), "pub fn unmerged() {}\n");
    git(&wt_dir, &["add", "."]);
    git(&wt_dir, &["commit", "-m", "Branch work"]);

    let lex_repo = fixture.base.join("lex-only");
    write(&lex_repo.join("lib.rs"), "pub fn lex() {}\n");

    let indexes = fixture.open(&main_repo, Backend::Hash, false, fast(false));
    indexes.set_active_day(40);

    // Create three indexes and strip their `lifetime` and `created_day` to simulate legacy manifests.
    let main_out = indexes
        .create_blocking(&CreateArgs {
            name: Some("proj".into()),
            embed: Some(true),
            ..create_args(&main_repo, Some(Kind::Code))
        })
        .unwrap();
    let lex_out = indexes
        .create_blocking(&CreateArgs {
            name: Some("lex-only".into()),
            embed: Some(false),
            ..create_args(&lex_repo, Some(Kind::Code))
        })
        .unwrap();
    let wt_out = indexes
        .create_blocking(&CreateArgs {
            name: Some("legacy-wt".into()),
            embed: Some(true),
            monitor: true,
            ..create_args(&wt_dir, Some(Kind::Code))
        })
        .unwrap();

    for layout in [&main_out.layout, &lex_out.layout, &wt_out.layout] {
        let mut m = store::load_manifest(&layout.dir).unwrap();
        m.lifetime = None;
        m.parent_index = None;
        m.created_day = 0;
        m.lifetime_changed_day = 0;
        store::save_manifest(&layout.dir, &m).unwrap();
    }

    let report = indexes.sweep_once();
    assert_eq!(report.migrated, 3, "{report:?}");
    assert!(report.removed_deleted.is_empty());
    assert!(report.removed_merged.is_empty());
    assert!(report.removed_expired.is_empty());

    let main_m = store::load_manifest(&main_out.layout.dir).unwrap();
    assert_eq!(main_m.lifetime, Some(Lifetime::Keep));

    let lex_m = store::load_manifest(&lex_out.layout.dir).unwrap();
    assert_eq!(lex_m.lifetime, Some(Lifetime::Days(30)));
    assert_eq!(
        lex_m.created_day, 40,
        "gets full 30-day grace from migration day"
    );

    let wt_m = store::load_manifest(&wt_out.layout.dir).unwrap();
    assert_eq!(
        wt_m.lifetime,
        Some(Lifetime::Worktree {
            main: "proj".into()
        })
    );
    assert_eq!(
        wt_m.parent_index.as_deref(),
        Some(main_out.layout.id().as_str())
    );
    assert!(!wt_m.monitor, "migrated worktree index disables monitoring");

    fixture.shutdown(&indexes);
}

/// Sweep removes deleted folders (only when parent exists), merged+clean
/// worktrees, and expired `Days`/`Worktree` indexes; spares unmounted drives,
/// `Keep` indexes, dirty/unmerged worktrees, and locked indexes.
#[test]
fn sweep_applies_all_cleanup_and_safety_rules() {
    let fixture = Fixture::new();
    let main_repo = fixture.code_project();
    init_git_repo(&main_repo);

    // Worktree 1: branch with a commit that gets merged into main (clean) -> REMOVED.
    let wt_merged = fixture.base.join("wt/merged");
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-b",
            "br-merged",
            wt_merged.to_str().unwrap(),
            "HEAD",
        ],
    );
    write(&wt_merged.join("src/m.rs"), "pub fn merged_fn() {}\n");
    git(&wt_merged, &["add", "."]);
    git(&wt_merged, &["commit", "-m", "Merged work"]);

    // Worktree 2: merged commit, BUT has an uncommitted file -> SPARED.
    let wt_dirty = fixture.base.join("wt/dirty");
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-b",
            "br-dirty",
            wt_dirty.to_str().unwrap(),
            "HEAD",
        ],
    );
    write(&wt_dirty.join("src/untracked.rs"), "pub fn dirty_fn() {}\n");

    // Worktree 3: unmerged commit (clean) -> SPARED until 30 active days pass.
    let wt_unmerged = fixture.base.join("wt/unmerged");
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-b",
            "br-unmerged",
            wt_unmerged.to_str().unwrap(),
            "HEAD",
        ],
    );
    write(&wt_unmerged.join("src/u.rs"), "pub fn unmerged_fn() {}\n");
    git(&wt_unmerged, &["add", "."]);
    git(&wt_unmerged, &["commit", "-m", "Unmerged work"]);

    // Folder that will be deleted while parent exists -> REMOVED.
    let deleted_dir = fixture.base.join("deleted-proj");
    write(&deleted_dir.join("lib.rs"), "pub fn gone() {}\n");

    // Folder on simulated unmounted drive (parent will also disappear) -> SPARED.
    let unmounted_dir = fixture.base.join("ext-drive/repos/usb-proj");
    write(&unmounted_dir.join("lib.rs"), "pub fn usb() {}\n");

    // Expiring Days(7) project.
    let expiring_dir = fixture.base.join("temp-7d");
    write(&expiring_dir.join("lib.rs"), "pub fn temp7() {}\n");

    // Locked expiring Days(7) project -> SKIPPED while locked.
    let locked_dir = fixture.base.join("locked-7d");
    write(&locked_dir.join("lib.rs"), "pub fn locked7() {}\n");

    let indexes = fixture.open(&main_repo, Backend::Hash, false, fast(false));
    indexes.set_active_day(10);

    indexes
        .create_blocking(&CreateArgs {
            name: Some("proj".into()),
            lifetime: Some(Lifetime::Keep),
            ..create_args(&main_repo, Some(Kind::Code))
        })
        .unwrap();
    indexes
        .create_blocking(&CreateArgs {
            name: Some("deleted-proj".into()),
            lifetime: Some(Lifetime::Keep),
            embed: Some(false),
            ..create_args(&deleted_dir, Some(Kind::Code))
        })
        .unwrap();
    indexes
        .create_blocking(&CreateArgs {
            name: Some("usb-proj".into()),
            lifetime: Some(Lifetime::Keep),
            embed: Some(false),
            ..create_args(&unmounted_dir, Some(Kind::Code))
        })
        .unwrap();
    indexes
        .create_blocking(&CreateArgs {
            name: Some("temp-7d".into()),
            lifetime: Some(Lifetime::Days(7)),
            embed: Some(false),
            ..create_args(&expiring_dir, Some(Kind::Code))
        })
        .unwrap();
    let locked_out = indexes
        .create_blocking(&CreateArgs {
            name: Some("locked-7d".into()),
            lifetime: Some(Lifetime::Days(7)),
            embed: Some(false),
            ..create_args(&locked_dir, Some(Kind::Code))
        })
        .unwrap();

    // Auto-create all 3 worktree indexes via search.
    for wt in [&wt_merged, &wt_dirty, &wt_unmerged] {
        indexes
            .search_blocking(&search_args(wt, "retry", Some(ModeArg::Lexical)))
            .unwrap();
    }

    // Merge `br-merged` into `main` so `wt_merged` becomes merged and clean.
    git(
        &main_repo,
        &["merge", "--no-ff", "br-merged", "-m", "Merge br-merged"],
    );
    // Delete `deleted_dir` (parent `fixture.base` still exists).
    std::fs::remove_dir_all(&deleted_dir).unwrap();
    // Unmount simulated drive (parent `ext-drive/repos` also gone!).
    std::fs::remove_dir_all(fixture.base.join("ext-drive")).unwrap();

    // Sweep on day 16 (6 active days elapsed):
    // - `deleted-proj` removed (deleted folder)
    // - `proj@br-merged` removed (merged + clean)
    // - `usb-proj`, `proj@br-dirty`, `proj@br-unmerged`, `temp-7d`, `locked-7d`, `proj` SPARED!
    indexes.set_active_day(16);
    let r1 = indexes.sweep_once();
    assert_eq!(r1.removed_deleted, vec!["deleted-proj"]);
    assert_eq!(r1.removed_merged, vec!["proj@br-merged"]);
    assert!(r1.removed_expired.is_empty(), "{r1:?}");
    assert!(indexes.resolve("usb-proj", &main_repo).is_ok());
    assert!(indexes.resolve("proj@br-dirty", &main_repo).is_ok());
    assert!(indexes.resolve("proj@br-unmerged", &main_repo).is_ok());
    assert!(indexes.resolve("temp-7d", &main_repo).is_ok());
    assert!(indexes.resolve("locked-7d", &main_repo).is_ok());

    // Hold `.index.lock` exclusively on `locked-7d`, then advance to day 17 (7 active days elapsed).
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(locked_out.layout.state().join(".index.lock"))
        .unwrap();
    lock_file.lock().unwrap();

    indexes.set_active_day(17);
    let r2 = indexes.sweep_once();
    assert_eq!(r2.removed_expired, vec!["temp-7d"], "{r2:?}");
    assert_eq!(r2.skipped_locked, vec!["locked-7d"], "{r2:?}");
    assert!(indexes.resolve("locked-7d", &main_repo).is_ok());
    drop(lock_file);

    // Once unlocked, `locked-7d` is removed; advancing to day 40 (30 active days elapsed)
    // expires even the unmerged & dirty worktrees, while `Keep` indexes (`proj`, `usb-proj`) stay forever!
    indexes.set_active_day(40);
    let r3 = indexes.sweep_once();
    assert!(
        r3.removed_expired.contains(&"locked-7d".to_string()),
        "{r3:?}"
    );
    assert!(
        r3.removed_expired.contains(&"proj@br-unmerged".to_string()),
        "{r3:?}"
    );
    assert!(
        r3.removed_expired.contains(&"proj@br-dirty".to_string()),
        "{r3:?}"
    );
    assert!(indexes.resolve("proj", &main_repo).is_ok());
    assert!(indexes.resolve("usb-proj", &main_repo).is_ok());

    fixture.shutdown(&indexes);
}

/// `Indexes::open` starts the background startup sweep thread (removing orphans
/// off the caller's thread) and `shutdown()` wakes and joins it promptly.
#[test]
fn background_sweep_runs_at_open_and_shuts_down_promptly() {
    let fixture = Fixture::new();
    let gone = fixture.base.join("orphan-proj");
    write(&gone.join("lib.rs"), "pub fn orphan() {}\n");

    let setup = fixture.open(&fixture.base, Backend::Hash, false, fast(false));
    setup
        .create_blocking(&CreateArgs {
            name: Some("orphan-proj".into()),
            lifetime: Some(Lifetime::Keep),
            embed: Some(false),
            ..create_args(&gone, Some(Kind::Code))
        })
        .unwrap();
    fixture.shutdown(&setup);

    std::fs::remove_dir_all(&gone).unwrap();

    // `Indexes::open` spawns the background sweep thread.
    let live = {
        let _guard = fixture.runtime.enter();
        Indexes::open(&fixture.data(), &fixture.base, None, false).unwrap()
    };
    wait_for("startup sweep to remove orphan", || live.all().is_empty());

    let started_shutdown = Instant::now();
    fixture.shutdown(&live);
    assert!(
        started_shutdown.elapsed() < Duration::from_secs(3),
        "shutdown should wake sweep thread immediately"
    );
}

/// CLI `create --lifetime` and `lifetime` subcommand work cleanly.
#[test]
fn cli_lifetime_defaults_and_changes() {
    let fixture = Fixture::new();
    let root = fixture.code_project();
    let id = store::index_id(&root, Kind::Code);

    // Default `arko index create` sets lifetime to `keep`.
    assert_eq!(
        cli(&fixture, &root, &["create", "--no-embed", "--no-monitor"]),
        Ok(ExitCode::SUCCESS)
    );
    let m = store::load_manifest(&store::indexes_dir(&fixture.data()).join(&id)).unwrap();
    assert_eq!(m.lifetime, Some(Lifetime::Keep));

    // `arko index lifetime proj 14` changes it to 14 days.
    assert_eq!(
        cli(&fixture, &root, &["lifetime", "proj", "14"]),
        Ok(ExitCode::SUCCESS)
    );
    let m = store::load_manifest(&store::indexes_dir(&fixture.data()).join(&id)).unwrap();
    assert_eq!(m.lifetime, Some(Lifetime::Days(14)));

    // `arko index create --lifetime 7` sets explicit lifetime.
    assert_eq!(
        cli(
            &fixture,
            &root,
            &["create", "--no-embed", "--no-monitor", "--lifetime", "7"]
        ),
        Ok(ExitCode::SUCCESS)
    );
    let m = store::load_manifest(&store::indexes_dir(&fixture.data()).join(&id)).unwrap();
    assert_eq!(m.lifetime, Some(Lifetime::Days(7)));

    // `arko index remove proj` works directly even on a kept index.
    assert_eq!(
        cli(&fixture, &root, &["lifetime", "proj", "keep"]),
        Ok(ExitCode::SUCCESS)
    );
    assert_eq!(
        cli(&fixture, &root, &["remove", "proj"]),
        Ok(ExitCode::SUCCESS)
    );
    assert!(!store::indexes_dir(&fixture.data()).join(&id).exists());
}

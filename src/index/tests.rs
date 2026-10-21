//! Tests of the index module: offline, without a Voyage key. Dense and hybrid
//! search run through the same code as Voyage with arko_index's `HashEmbedder`
//! (`Backend::Hash`); a live Voyage test is `#[ignore]`d.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arko_agents::{TaskContext, ToolRecovery};
use arko_index::hybrid::Mode;
use clap::Parser;
use serde_json::{Value, json};

use super::census::{self, Census, Class};
use super::cli::{IndexCommand, run_command_with};
use super::embed::{self, Backend};
use super::engine::{
    Action, CreateArgs, CreateOutcome, EmbedPlan, Hit, Indexes, ModeArg, SearchArgs, SearchOutput,
    Timing, WatchState,
};
use super::store::{self, Kind, Layout, Manifest, Rules};
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
/// attempts run out; the delay doubles after each failure.
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

    fn shutdown(&self, indexes: &Indexes) {
        self.runtime.block_on(indexes.shutdown());
    }
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
        wait: true,
        defer_embed: false,
    }
}

fn search_args(cwd: &Path, query: &str, mode: Option<ModeArg>) -> SearchArgs {
    SearchArgs {
        query: query.into(),
        index: None,
        mode,
        limit: 10,
        path_prefix: None,
        language: None,
        cwd: cwd.to_path_buf(),
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

    // A newer Arko's manifest: unknown fields, missing ones.
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

/// The indexes the agents' instructions name: where each lies from the
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
    // The ignored folder isn't counted; neither is .gitignore as code or prose.
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

/// An app reads the workspace's indexes as data: the ones the banner
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
    // The same tree as docs: another index, another name.
    let docs = indexes
        .create_blocking(&create_args(&root, Some(Kind::Docs)))
        .unwrap();
    assert_eq!(docs.manifest.name, "proj-docs");
    assert_eq!(docs.manifest.model, "voyage-4");
    // Again: updated in place.
    let again = CreateArgs {
        monitor: false,
        ..create_args(&root, Some(Kind::Code))
    };
    let again = indexes.create_blocking(&again).unwrap();
    assert!(again.updated);
    assert_eq!(again.manifest.name, "proj");
    assert!(!again.manifest.monitor);
    assert_eq!(indexes.all().len(), 2);

    // Two indexes cover the directory: a search uses both.
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
    assert!(manifest.last_build.is_some() && manifest.last_error.is_none());

    for (mode, expected) in [
        (Some(ModeArg::Semantic), Mode::Dense),
        (Some(ModeArg::Hybrid), Mode::Hybrid),
        (None, Mode::Hybrid),
        (Some(ModeArg::Lexical), Mode::Lexical),
    ] {
        let output = indexes
            .search_blocking(&search_args(&root, "retry backoff delay", mode))
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

    // Filters: a path prefix (relative to the search's directory) and a language.
    let mut args = search_args(&root.join("src"), "retry", Some(ModeArg::Hybrid));
    args.path_prefix = Some("config.rs".into());
    let output = indexes.search_blocking(&args).unwrap();
    assert!(
        output.hits.iter().all(|hit| hit.display == "config.rs"),
        "{output:?}"
    );
    let mut args = search_args(&root, "proj helpers", Some(ModeArg::Lexical));
    args.language = Some("Markdown".into());
    let output = indexes.search_blocking(&args).unwrap();
    assert!(!output.hits.is_empty());
    assert!(
        output.hits.iter().all(|hit| hit.display == "README.md"),
        "{output:?}"
    );
    // A language that matches nothing gives the results from every
    // language, and says so.
    let mut args = search_args(&root, "retry backoff delay", Some(ModeArg::Lexical));
    args.language = Some("python".into());
    let output = indexes.search_blocking(&args).unwrap();
    assert_eq!(hit_files(&output)[0], "src/net.rs", "{output:?}");
    assert_eq!(
        output.notes,
        ["nothing matched in python, so these results are from every language"]
    );
    let mut args = search_args(&root, "zzqqxx", Some(ModeArg::Lexical));
    args.language = Some("python".into());
    let output = indexes.search_blocking(&args).unwrap();
    assert!(
        output.hits.is_empty() && output.notes.is_empty(),
        "{output:?}"
    );
    let mut args = search_args(&root, "retry", None);
    args.path_prefix = Some(
        fixture
            .base
            .join("elsewhere")
            .to_string_lossy()
            .into_owned(),
    );
    // Without an index named, the path picks the indexes: there are none.
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
    // Nothing changed: no rebuild.
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
    assert!(tools::format_status(&reports, None).contains("another Arko process"));

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

/// An index made in fin's data folder (Arko's name before) opens, searches
/// and keeps current after the folder moves to Arko's: nothing saved in it
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
    for (mode, searched) in [
        (ModeArg::Semantic, Mode::Dense),
        (ModeArg::Hybrid, Mode::Hybrid),
        (ModeArg::Lexical, Mode::Lexical),
    ] {
        let output = second
            .search_blocking(&search_args(&root, "retry backoff delay", Some(mode)))
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
    // Arko's data folder inside the indexed tree, not ignored: its manifests
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
    assert_eq!(
        tools::parse_detail(&params(json!({}))).unwrap(),
        tools::Detail::Full
    );
    assert_eq!(detail(json!(" Brief ")).unwrap(), tools::Detail::Brief(3));
    assert_eq!(detail(json!("full")).unwrap(), tools::Detail::Full);
    assert!(detail(json!("everything")).is_err());

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
    assert_eq!(search.limit, 50);
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
    assert_eq!(search.limit, 1);
    assert_eq!(search.mode, Some(ModeArg::Semantic));
    assert_eq!(search.index.as_deref(), Some("arko"));
    assert_eq!(search.path_prefix.as_deref(), Some("src/"));
    assert_eq!(search.language.as_deref(), Some("rust"));
    let search =
        tools::parse_search(&params(json!({"query": "q", "mode": "lexical"})), cwd).unwrap();
    assert_eq!(search.limit, 10);
    assert_eq!(search.mode, Some(ModeArg::Lexical));
    // The agent picks the mode every time.
    assert!(tools::parse_search(&params(json!({"query": "q"})), cwd).is_err());
    assert!(tools::parse_search(&params(json!({"query": "q", "mode": "fuzzy"})), cwd).is_err());
    assert!(tools::parse_search(&params(json!({"query": "  ", "mode": "hybrid"})), cwd).is_err());

    let create = tools::parse_create(&params(json!({})), cwd).unwrap();
    assert_eq!(create.path, PathBuf::from("."));
    assert_eq!(create.kind, None);
    assert!(create.monitor && !create.wait && !create.defer_embed);
    assert_eq!(create.embed, None);
    let create = tools::parse_create(
        &params(json!({"path": "docs", "kind": "docs", "monitor": "false", "embed": false,
                       "max_embed_tokens": "1M", "include": "*.md", "wait": true, "name": "handbook"})),
        cwd,
    )
    .unwrap();
    assert_eq!(create.kind, Some(Kind::Docs));
    assert!(!create.monitor && create.wait);
    assert_eq!(create.embed, Some(false));
    assert_eq!(create.max_embed_tokens, Some(1_000_000));
    assert_eq!(create.include, vec!["*.md"]);
    assert_eq!(create.name.as_deref(), Some("handbook"));
    assert!(tools::parse_create(&params(json!({"kind": "poetry"})), cwd).is_err());

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
        json!({"path": ".", "kind": "auto", "wait": "true"}),
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
        scope: "fn retry".into(),
        symbol: "retry".into(),
        changed: true,
    };
    let output = SearchOutput {
        hits: vec![hit.clone()],
        searched: vec![("proj".into(), Kind::Code, Mode::Hybrid)],
        notes: vec!["a note".into()],
        query_tokens: 3,
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
    let none = SearchOutput {
        searched: vec![("proj".into(), Kind::Code, Mode::Dense)],
        ..SearchOutput::default()
    };
    let text = tools::format_search("zz", &none, 3);
    assert!(
        text.starts_with("No results for \"zz\" in proj (code, semantic)."),
        "{text}"
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
/// Face once); no key needed.
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

/// A live index with Voyage: run with `VOYAGE_API_KEY=... cargo test -- --ignored
/// voyage_live` (embeds a few hundred tokens: well under a cent).
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
        scope: chunk.scope.clone(),
        symbol: chunk.symbol_path.clone(),
        changed: false,
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
    // `big` is split into several chunks; `small` is one.
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
    // Its closing brace: the blank line after it isn't shown.
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
    // `huge` is too long to show whole: the part that matched.
    let huge_lines = huge.last().unwrap().end_line + 1 - huge[0].start_line;
    assert!(huge_lines > 200);
    assert!(
        text.contains(&format!(
            "-line definition at lines {}-",
            huge[0].start_line
        )) && text.contains("read_file shows the rest"),
        "{text}"
    );

    // Brief: the locations and snippets of before.
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

    // The file changed since it was indexed: found by its symbol.
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

    // An Arko working elsewhere: the index isn't its workspace's.
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

    // No index covers the working directory: a path_prefix picks one.
    let mut args = search_args(&work, "retry backoff", Some(ModeArg::Lexical));
    let error = indexes.search_blocking(&args).unwrap_err();
    assert!(error.contains("no index covers"), "{error}");
    args.path_prefix = Some("../proj/src".into());
    let output = indexes.search_blocking(&args).unwrap();
    let net = root.join("src/net.rs").to_string_lossy().into_owned();
    assert_eq!(hit_files(&output)[0], net, "{output:?}");
    let mut args = search_args(&work, "retry backoff", Some(ModeArg::Lexical));
    args.index = Some(net.clone());
    let output = indexes.search_blocking(&args).unwrap();
    assert_eq!(hit_files(&output)[0], net, "{output:?}");

    // Once searched, a monitored index is kept current.
    wait_for("the watcher", || {
        indexes.watch_state(&outcome.layout).active()
    });
    fixture.shutdown(&indexes);
}

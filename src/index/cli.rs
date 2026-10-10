//! `fin index ...`: the indexes from the command line.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use arko_index::dense::{EmbedProgress, StopSignal};

use super::embed::{self, Backend};
use super::engine::{
    self, Action, CreateArgs, EmbedSummary, Events, Found, Indexes, ModeArg, SearchArgs, Timing,
    fmt_count, fmt_dollars,
};
use super::tools::{self, format_create, format_search, format_status};

/// `fin index` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum IndexCommand {
    /// Create (or update) the index of a directory: build it, estimate and run its embedding
    Create {
        /// The directory to index (default: the current directory)
        path: Option<PathBuf>,
        /// code (voyage-code-4), docs (voyage-4), or auto (by the files)
        #[arg(long, default_value = "auto")]
        kind: String,
        /// The index's name (default: the directory's name)
        #[arg(long)]
        name: Option<String>,
        /// Only files matching these gitignore-style patterns (comma-separated, repeatable)
        #[arg(long, value_delimiter = ',')]
        include: Vec<String>,
        /// Leave out files matching these gitignore-style patterns (comma-separated, repeatable)
        #[arg(long, value_delimiter = ',')]
        exclude: Vec<String>,
        /// Don't keep the index current while fin runs
        #[arg(long)]
        no_monitor: bool,
        /// Lexical only: no embeddings
        #[arg(long)]
        no_embed: bool,
        /// Token cap of each embedding run (default 5M; 200k, 5M, 5000000)
        #[arg(long, value_parser = count_arg)]
        max_embed_tokens: Option<u64>,
        /// Build and estimate only: embed later (fin's monitoring or `fin index embed`)
        #[arg(long)]
        no_wait: bool,
    },
    /// List the indexes
    List,
    /// Show the state of an index (default: all)
    Status {
        /// An index name or root path
        index: Option<String>,
    },
    /// Search the indexes of the current directory (or --index)
    Search {
        /// The query
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// An index name or root path
        #[arg(long, short)]
        index: Option<String>,
        /// hybrid, semantic or lexical (default: hybrid when the index has embeddings)
        #[arg(long, short)]
        mode: Option<String>,
        /// Results, 1-50
        #[arg(long, short = 'n', default_value_t = engine::DEFAULT_LIMIT)]
        limit: usize,
        /// Only results under this path (relative to the current directory)
        #[arg(long)]
        path_prefix: Option<String>,
        /// Only chunks of this language (rust, python, markdown, ...)
        #[arg(long)]
        language: Option<String>,
        /// Snippet lines per result
        #[arg(long, default_value_t = tools::SNIPPET_LINES)]
        lines: usize,
    },
    /// Bring an index up to date with its files: rebuild what changed, embed it
    Update {
        /// An index name or root path (default: the current directory's)
        index: Option<String>,
        /// Rebuild even when no file changed
        #[arg(long)]
        force: bool,
    },
    /// Embed the chunks without vectors (or only --estimate the cost)
    Embed {
        /// An index name or root path (default: the current directory's)
        index: Option<String>,
        /// The run's token cap (default: the index's)
        #[arg(long, value_parser = count_arg)]
        max_tokens: Option<u64>,
        /// Only estimate (free)
        #[arg(long)]
        estimate: bool,
    },
    /// Keep indexes current until Ctrl-C (default: the monitored indexes of the current directory)
    Watch {
        /// Index names or root paths
        indexes: Vec<String>,
        /// Every monitored index
        #[arg(long)]
        all: bool,
    },
    /// Turn an index's monitoring on or off
    Monitor {
        /// An index name or root path
        index: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Delete an index (the indexed files are untouched)
    Remove {
        /// An index name or root path
        index: String,
    },
}

fn count_arg(text: &str) -> Result<u64, String> {
    tools::parse_count(text)
        .ok_or_else(|| format!("{text:?} is not a token count (200k, 5M, 5000000)"))
}

/// Runs a `fin index` command: `data_root` is fin's data folder, `voyage_key`
/// the key fin found (none: lexical only). Call it in fin's tokio runtime.
pub async fn run_command(
    cmd: IndexCommand,
    data_root: &Path,
    voyage_key: Option<String>,
) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|error| format!("the current directory: {error}"))?;
    run_command_with(
        cmd,
        data_root,
        &cwd,
        Backend::voyage(voyage_key),
        Timing::default(),
    )
    .await
}

pub(crate) async fn run_command_with(
    cmd: IndexCommand,
    data_root: &Path,
    cwd: &Path,
    backend: Backend,
    timing: Timing,
) -> Result<ExitCode, String> {
    let monitor = matches!(cmd, IndexCommand::Watch { .. });
    let events: Events = Arc::new(|line: &str| eprintln!("{line}"));
    let indexes = Indexes::open_with(data_root, cwd, backend, monitor, timing, Some(events))?;
    let result = run(&indexes, cmd, cwd).await;
    indexes.shutdown().await;
    result.map_err(|error| cli_text(&error))
}

/// The text the tools share, with the tools' hints turned into commands.
fn cli_text(text: &str) -> String {
    [
        ("index_manage action=embed", "`fin index embed`"),
        ("index_manage action=rebuild", "`fin index update --force`"),
        ("index_status", "`fin index status`"),
        (
            "index_create (or `fin index create PATH`)",
            "`fin index create PATH`",
        ),
        ("index_create", "`fin index create PATH`"),
    ]
    .iter()
    .fold(text.to_string(), |text, (tool, command)| {
        text.replace(tool, command)
    })
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| format!("index work failed: {error}"))?
}

/// The one index `index` names, or the current directory's.
fn one(indexes: &Indexes, index: Option<&str>, cwd: &Path) -> Result<Found, String> {
    let mut targets = indexes.targets(index, cwd)?;
    if targets.len() > 1 {
        let names: Vec<&str> = targets
            .iter()
            .map(|found| found.manifest.name.as_str())
            .collect();
        return Err(format!(
            "several indexes cover {}: {}; name one",
            cwd.display(),
            names.join(", ")
        ));
    }
    Ok(targets.remove(0))
}

async fn run(indexes: &Arc<Indexes>, cmd: IndexCommand, cwd: &Path) -> Result<ExitCode, String> {
    match cmd {
        IndexCommand::Create {
            path,
            kind,
            name,
            include,
            exclude,
            no_monitor,
            no_embed,
            max_embed_tokens,
            no_wait,
        } => {
            let args = CreateArgs {
                path: path.unwrap_or_else(|| PathBuf::from(".")),
                cwd: cwd.to_path_buf(),
                kind: tools::parse_kind(Some(&kind))?,
                name,
                include,
                exclude,
                monitor: !no_monitor,
                embed: no_embed.then_some(false),
                max_embed_tokens,
                wait: false,
                defer_embed: true,
            };
            eprintln!("Building the index of {}…", args.path.display());
            let worker = Arc::clone(indexes);
            let outcome = blocking(move || worker.create_blocking(&args)).await?;
            println!("{}", cli_text(&format_create(&outcome)));
            let manifest = &outcome.manifest;
            if manifest.embed && indexes.has_key() && !no_wait {
                let found = Found {
                    layout: outcome.layout.clone(),
                    manifest: manifest.clone(),
                };
                let summary =
                    embed_with_progress(indexes, &found, manifest.max_embed_tokens).await?;
                println!("Embeddings ({}): {}.", manifest.model, summary.line());
            } else if manifest.embed && indexes.has_key() {
                println!(
                    "Embedding later: fin embeds while it monitors the index, or run `fin index embed`."
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        IndexCommand::List => {
            let all = indexes.all();
            if all.is_empty() {
                println!("No indexes yet: `fin index create PATH` makes one.");
            }
            let reports = {
                let worker = Arc::clone(indexes);
                let cwd = cwd.to_path_buf();
                blocking(move || worker.status_blocking(None, &cwd)).await?
            };
            for report in reports {
                match &report.manifest {
                    Ok(manifest) => {
                        let vectors = match (&report.dense, manifest.embed) {
                            (_, false) => "lexical".to_string(),
                            (Some(info), true) => format!(
                                "{}/{} embedded",
                                fmt_count(info.embedded),
                                fmt_count(info.docs)
                            ),
                            (None, true) => "not embedded".to_string(),
                        };
                        println!(
                            "{}\t{}\t{} chunks\t{}\t{}\t{}",
                            manifest.name,
                            manifest.kind.as_str(),
                            report.chunks.map(fmt_count).unwrap_or_else(|| "-".into()),
                            vectors,
                            if manifest.monitor { "monitored" } else { "-" },
                            manifest.root.display()
                        );
                    }
                    Err(error) => println!("{}\tunreadable: {error}", report.id),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        IndexCommand::Status { index } => {
            let worker = Arc::clone(indexes);
            let cwd = cwd.to_path_buf();
            let reports = blocking(move || worker.status_blocking(index.as_deref(), &cwd)).await?;
            println!("{}", cli_text(&format_status(&reports, None)));
            for report in &reports {
                println!("{} is stored in {}", report.id, report.dir.display());
            }
            if !indexes.has_key() {
                println!(
                    "({} is not set: semantic search is unavailable.)",
                    embed::KEY_NAME
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        IndexCommand::Search {
            query,
            index,
            mode,
            limit,
            path_prefix,
            language,
            lines,
        } => {
            let mode = match mode.as_deref() {
                None => None,
                Some(text) => Some(
                    ModeArg::parse(text)
                        .ok_or_else(|| format!("{text:?}: use hybrid, semantic or lexical"))?,
                ),
            };
            let args = SearchArgs {
                query: query.join(" "),
                index,
                mode,
                limit: limit.clamp(1, engine::MAX_LIMIT),
                path_prefix,
                language,
                cwd: cwd.to_path_buf(),
            };
            let text = args.query.clone();
            let worker = Arc::clone(indexes);
            let output = blocking(move || worker.search_blocking(&args)).await?;
            println!("{}", cli_text(&format_search(&text, &output, lines)));
            Ok(ExitCode::SUCCESS)
        }
        IndexCommand::Update { index, force } => {
            let found = one(indexes, index.as_deref(), cwd)?;
            let worker = Arc::clone(indexes);
            let target = found.clone();
            let built = blocking(move || {
                if force {
                    worker.rebuild_of(&target).map(Some)
                } else {
                    worker.update_of(&target)
                }
            })
            .await?;
            match built {
                Some(record) => println!(
                    "Rebuilt {}: {} chunks in {} files ({:.1} s); {}.",
                    found.manifest.name,
                    fmt_count(record.chunks),
                    fmt_count(record.files),
                    record.seconds,
                    record.summary
                ),
                None => println!("{} is up to date with its files.", found.manifest.name),
            }
            if found.manifest.embed && indexes.has_key() {
                let summary =
                    embed_with_progress(indexes, &found, found.manifest.max_embed_tokens).await?;
                if summary.new > 0 || summary.embedded < summary.docs {
                    println!("Embeddings: {}.", summary.line());
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        IndexCommand::Embed {
            index,
            max_tokens,
            estimate,
        } => {
            let found = one(indexes, index.as_deref(), cwd)?;
            if !estimate && !indexes.has_key() {
                return Err(format!(
                    "embedding needs {} (fin reads it from its .env; see fin --setup)",
                    embed::KEY_NAME
                ));
            }
            let worker = Arc::clone(indexes);
            let target = found.clone();
            let (manifest, record) = if estimate {
                (found.manifest.clone(), None)
            } else {
                blocking(move || worker.ensure_dense(&target)).await?
            };
            if let Some(record) = record {
                println!(
                    "Rebuilt {} with embeddings enabled ({} chunks).",
                    manifest.name,
                    fmt_count(record.chunks)
                );
            }
            let found = Found {
                layout: found.layout,
                manifest,
            };
            let worker = Arc::clone(indexes);
            let target = found.clone();
            match blocking(move || worker.estimate_of(&target)).await {
                Ok(estimate) => println!("Estimate: {}.", estimate.line()),
                Err(error) => println!("Estimate unavailable: {error}."),
            }
            if !estimate {
                let cap = max_tokens.unwrap_or(found.manifest.max_embed_tokens);
                let summary = embed_with_progress(indexes, &found, cap).await?;
                println!("{}: {}.", found.manifest.name, summary.line());
            }
            Ok(ExitCode::SUCCESS)
        }
        IndexCommand::Watch {
            indexes: names,
            all,
        } => {
            let targets: Vec<Found> = if all {
                indexes
                    .all()
                    .into_iter()
                    .filter(|found| found.manifest.monitor)
                    .collect()
            } else if names.is_empty() {
                indexes
                    .related(cwd)
                    .into_iter()
                    .filter(|found| found.manifest.monitor)
                    .collect()
            } else {
                names
                    .iter()
                    .map(|name| indexes.resolve(name, cwd))
                    .collect::<Result<_, _>>()?
            };
            if targets.is_empty() {
                return Err("no monitored index to watch here: name one, or use --all".into());
            }
            for found in &targets {
                indexes.watch_index(found);
            }
            eprintln!(
                "Watching {} index{} until Ctrl-C.",
                targets.len(),
                if targets.len() == 1 { "" } else { "es" }
            );
            tokio::signal::ctrl_c()
                .await
                .map_err(|error| format!("waiting for Ctrl-C: {error}"))?;
            eprintln!("Stopping…");
            let (tokens, dollars) = indexes.spent();
            if tokens > 0 {
                eprintln!(
                    "Embeddings this session: {} tokens ({}).",
                    fmt_count(tokens),
                    fmt_dollars(dollars)
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        IndexCommand::Monitor { index, state } => {
            let worker = Arc::clone(indexes);
            let cwd = cwd.to_path_buf();
            let on = state == "on";
            let text = blocking(move || worker.manage_blocking(&index, &Action::Monitor(on), &cwd))
                .await?;
            println!("{}", cli_text(&text));
            Ok(ExitCode::SUCCESS)
        }
        IndexCommand::Remove { index } => {
            let worker = Arc::clone(indexes);
            let cwd = cwd.to_path_buf();
            let text =
                blocking(move || worker.manage_blocking(&index, &Action::Remove, &cwd)).await?;
            println!("{}", cli_text(&text));
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// An embedding run in the foreground: progress on stderr, Ctrl-C stops it after
/// its current request (the vectors paid for are kept).
async fn embed_with_progress(
    indexes: &Arc<Indexes>,
    found: &Found,
    cap: u64,
) -> Result<EmbedSummary, String> {
    let stop = Arc::new(StopSignal::new());
    let interrupt = {
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\nStopping after the current request…");
                stop.stop("interrupted (Ctrl-C)");
            }
        })
    };
    let worker = Arc::clone(indexes);
    let target = found.clone();
    let model = found.manifest.model.clone();
    let result = blocking(move || {
        let progress = move |update: &EmbedProgress| {
            eprint!(
                "\rembedding: {}/{} texts, {} tokens ({}); {}/{} chunks have vectors   ",
                fmt_count(update.texts),
                fmt_count(update.total),
                fmt_count(update.tokens),
                fmt_dollars(embed::cost(&model, update.tokens)),
                fmt_count(update.embedded_docs),
                fmt_count(update.docs)
            );
            let _ = std::io::stderr().flush();
        };
        worker.embed_foreground(&target, cap, &progress, stop)
    })
    .await;
    interrupt.abort();
    eprintln!();
    result
}

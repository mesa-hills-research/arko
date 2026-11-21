//! `arko index ...` covers the indexes from the command line.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use arko_index::dense::{EmbedProgress, StopSignal};

use super::embed::{self, Backend};
use super::engine::{
    Action, CreateArgs, EmbedSummary, Events, Found, Indexes, ModeArg, SearchArgs, SearchOutput,
    StatusScope, Timing, chunks_in_files, fmt_count, fmt_counted, fmt_dollars,
};
use super::store::Lifetime;
use super::tools::{self, format_create, format_status_output};

/// `arko index` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum IndexCommand {
    /// Create (or update) the index of a directory, build it, and estimate and run its embedding
    Create {
        /// The directory to index (by default the current directory)
        path: Option<PathBuf>,
        /// code (voyage-code-4), docs (voyage-4), or auto (by the files)
        #[arg(long, default_value = "auto")]
        kind: String,
        /// The index's name (by default the directory's name)
        #[arg(long)]
        name: Option<String>,
        /// Only files matching these gitignore-style patterns (comma-separated, repeatable)
        #[arg(long, value_delimiter = ',')]
        include: Vec<String>,
        /// Leave out files matching these gitignore-style patterns (comma-separated, repeatable)
        #[arg(long, value_delimiter = ',')]
        exclude: Vec<String>,
        /// Don't keep the index current while Arko runs
        #[arg(long)]
        no_monitor: bool,
        /// Lexical only, without embeddings
        #[arg(long)]
        no_embed: bool,
        /// Token cap of each embedding run, such as 200k, 5M or 5000000 (default 5M)
        #[arg(long, value_parser = count_arg)]
        max_embed_tokens: Option<u64>,
        /// How long to keep the index without a search: keep (default) or days of use (e.g. 7, 30)
        #[arg(long)]
        lifetime: Option<String>,
        /// Build and estimate only, and embed later (Arko's monitoring or `arko index embed`)
        #[arg(long)]
        no_wait: bool,
    },
    /// List the indexes
    List,
    /// Show the state of an index (all by default)
    Status {
        /// An index name or root path
        index: Option<String>,
    },
    /// Search the indexes of the current directory (or --index)
    Search {
        /// The query
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// An index name, or a directory or file inside an index
        #[arg(long, short)]
        index: Option<String>,
        /// hybrid, semantic or lexical (by default hybrid when the index has embeddings)
        #[arg(long, short)]
        mode: Option<String>,
        /// Results (by default 10, and 100 in lexical mode, or as
        /// ARKO_INDEX_LIMITS says), at most 50 in semantic and hybrid mode
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Every match of a lexical search, whatever --limit says
        #[arg(long)]
        exhaustive: bool,
        /// A lexical search lists the matches after this cursor, which the
        /// note of a cut list gives (src/a.rs:804:5). The note quotes it for
        /// POSIX shells and PowerShell
        #[arg(long, allow_hyphen_values = true)]
        after: Option<String>,
        /// A lexical search matches the query's ASCII letters in any case
        #[arg(long)]
        ignore_case: bool,
        /// Only results under this path (absolute, or relative to the current
        /// directory). Without --index, it picks the indexes too
        #[arg(long)]
        path_prefix: Option<String>,
        /// Only chunks of this language (rust, python, markdown, ...)
        #[arg(long)]
        language: Option<String>,
        /// Snippet lines per result (3 by default, and lexical search lists
        /// the lines that matched)
        #[arg(long)]
        lines: Option<usize>,
        /// Show the top results' whole definitions (functions, types, doc
        /// sections) with line numbers, as the agents get them
        #[arg(long, conflicts_with = "lines")]
        full: bool,
    },
    /// Bring an index up to date with its files, rebuilding what changed and embedding it
    Update {
        /// An index name or root path (by default the current directory's)
        index: Option<String>,
        /// Rebuild even when no file changed
        #[arg(long)]
        force: bool,
    },
    /// Embed the chunks without vectors (or only --estimate the cost)
    Embed {
        /// An index name or root path (by default the current directory's)
        index: Option<String>,
        /// The run's token cap (by default the index's)
        #[arg(long, value_parser = count_arg)]
        max_tokens: Option<u64>,
        /// Only estimate (free)
        #[arg(long)]
        estimate: bool,
    },
    /// Keep indexes current until Ctrl-C (by default the monitored indexes of the current directory)
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
    /// Change an index's lifetime (keep, or a positive number of days of use)
    Lifetime {
        /// An index name or root path
        index: String,
        /// keep, or a positive number of days of use (e.g. 7, 30)
        lifetime: String,
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

/// Runs an `arko index` command. `data_root` is Arko's data folder, `voyage_key`
/// the key Arko found (none means lexical only). Call it in Arko's tokio runtime.
pub async fn run_command(
    cmd: IndexCommand,
    data_root: &Path,
    voyage_key: Option<String>,
) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("could not read the current directory ({error})"))?;
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
    let events: Events = Arc::new(|line: &str| eprintln!("{}", crate::glyphs::render(line)));
    let indexes = Indexes::open_with(data_root, cwd, backend, monitor, timing, Some(events))?;
    let result = run(&indexes, cmd, cwd).await;
    indexes.shutdown().await;
    result.map_err(|error| cli_text(&error))
}

/// The text the tools share, with the tools' hints turned into commands.
pub(super) fn cli_text(text: &str) -> String {
    let text = [
        ("index_manage action=embed", "`arko index embed`"),
        ("index_manage action=rebuild", "`arko index update --force`"),
        ("index_manage action=lifetime", "`arko index lifetime`"),
        (
            "call index_status with index=\"all\"",
            "run `arko index status`",
        ),
        ("index_status", "`arko index status`"),
        (
            "index_create (or `arko index create PATH`)",
            "`arko index create PATH`",
        ),
        ("index_create", "`arko index create PATH`"),
        ("exhaustive=true", "--exhaustive"),
        ("ignore_case=true", "--ignore-case"),
        ("path_prefix narrows", "--path-prefix narrows"),
        ("hybrid finds", "--mode hybrid finds"),
    ]
    .iter()
    .fold(text.to_string(), |text, (tool, command)| {
        text.replace(tool, command)
    });
    crate::glyphs::render(&text).into_owned()
}

/// A search's result for the command, meaning its notes with the tools' hints
/// turned into commands, and the results as they are, since a matched line
/// may hold the same words.
pub(super) fn cli_search_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.starts_with("note: ") {
                cli_text(line)
            } else {
                crate::glyphs::render(line).into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A search's result as the command prints it. The note that gives the next
/// page is written for the shell, from the cursor, rather than from the
/// tools' words, which could change the cursor.
pub(super) fn search_text(query: &str, output: &SearchOutput, detail: tools::Detail) -> String {
    let mut text = cli_search_text(&tools::format_search_body(query, output, detail));
    if let Some(note) = tools::next_note(output, true) {
        text.push_str(&format!("\nnote: {note}"));
    }
    text
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| format!("index work failed ({error})"))?
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
            "several indexes cover {} ({}), so name one",
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
            lifetime,
            no_wait,
        } => {
            let lifetime = lifetime.as_deref().map(Lifetime::parse).transpose()?;
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
                lifetime,
                created_by_session: None,
                created_by_run: None,
                wait: false,
                defer_embed: true,
            };
            eprintln!(
                "{}",
                crate::glyphs::render(&format!("Building the index of {}…", args.path.display()))
            );
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
                println!(
                    "Embeddings by {}. {}.",
                    manifest.model,
                    crate::ui::sentence(&summary.line())
                );
            } else if manifest.embed && indexes.has_key() {
                println!(
                    "Embedding comes later, while Arko monitors the index, or with `arko index embed`."
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        IndexCommand::List => {
            let all = indexes.all();
            if all.is_empty() {
                println!("No indexes yet. `arko index create PATH` makes one.");
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
                        let life = match &report.lifetime {
                            Some(Lifetime::Keep) | None => "keep".to_string(),
                            Some(Lifetime::Days(days)) => format!("{days}d"),
                            Some(Lifetime::Worktree { .. }) => "worktree".to_string(),
                        };
                        println!(
                            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            manifest.name,
                            manifest.kind.as_str(),
                            life,
                            report.chunks.map_or_else(
                                || "- chunks".into(),
                                |chunks| fmt_counted(chunks, "chunk", "chunks")
                            ),
                            vectors,
                            if manifest.monitor { "monitored" } else { "-" },
                            manifest.root.display()
                        );
                    }
                    Err(error) => println!("{}\tunreadable ({error})", report.id),
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        IndexCommand::Status { index } => {
            let worker = Arc::clone(indexes);
            let cwd = cwd.to_path_buf();
            let output = blocking(move || {
                let scope = match index.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    None => StatusScope::All,
                    Some(name) if name.eq_ignore_ascii_case("all") => StatusScope::All,
                    Some(name) => StatusScope::One(name),
                };
                worker.status_for(scope, &cwd)
            })
            .await?;
            println!("{}", cli_text(&format_status_output(&output, None)));
            for report in &output.reports {
                println!("{} is stored in {}", report.id, report.dir.display());
            }
            if !indexes.has_key() {
                println!(
                    "({} is not set, so semantic search is unavailable.)",
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
            exhaustive,
            after,
            ignore_case,
            path_prefix,
            language,
            lines,
            full,
        } => {
            let mode = match mode.as_deref() {
                None => None,
                Some(text) => Some(ModeArg::parse(text).ok_or_else(|| {
                    format!("{text:?} isn't a search mode (use hybrid, semantic or lexical)")
                })?),
            };
            let args = SearchArgs {
                query: query.join(" "),
                index,
                mode,
                limit,
                exhaustive,
                after,
                path_prefix,
                language,
                ignore_case,
                cwd: cwd.to_path_buf(),
                created_by_session: None,
                created_by_run: None,
            };
            let text = args.query.clone();
            let worker = Arc::clone(indexes);
            let detail = match (full, lines) {
                (true, _) => Some(tools::Detail::Full),
                (false, Some(lines)) => Some(tools::Detail::Brief(lines)),
                // The agents' search, but with snippets for whole results.
                (false, None) if mode != Some(ModeArg::Lexical) => {
                    Some(tools::Detail::Brief(tools::SNIPPET_LINES))
                }
                (false, None) => None,
            };
            let detail = tools::detail_for(mode, detail);
            let output = blocking(move || worker.search_blocking(&args)).await?;
            println!("{}", search_text(&text, &output, detail));
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
                Some(record) => {
                    let borrowed = if record.borrowed > 0 {
                        format!(
                            " Borrowed {}.",
                            fmt_counted(record.borrowed, "cached vector", "cached vectors")
                        )
                    } else {
                        String::new()
                    };
                    println!(
                        "Rebuilt {} with {} ({:.1} s). {}.{borrowed}",
                        found.manifest.name,
                        chunks_in_files(record.chunks, record.files),
                        record.seconds,
                        record.summary
                    );
                }
                None => println!("{} is up to date with its files.", found.manifest.name),
            }
            if found.manifest.embed && indexes.has_key() {
                let summary =
                    embed_with_progress(indexes, &found, found.manifest.max_embed_tokens).await?;
                if summary.new > 0 || summary.borrowed > 0 || summary.embedded < summary.docs {
                    println!("{}.", crate::ui::sentence(&summary.line()));
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
                    "embedding needs {}, which Arko reads from its .env (see arko --setup)",
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
                    "Rebuilt {} with embeddings enabled ({}).",
                    manifest.name,
                    fmt_counted(record.chunks, "chunk", "chunks")
                );
            }
            let found = Found {
                layout: found.layout,
                manifest,
            };
            let worker = Arc::clone(indexes);
            let target = found.clone();
            match blocking(move || worker.estimate_of(&target)).await {
                Ok(estimate) => println!("{}.", crate::ui::sentence(&estimate.line())),
                Err(error) => println!("No estimate ({error})."),
            }
            if !estimate {
                let cap = max_tokens.unwrap_or(found.manifest.max_embed_tokens);
                let summary = embed_with_progress(indexes, &found, cap).await?;
                println!("{} {}.", found.manifest.name, summary.line());
            }
            Ok(ExitCode::SUCCESS)
        }
        IndexCommand::Watch {
            indexes: names,
            all,
        } => {
            let all_indexes = indexes.all();
            let targets: Vec<Found> = if all {
                all_indexes
                    .iter()
                    .filter(|found| {
                        found.manifest.monitor
                            && !indexes
                                .effective_lifetime(found, &all_indexes)
                                .is_worktree()
                    })
                    .cloned()
                    .collect()
            } else if names.is_empty() {
                indexes
                    .related(cwd)
                    .into_iter()
                    .filter(|found| {
                        found.manifest.monitor
                            && !indexes
                                .effective_lifetime(found, &all_indexes)
                                .is_worktree()
                    })
                    .collect()
            } else {
                names
                    .iter()
                    .map(|name| indexes.resolve(name, cwd))
                    .collect::<Result<_, _>>()?
            };
            if targets.is_empty() {
                return Err("no monitored index to watch here, so name one, or use --all".into());
            }
            for found in &targets {
                indexes.watch_index(found);
            }
            eprintln!(
                "Watching {} until Ctrl-C.",
                crate::remote::plural(targets.len(), "index", "indexes")
            );
            tokio::signal::ctrl_c()
                .await
                .map_err(|error| format!("could not wait for Ctrl-C ({error})"))?;
            eprintln!("{}", crate::glyphs::render("Stopping…"));
            let (tokens, dollars) = indexes.spent();
            if tokens > 0 {
                eprintln!(
                    "This session embedded {} tokens ({}).",
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
        IndexCommand::Lifetime { index, lifetime } => {
            let lifetime = Lifetime::parse(&lifetime)?;
            let worker = Arc::clone(indexes);
            let cwd = cwd.to_path_buf();
            let text = blocking(move || {
                worker.manage_blocking(&index, &Action::SetLifetime(lifetime), &cwd)
            })
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

/// An embedding run in the foreground, with progress on stderr. Ctrl-C stops it after
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
                eprintln!(
                    "\n{}",
                    crate::glyphs::render("Stopping after the current request…")
                );
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
                "\rembedding {}/{} texts, {} tokens ({}), and {}/{} chunks have vectors   ",
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

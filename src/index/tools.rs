//! The agent's tools (`index_search`, `index_status`, `index_create`,
//! `index_manage`), lenient parameter parsing, and the text both the tools and
//! the CLI print.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arko_agents::{
    TaskContext, Tool, ToolDefinition, ToolError, ToolParameterDefinition, ToolParameterType,
    ToolRecovery, ToolTypeDetails,
};
use async_trait::async_trait;
use serde_json::Value;

use super::embed;
use super::engine::{
    self, Action, CreateArgs, CreateOutcome, EmbedPlan, IndexReport, Indexes, ModeArg, SearchArgs,
    SearchOutput, fmt_ago, fmt_count, fmt_dollars,
};
use super::store::Kind;

/// The tools' names.
pub const TOOL_NAMES: &[&str] = &[
    "index_search",
    "index_status",
    "index_create",
    "index_manage",
];

/// Lines of each result's snippet.
pub const SNIPPET_LINES: usize = 3;
/// Characters of a snippet line.
const SNIPPET_WIDTH: usize = 140;

impl Indexes {
    /// The agent's tools, sharing this instance.
    pub fn tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(SearchTool {
                indexes: Arc::clone(self),
            }),
            Arc::new(StatusTool {
                indexes: Arc::clone(self),
            }),
            Arc::new(CreateTool {
                indexes: Arc::clone(self),
            }),
            Arc::new(ManageTool {
                indexes: Arc::clone(self),
            }),
        ]
    }
}

// ----- parameters -----

fn invalid(name: &str, message: impl Into<String>) -> ToolError {
    ToolError::ParameterValidation {
        param_name: name.into(),
        message: message.into(),
    }
}

/// A string parameter: a string (trimmed; empty is none), or a number or bool
/// as text.
pub(crate) fn str_param(params: &HashMap<String, Value>, name: &str) -> Option<String> {
    match params.get(name)? {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

pub(crate) fn required_str(
    params: &HashMap<String, Value>,
    name: &str,
) -> Result<String, ToolError> {
    str_param(params, name).ok_or_else(|| ToolError::MissingParameter {
        param_name: name.into(),
    })
}

/// A boolean: true/false, yes/no, on/off, 1/0, as JSON or text.
pub(crate) fn bool_param(
    params: &HashMap<String, Value>,
    name: &str,
) -> Result<Option<bool>, ToolError> {
    let Some(value) = params.get(name) else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::Bool(flag) => Ok(Some(*flag)),
        Value::Number(number) => Ok(Some(number.as_f64().unwrap_or(0.0) != 0.0)),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "" => Ok(None),
            "true" | "yes" | "y" | "on" | "1" => Ok(Some(true)),
            "false" | "no" | "n" | "off" | "0" => Ok(Some(false)),
            other => Err(invalid(
                name,
                format!("expected true or false, not {other:?}"),
            )),
        },
        _ => Err(invalid(name, "expected true or false")),
    }
}

/// A count: a JSON number, or text like `5000000`, `5,000,000`, `5_000_000`,
/// `200k`, `5M`.
pub(crate) fn parse_count(text: &str) -> Option<u64> {
    let text: String = text
        .trim()
        .chars()
        .filter(|c| !matches!(c, ',' | '_' | ' '))
        .collect();
    let lower = text.to_ascii_lowercase();
    let (number, scale) = if let Some(number) = lower.strip_suffix('k') {
        (number, 1_000.0)
    } else if let Some(number) = lower.strip_suffix('m') {
        (number, 1_000_000.0)
    } else {
        (lower.as_str(), 1.0)
    };
    let value: f64 = number.parse().ok()?;
    (value.is_finite() && value >= 0.0).then(|| (value * scale).round() as u64)
}

pub(crate) fn count_param(
    params: &HashMap<String, Value>,
    name: &str,
) -> Result<Option<u64>, ToolError> {
    let Some(value) = params.get(name) else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::Number(number) => number
            .as_u64()
            .or_else(|| {
                number
                    .as_f64()
                    .filter(|value| *value >= 0.0)
                    .map(|value| value.round() as u64)
            })
            .map(Some)
            .ok_or_else(|| invalid(name, "expected a non-negative number")),
        Value::String(text) if text.trim().is_empty() => Ok(None),
        Value::String(text) => parse_count(text)
            .map(Some)
            .ok_or_else(|| invalid(name, format!("expected a number, not {text:?}"))),
        _ => Err(invalid(name, "expected a number")),
    }
}

/// A list: a JSON array of strings, or text separated by commas or newlines.
pub(crate) fn list_param(params: &HashMap<String, Value>, name: &str) -> Vec<String> {
    let split = |text: &str| -> Vec<String> {
        text.split([',', '\n'])
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect()
    };
    match params.get(name) {
        Some(Value::String(text)) => split(text),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str())
            .flat_map(split)
            .collect(),
        _ => Vec::new(),
    }
}

fn param(
    name: &str,
    description: &str,
    param_type: ToolParameterType,
    required: bool,
) -> ToolParameterDefinition {
    ToolParameterDefinition {
        name: name.into(),
        description: description.into(),
        param_type,
        required,
        is_path: false,
        flag: None,
        default_value: None,
    }
}

fn definition(
    name: &str,
    description: &str,
    parameters: Vec<ToolParameterDefinition>,
    parallel_safe: bool,
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        output_based_example: None,
        parameters,
        parallel_safe,
        tool_type_details: ToolTypeDetails::DeveloperDefined,
        raw_json_schema_override: None,
    }
}

/// Runs blocking index work off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ToolError::ExecutionFailed(format!("index work failed: {error}")))?
        .map_err(ToolError::ExecutionFailed)
}

// ----- parsed arguments -----

pub(crate) fn parse_search(
    params: &HashMap<String, Value>,
    cwd: &Path,
) -> Result<SearchArgs, ToolError> {
    let query = required_str(params, "query")?;
    let mode = match str_param(params, "mode") {
        None => None,
        Some(text) if text.eq_ignore_ascii_case("auto") => None,
        Some(text) => Some(ModeArg::parse(&text).ok_or_else(|| {
            invalid("mode", format!("{text:?}: use hybrid, semantic or lexical"))
        })?),
    };
    let limit = count_param(params, "limit")?
        .map(|limit| (limit as usize).clamp(1, engine::MAX_LIMIT))
        .unwrap_or(engine::DEFAULT_LIMIT);
    Ok(SearchArgs {
        query,
        index: str_param(params, "index"),
        mode,
        limit,
        path_prefix: str_param(params, "path_prefix"),
        language: str_param(params, "language"),
        cwd: cwd.to_path_buf(),
    })
}

/// `code`, `docs` or `auto` (None).
pub(crate) fn parse_kind(text: Option<&str>) -> Result<Option<Kind>, String> {
    match text.map(str::trim).filter(|text| !text.is_empty()) {
        None => Ok(None),
        Some(text) if text.eq_ignore_ascii_case("auto") => Ok(None),
        Some(text) => Kind::parse(text)
            .map(Some)
            .ok_or_else(|| format!("{text:?}: use code, docs or auto")),
    }
}

pub(crate) fn parse_create(
    params: &HashMap<String, Value>,
    cwd: &Path,
) -> Result<CreateArgs, ToolError> {
    let path = str_param(params, "path").unwrap_or_else(|| ".".into());
    let kind =
        parse_kind(str_param(params, "kind").as_deref()).map_err(|error| invalid("kind", error))?;
    Ok(CreateArgs {
        path: PathBuf::from(path),
        cwd: cwd.to_path_buf(),
        kind,
        name: str_param(params, "name"),
        include: list_param(params, "include"),
        exclude: list_param(params, "exclude"),
        monitor: bool_param(params, "monitor")?.unwrap_or(true),
        embed: bool_param(params, "embed")?,
        max_embed_tokens: count_param(params, "max_embed_tokens")?,
        wait: bool_param(params, "wait")?.unwrap_or(false),
        defer_embed: false,
    })
}

pub(crate) fn parse_action(params: &HashMap<String, Value>) -> Result<Action, ToolError> {
    let action = required_str(params, "action")?;
    let normalized = action.to_ascii_lowercase().replace(['-', ' '], "_");
    Ok(match normalized.as_str() {
        "rebuild" | "update" | "build" | "reindex" => Action::Rebuild,
        "embed" => Action::Embed {
            max_tokens: count_param(params, "max_tokens")?,
            wait: bool_param(params, "wait")?.unwrap_or(false),
        },
        "estimate" | "cost" => Action::Estimate,
        "monitor_on" | "monitor" | "watch" => Action::Monitor(true),
        "monitor_off" | "unmonitor" | "unwatch" => Action::Monitor(false),
        "remove" | "delete" => Action::Remove,
        _ => {
            return Err(invalid(
                "action",
                format!(
                    "{action:?}: use rebuild, embed, estimate, monitor_on, monitor_off or remove"
                ),
            ));
        }
    })
}

// ----- output -----

/// The first `lines` non-blank lines of a chunk (bytes `start..end` of `file`),
/// with their common indentation removed, each cut at [`SNIPPET_WIDTH`].
pub(crate) fn snippet(file: &Path, start: usize, end: usize, lines: usize) -> Vec<String> {
    if lines == 0 {
        return Vec::new();
    }
    let Ok(bytes) = std::fs::read(file) else {
        return Vec::new();
    };
    let end = end.min(bytes.len());
    if start >= end {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes[start..end]);
    let picked: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(lines)
        .collect();
    let indent = picked
        .iter()
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    picked
        .iter()
        .map(|line| {
            let line = line.get(indent..).unwrap_or(line.trim_start()).trim_end();
            if line.chars().count() > SNIPPET_WIDTH {
                let cut: String = line.chars().take(SNIPPET_WIDTH - 1).collect();
                format!("{cut}…")
            } else {
                line.to_string()
            }
        })
        .collect()
}

fn mode_name(mode: arko_index::hybrid::Mode) -> &'static str {
    match mode {
        arko_index::hybrid::Mode::Lexical => "lexical",
        arko_index::hybrid::Mode::Dense => "semantic",
        arko_index::hybrid::Mode::Hybrid => "hybrid",
    }
}

/// A search's results: a header line, then per result `path:start-end · scope`
/// and its snippet, then notes.
pub fn format_search(query: &str, output: &SearchOutput, snippet_lines: usize) -> String {
    let mut out = String::new();
    let searched: Vec<String> = output
        .searched
        .iter()
        .map(|(name, kind, mode)| format!("{name} ({}, {})", kind.as_str(), mode_name(*mode)))
        .collect();
    let several = output.searched.len() > 1;
    if output.hits.is_empty() {
        out.push_str(&format!("No results for {query:?}"));
        if !searched.is_empty() {
            out.push_str(&format!(" in {}", searched.join(", ")));
        }
        out.push_str(". Try other words, a broader path_prefix, or rg for exact text.\n");
    } else {
        out.push_str(&format!(
            "{} result{} from {}:\n",
            output.hits.len(),
            if output.hits.len() == 1 { "" } else { "s" },
            searched.join(", ")
        ));
        for hit in &output.hits {
            let mut header = String::new();
            if several {
                header.push_str(&format!("[{}] ", hit.index));
            }
            header.push_str(&format!(
                "{}:{}-{}",
                hit.display, hit.start_line, hit.end_line
            ));
            let context = if hit.scope.is_empty() {
                &hit.symbol
            } else {
                &hit.scope
            };
            if !context.is_empty() {
                header.push_str(&format!(" · {context}"));
            }
            if hit.changed {
                header.push_str(" (file changed since indexed)");
            }
            out.push_str(&header);
            out.push('\n');
            for line in snippet(&hit.file, hit.start_byte, hit.end_byte, snippet_lines) {
                out.push_str("    ");
                out.push_str(&line);
                out.push('\n');
            }
        }
    }
    for note in &output.notes {
        out.push_str(&format!("note: {note}\n"));
    }
    out.trim_end().to_string()
}

/// "embedding now", with the run's progress and where it runs.
fn embedding_now(report: &IndexReport) -> String {
    let mut text = "embedding now".to_string();
    if let Some(progress) = &report.progress {
        text.push_str(&format!(
            " ({}/{} texts, {} tokens so far)",
            fmt_count(progress.texts),
            fmt_count(progress.total),
            fmt_count(progress.tokens)
        ));
    }
    text.push_str(if report.embed_running_here {
        " in this fin"
    } else {
        " in another process"
    });
    text
}

/// Indexes' states, one block each, and the session's embedding spending.
pub fn format_status(reports: &[IndexReport], spent: Option<(u64, f64)>) -> String {
    let mut out = String::new();
    if reports.is_empty() {
        out.push_str(
            "There are no indexes yet: index_create (or `fin index create PATH`) makes one.\n",
        );
    }
    for report in reports {
        let manifest = match &report.manifest {
            Ok(manifest) => manifest,
            Err(error) => {
                out.push_str(&format!("{} — unreadable: {error}\n", report.id));
                continue;
            }
        };
        out.push_str(&format!(
            "{} — {}, {} — {}\n",
            manifest.name,
            manifest.kind.as_str(),
            if manifest.embed {
                manifest.model.as_str()
            } else {
                "lexical only"
            },
            manifest.root.display()
        ));
        match (report.chunks, &manifest.last_build) {
            (Some(chunks), Some(build)) => out.push_str(&format!(
                "  {} chunks in {} files; built {} ({:.1} s)\n",
                fmt_count(chunks),
                fmt_count(build.files),
                build.at.map(fmt_ago).unwrap_or_else(|| "earlier".into()),
                build.seconds
            )),
            (Some(chunks), None) => out.push_str(&format!("  {} chunks\n", fmt_count(chunks))),
            (None, _) => out.push_str(&format!(
                "  not built: {}\n",
                report.status_error.as_deref().unwrap_or("no index")
            )),
        }
        if manifest.embed {
            let mut line = match &report.dense {
                Some(info) => format!(
                    "  embeddings: {}/{} chunks, {}",
                    fmt_count(info.embedded),
                    fmt_count(info.docs),
                    if report.embedding_active {
                        embedding_now(report)
                    } else {
                        engine::describe_dense(info).to_string()
                    }
                ),
                None => "  embeddings: none yet".to_string(),
            };
            if !report.has_key {
                line.push_str(&format!(" (no {}: lexical search only)", embed::KEY_NAME));
            } else if manifest.cap_reached {
                line.push_str(&format!(
                    " (the last run stopped at its cap of {} tokens; index_manage action=embed continues)",
                    fmt_count(manifest.max_embed_tokens)
                ));
            }
            line.push_str(&format!(
                "; spent {} tokens ({}) in all",
                fmt_count(manifest.embed_tokens),
                fmt_dollars(manifest.embed_cost)
            ));
            out.push_str(&line);
            out.push('\n');
            if let Some(last) = &report.last_embed {
                out.push_str(&format!("  last background run: {last}\n"));
            }
        }
        let mut monitor = format!(
            "  monitor: {}; {}",
            if manifest.monitor { "on" } else { "off" },
            report.watch.describe()
        );
        if report.pending_changes > 0 {
            monitor.push_str("; changes waiting for a rebuild");
        }
        out.push_str(&monitor);
        out.push('\n');
        if let Some(error) = &manifest.last_error {
            out.push_str(&format!("  last error: {error}\n"));
        }
    }
    if let Some((tokens, dollars)) = spent
        && tokens > 0
    {
        out.push_str(&format!(
            "This session's embeddings: {} tokens ({}).\n",
            fmt_count(tokens),
            fmt_dollars(dollars)
        ));
    }
    out.trim_end().to_string()
}

/// What `create` did.
pub fn format_create(outcome: &CreateOutcome) -> String {
    let manifest = &outcome.manifest;
    let mut out = format!(
        "{} index {} ({}) of {}: {} chunks in {} files ({:.1} s).\n",
        if outcome.updated {
            "Updated"
        } else {
            "Created"
        },
        manifest.name,
        manifest.kind.as_str(),
        manifest.root.display(),
        fmt_count(outcome.record.chunks),
        fmt_count(outcome.record.files),
        outcome.record.seconds,
    );
    if let Some(census) = &outcome.census {
        out.push_str(&format!(
            "Kind {} by the files: {}.\n",
            manifest.kind.as_str(),
            census.describe()
        ));
    }
    out.push_str(&format!("Files: {}.\n", outcome.record.summary));
    match &outcome.estimate {
        Some(Ok(estimate)) => out.push_str(&format!("Estimate: {}.\n", estimate.line())),
        Some(Err(error)) => out.push_str(&format!("Estimate unavailable: {error}.\n")),
        None => {}
    }
    match &outcome.embedding {
        EmbedPlan::Off => out.push_str(
            "Lexical only (no embeddings): index_manage action=embed adds them.\n",
        ),
        EmbedPlan::NoKey => out.push_str(&format!(
            "Embeddings ({}) wait for {}: until it is set, searches are lexical.\n",
            manifest.model,
            embed::KEY_NAME
        )),
        EmbedPlan::Done(Ok(summary)) => {
            out.push_str(&format!("Embeddings ({}): {}.\n", manifest.model, summary.line()))
        }
        EmbedPlan::Done(Err(error)) => out.push_str(&format!(
            "Embedding failed: {error}. Lexical search works; index_manage action=embed retries.\n"
        )),
        EmbedPlan::Deferred => {}
        EmbedPlan::Background => out.push_str(&format!(
            "Embedding with {} in the background (cap {} tokens per run); until it finishes, chunks without vectors are found by keywords. index_status shows progress.\n",
            manifest.model,
            fmt_count(manifest.max_embed_tokens)
        )),
    }
    out.push_str(&format!(
        "Monitor: {}.\n",
        if manifest.monitor {
            outcome.watch.describe()
        } else {
            "off (brought up to date when searched)".to_string()
        }
    ));
    for note in &outcome.notes {
        out.push_str(&format!("note: {note}\n"));
    }
    out.trim_end().to_string()
}

// ----- the tools -----

const SEARCH_DESCRIPTION: &str = "Semantic search of code and docs in fin's indexes (syntax-aware chunks, keyword BM25 plus Voyage embeddings). \
Prefer it to rg for concept or behaviour questions and when you don't know the names: \"where are failed requests retried\", \"how is the session saved to disk\", \"code that validates config paths\". \
Use rg/grep for exact identifiers, strings and regexes, or to find every occurrence. \
Returns ranked `path:start-end · scope` locations with a short snippet; read the promising ones with the shell (sed -n 'START,ENDp' FILE) before relying on them. \
Searches the indexes covering the current directory unless `index` names one; index_status lists them.";

const STATUS_DESCRIPTION: &str = "Shows fin's semantic search indexes (all, or the one named): root, kind, chunks, embedding coverage and cost, monitoring and pending changes, errors. Check it before index_create.";

const CREATE_DESCRIPTION: &str = "Creates a semantic search index of a directory and builds it, for index_search: local syntax-aware chunking, .gitignore respected, embeddings by Voyage (voyage-code-4 for code, voyage-4 for docs; kind auto decides by the files). \
Index a project or docs folder, not / or the home directory. The build is local (seconds for most projects); embedding costs about $0.12 per million tokens for code and $0.06 for docs: a free estimate is reported, each run is capped (5M tokens by default), and it runs in the background unless wait=true (searches meanwhile find chunks without vectors by keywords). \
With monitor=true (default) fin keeps the index current as files change. Calling it again for the same directory and kind updates the settings.";

const MANAGE_DESCRIPTION: &str = "Maintains an index: action=rebuild (re-chunk the files now), embed (embed chunks without vectors; max_tokens raises the run's cap; wait=true waits), estimate (free cost estimate of embedding what's left), monitor_on / monitor_off (keep it current as files change), remove (delete the index; the files are untouched).";

#[derive(Debug)]
struct SearchTool {
    indexes: Arc<Indexes>,
}

#[async_trait]
impl Tool for SearchTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "index_search",
            SEARCH_DESCRIPTION,
            vec![
                param(
                    "query",
                    "What to find, in words (a behaviour, a concept, a question) or identifiers.",
                    ToolParameterType::String,
                    true,
                ),
                param(
                    "index",
                    "An index name or root path (default: the indexes covering the current directory).",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "mode",
                    "hybrid (default when the index has embeddings), semantic, or lexical (keywords only, free).",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "limit",
                    "Results, 1-50 (default 10).",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "path_prefix",
                    "Only results under this directory or file (relative to the current directory).",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "language",
                    "Only chunks of this language: rust, python, typescript, markdown, ...",
                    ToolParameterType::String,
                    false,
                ),
            ],
            true,
        )
    }

    fn recovery_mode(&self) -> ToolRecovery {
        ToolRecovery::Rerun
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let args = parse_search(&params, context.current_working_directory)?;
        let indexes = Arc::clone(&self.indexes);
        let query = args.query.clone();
        let output = blocking(move || indexes.search_blocking(&args)).await?;
        Ok(format_search(&query, &output, SNIPPET_LINES))
    }
}

#[derive(Debug)]
struct StatusTool {
    indexes: Arc<Indexes>,
}

#[async_trait]
impl Tool for StatusTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "index_status",
            STATUS_DESCRIPTION,
            vec![param(
                "index",
                "An index name or root path (default: all indexes).",
                ToolParameterType::String,
                false,
            )],
            true,
        )
    }

    fn recovery_mode(&self) -> ToolRecovery {
        ToolRecovery::Rerun
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let index = str_param(&params, "index");
        let cwd = context.current_working_directory.to_path_buf();
        let indexes = Arc::clone(&self.indexes);
        let reports = blocking(move || indexes.status_blocking(index.as_deref(), &cwd)).await?;
        Ok(format_status(&reports, Some(self.indexes.spent())))
    }
}

#[derive(Debug)]
struct CreateTool {
    indexes: Arc<Indexes>,
}

#[async_trait]
impl Tool for CreateTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "index_create",
            CREATE_DESCRIPTION,
            vec![
                param(
                    "path",
                    "The directory to index (default: the current directory).",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "kind",
                    "code, docs, or auto (default: by the files).",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "name",
                    "A name for the index (default: the directory's name).",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "include",
                    "Only files matching these gitignore-style patterns, comma-separated (e.g. \"src/,*.md\").",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "exclude",
                    "Leave out files matching these gitignore-style patterns, comma-separated (e.g. \"vendor/,*.generated.ts\"); lock files and minified assets are always left out.",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "monitor",
                    "Keep the index current as files change (default true).",
                    ToolParameterType::Bool,
                    false,
                ),
                param(
                    "embed",
                    "Embed for semantic search (default true when a Voyage key is set).",
                    ToolParameterType::Bool,
                    false,
                ),
                param(
                    "max_embed_tokens",
                    "Token cap of each embedding run (default 5000000).",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "wait",
                    "Wait for the embedding before returning (default false: background).",
                    ToolParameterType::Bool,
                    false,
                ),
            ],
            false,
        )
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let args = parse_create(&params, context.current_working_directory)?;
        let indexes = Arc::clone(&self.indexes);
        let outcome = blocking(move || indexes.create_blocking(&args)).await?;
        Ok(format_create(&outcome))
    }
}

#[derive(Debug)]
struct ManageTool {
    indexes: Arc<Indexes>,
}

#[async_trait]
impl Tool for ManageTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "index_manage",
            MANAGE_DESCRIPTION,
            vec![
                param(
                    "index",
                    "The index name or root path (default: the one covering the current directory).",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "action",
                    "rebuild, embed, estimate, monitor_on, monitor_off or remove.",
                    ToolParameterType::String,
                    true,
                ),
                param(
                    "max_tokens",
                    "embed: the run's token cap (default: the index's, 5000000).",
                    ToolParameterType::String,
                    false,
                ),
                param(
                    "wait",
                    "embed: wait for the run to finish (default false).",
                    ToolParameterType::Bool,
                    false,
                ),
            ],
            false,
        )
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let action = parse_action(&params)?;
        let index = str_param(&params, "index");
        let cwd = context.current_working_directory.to_path_buf();
        let indexes = Arc::clone(&self.indexes);
        blocking(move || {
            let name = match index {
                Some(index) => index,
                None => {
                    let targets = indexes.targets(None, &cwd)?;
                    if targets.len() > 1 {
                        let names: Vec<&str> = targets
                            .iter()
                            .map(|found| found.manifest.name.as_str())
                            .collect();
                        return Err(format!(
                            "several indexes cover this directory ({}): name one",
                            names.join(", ")
                        ));
                    }
                    targets[0].layout.id()
                }
            };
            indexes.manage_blocking(&name, &action, &cwd)
        })
        .await
    }
}

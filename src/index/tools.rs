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
use arko_index::dense::StopSignal;
use async_trait::async_trait;
use serde_json::Value;

use super::embed;
use super::engine::{
    self, Action, CreateArgs, CreateOutcome, EmbedPlan, IndexReport, Indexes, ModeArg, SearchArgs,
    SearchOutput, chunks_in_files, fmt_ago, fmt_count, fmt_counted, fmt_dollars,
};
use super::lexical::SINGLE_QUOTES;
use super::store::Kind;

/// The search tool's name.
pub const SEARCH_TOOL_NAME: &str = "index_search";
/// The status tool's name.
pub const STATUS_TOOL_NAME: &str = "index_status";

/// The tools' names.
pub const TOOL_NAMES: &[&str] = &[
    SEARCH_TOOL_NAME,
    STATUS_TOOL_NAME,
    "index_create",
    "index_manage",
];

/// Lines of each result's snippet.
pub const SNIPPET_LINES: usize = 3;
/// Characters of a snippet line.
const SNIPPET_WIDTH: usize = 140;
/// Bytes of whole definitions one search shows. The results past it show
/// their snippet.
const FULL_BUDGET: usize = 24_000;
/// Lines of one whole definition. A longer one shows the part that matched.
const FULL_MAX_LINES: usize = 200;
/// Characters of a line of a whole definition.
const FULL_WIDTH: usize = 300;

/// How much of each result a search shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detail {
    /// The location and this many lines of the chunk that matched.
    Brief(usize),
    /// The whole definition (function, type, doc section) of the top results,
    /// with line numbers, up to [`FULL_BUDGET`], and the rest as `Brief`.
    Full,
    /// Grouped by file, each result's lines and scope and the lines that
    /// hold the query (an exact search's matches).
    Lines,
}

/// The definitions of the tools, which don't depend on the indexes, meaning what a
/// conversation saved with them was offered, for a session that has no
/// indexes.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        search_definition(),
        status_definition(),
        create_definition(),
        manage_definition(),
    ]
}

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

/// A string parameter, meaning a string (trimmed, with empty as none), or a number or bool
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

/// A boolean, as true/false, yes/no, on/off, 1/0, as JSON or text.
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

/// A count, as a JSON number, or text like `5000000`, `5,000,000`, `5_000_000`,
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

/// A list, as a JSON array of strings, or text separated by commas or newlines.
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
    read_only: bool,
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        output_based_example: None,
        parameters,
        // The tools that only read are the ones whose calls can overlap.
        parallel_safe: read_only,
        read_only,
        tool_type_details: ToolTypeDetails::DeveloperDefined,
        raw_json_schema_override: None,
    }
}

/// The stop signal of an index's embedding run, fired when the call's
/// `stop` fires (the turn was stopped, or the job a moved call runs in).
/// The run goes on on a blocking thread, which a dropped call doesn't stop,
/// so the forwarding lasts until the work drops the sender returned with
/// it, whether the call is still there or not.
pub(crate) fn embedding_stop(
    stop: &arko_agents::stop::StopSignal,
) -> (Arc<StopSignal>, tokio::sync::oneshot::Sender<()>) {
    let embedding = Arc::new(StopSignal::new());
    let (working, done) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn({
        let (stop, embedding) = (stop.clone(), embedding.clone());
        async move {
            tokio::select! {
                biased;
                () = stop.stopping() => {
                    embedding.stop(stop.reason().unwrap_or_else(|| "the call was stopped".into()));
                }
                _ = done => {}
            }
        }
    });
    (embedding, working)
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

/// `detail` is `full` or `brief`, or None for the mode's own.
pub(crate) fn parse_detail(params: &HashMap<String, Value>) -> Result<Option<Detail>, ToolError> {
    match str_param(params, "detail").map(|text| text.to_ascii_lowercase()) {
        None => Ok(None),
        Some(text) if matches!(text.as_str(), "full" | "whole" | "code") => Ok(Some(Detail::Full)),
        Some(text) if matches!(text.as_str(), "brief" | "short" | "snippet" | "snippets") => {
            Ok(Some(Detail::Brief(SNIPPET_LINES)))
        }
        Some(text) => Err(invalid("detail", format!("{text:?}: use full or brief"))),
    }
}

/// How a search in `mode` shows its results unless `detail` says. An exact
/// search lists its matches' lines, and the others show the top results
/// whole.
pub(crate) fn detail_for(mode: Option<ModeArg>, detail: Option<Detail>) -> Detail {
    detail.unwrap_or(if mode == Some(ModeArg::Lexical) {
        Detail::Lines
    } else {
        Detail::Full
    })
}

pub(crate) fn parse_search(
    params: &HashMap<String, Value>,
    cwd: &Path,
) -> Result<SearchArgs, ToolError> {
    let query = required_str(params, "query")?;
    let mode = match str_param(params, "mode") {
        None => ModeArg::Hybrid,
        Some(mode) => ModeArg::parse(&mode)
            .ok_or_else(|| invalid("mode", format!("{mode:?}: use semantic, lexical or hybrid")))?,
    };
    let limit = count_param(params, "limit")?
        .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX).max(1));
    let exhaustive = bool_param(params, "exhaustive")?.unwrap_or(false);
    let ignore_case = bool_param(params, "ignore_case")?.unwrap_or(false);
    let after = str_param(params, "after");
    if after.is_some() && mode != ModeArg::Lexical {
        return Err(invalid(
            "after",
            "it is for mode lexical, whose results hold the query as written, in path order",
        ));
    }
    Ok(SearchArgs {
        query,
        index: str_param(params, "index"),
        mode: Some(mode),
        limit,
        exhaustive,
        after,
        path_prefix: str_param(params, "path_prefix"),
        language: str_param(params, "language"),
        ignore_case,
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

/// Where a result's whole definition lies (1-based lines, inclusive).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Definition {
    pub start_line: usize,
    pub end_line: usize,
    /// The chunk that matched, in the file as it is now.
    pub matched: (usize, usize),
}

/// The whole definition `hit` lies in, meaning its file chunked again as an index of
/// `kind` chunks it, and the chunks next to the hit's that share its symbol
/// (a long function is split into several). When the file changed after it
/// was indexed, the chunk with the hit's symbol. None when the file can't be
/// read or the chunk isn't there any more.
pub(crate) fn whole_definition(hit: &engine::Hit, kind: Kind) -> Option<Definition> {
    let chunks = arko_index::bench_v1::chunk_file(&hit.file, &engine::chunk_options(kind)).ok()?;
    let same = |chunk: &arko_index::bench_v1::Chunk| chunk.symbol_path == hit.symbol;
    let at = chunks
        .iter()
        .position(|chunk| chunk.start_byte == hit.start_byte && same(chunk))
        .or_else(|| {
            if hit.symbol.is_empty() {
                None
            } else {
                chunks.iter().position(same)
            }
        })?;
    let (mut first, mut last) = (at, at);
    if !hit.symbol.is_empty() {
        while first > 0 && same(&chunks[first - 1]) {
            first -= 1;
        }
        while last + 1 < chunks.len() && same(&chunks[last + 1]) {
            last += 1;
        }
    }
    Some(Definition {
        start_line: chunks[first].start_line,
        end_line: chunks[last].end_line,
        matched: (chunks[at].start_line, chunks[at].end_line),
    })
}

/// Lines `start..=end` of `text` as `read_file` shows them (`12 | line`),
/// without trailing blank lines, and long lines are cut at [`FULL_WIDTH`].
fn numbered(text: &str, start: usize, end: usize) -> (String, usize) {
    let lines: Vec<&str> = text
        .lines()
        .enumerate()
        .skip(start.saturating_sub(1))
        .take((end + 1).saturating_sub(start.max(1)))
        .map(|(_, line)| line)
        .collect();
    let kept = lines.len()
        - lines
            .iter()
            .rev()
            .take_while(|line| line.trim().is_empty())
            .count();
    let mut out = String::new();
    for (offset, line) in lines[..kept].iter().enumerate() {
        let line = line.trim_end();
        let line = if line.chars().count() > FULL_WIDTH {
            let cut: String = line.chars().take(FULL_WIDTH - 1).collect();
            format!("{cut}…")
        } else {
            line.to_string()
        };
        out.push_str(&format!("{} | {line}\n", start.max(1) + offset));
    }
    (out, start.max(1) + kept.saturating_sub(1))
}

/// A result's header, as `[index] path:start-end · scope`.
fn result_header(hit: &engine::Hit, several: bool, lines: (usize, usize)) -> String {
    let mut header = String::new();
    if several {
        header.push_str(&format!("[{}] ", hit.index));
    }
    header.push_str(&format!("{}:{}-{}", hit.display, lines.0, lines.1));
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
    header
}

/// A result shown whole, meaning its header and numbered lines, and the lines shown.
fn whole_result(
    hit: &engine::Hit,
    found: &Definition,
    several: bool,
) -> Option<(String, (usize, usize))> {
    let text = String::from_utf8_lossy(&std::fs::read(&hit.file).ok()?).into_owned();
    let lines = found.end_line + 1 - found.start_line.min(found.end_line);
    // A definition too long to show whole, so it shows the part that matched.
    let partial = lines > FULL_MAX_LINES;
    let (start, last) = if partial {
        let (start, end) = found.matched;
        (start, end.min(start + FULL_MAX_LINES - 1))
    } else {
        (found.start_line, found.end_line)
    };
    let (body, end) = numbered(&text, start, last);
    if body.is_empty() {
        return None;
    }
    let mut header = result_header(hit, several, (start, end));
    if partial {
        header.push_str(&format!(
            " (part of a {lines}-line definition at lines {}-{}: read_file shows the rest)",
            found.start_line, found.end_line
        ));
    } else if found.matched != (found.start_line, found.end_line) {
        header.push_str(&format!(
            " (matched lines {}-{})",
            found.matched.0, found.matched.1
        ));
    }
    Some((format!("{header}\n{body}"), (start, end)))
}

/// How the first line of a search's result begins when it found nothing,
/// and how that line ends.
const NO_RESULTS: &str = "No results for ";
const NO_RESULTS_HINT: &str = ". Try other words, a broader path_prefix, or rg for exact text.";

/// How the first line of a search's result begins when a page after a cursor
/// had no results, though the search had some.
const NO_MORE: &str = "No results after ";

/// The first line of a search's result, which says how many results it had
/// from which indexes (`searched`, each `name (kind, mode)`), and of how many
/// in all when the list is cut ("100 of 340 results") or starts after a
/// cursor ("101-200 of 340 results"). [`read_search_header`] reads it back.
fn search_header(query: &str, searched: &[String], output: &SearchOutput) -> String {
    let hits = output.hits.len();
    let names = searched.join(", ");
    if hits == 0 {
        if let (Some(after), Some(total)) = (&output.after, output.total)
            && total > 0
        {
            return format!(
                "{NO_MORE}{after:?} from {names}. {} in all.",
                fmt_count(total as u64)
            );
        }
        let within = if searched.is_empty() {
            String::new()
        } else {
            format!(" in {names}")
        };
        return format!("{NO_RESULTS}{query:?}{within}{NO_RESULTS_HINT}");
    }
    let first = output.before as u64 + 1;
    let count = match output.total {
        Some(total) if output.before > 0 => format!(
            "{}-{} of {}",
            fmt_count(first),
            fmt_count(first + hits as u64 - 1),
            fmt_counted(total as u64, "result", "results")
        ),
        Some(total) if total > hits => format!(
            "{} of {}",
            fmt_count(hits as u64),
            fmt_counted(total as u64, "result", "results")
        ),
        _ => fmt_counted(hits as u64, "result", "results"),
    };
    format!("{count} from {names}:")
}

/// The hits of a search's result, as [`format_search_detail`] writes it,
/// for a person: its first line ("6 results from arko (code, hybrid)"),
/// then each result's header ("src/engine.rs:96-105 · fn branched", also
/// for results grouped by file) without the notes the agent reads (where the
/// rest of a definition is, the lines that matched). `None` when it found
/// nothing.
pub fn read_search_hits(output: &str) -> Option<Vec<String>> {
    let mut lines = output.lines();
    let first = lines.next()?;
    if read_search_header(first)?.1 == 0 {
        return None;
    }
    let mut hits = vec![first.trim_end_matches(':').to_string()];
    // The last line that could be a file of grouped results, and where it went.
    let mut file: Option<(usize, &str)> = None;
    for line in lines {
        // A result of grouped ones ("  12-30 · fn x" under its file), which
        // makes the line above its file rather than a result.
        if let Some(result) = line.strip_prefix("  ")
            && result.starts_with(|c: char| c.is_ascii_digit())
        {
            if let Some((at, path)) = file {
                if hits.get(at).is_some_and(|hit| hit == path) {
                    hits.remove(at);
                    file = Some((usize::MAX, path));
                }
                hits.push(format!("{path}:{result}"));
            }
            continue;
        }
        // (A whole result's numbered lines, a snippet's indented ones, and
        // the notes after the results.)
        let numbered = line
            .split_once(" | ")
            .is_some_and(|(number, _)| number.bytes().all(|byte| byte.is_ascii_digit()));
        if line.trim().is_empty()
            || line.starts_with(char::is_whitespace)
            || numbered
            || line.starts_with("note: ")
        {
            continue;
        }
        let mut hit = line;
        for note in [" (part of a ", " (matched lines ", ": in the lines "] {
            if let Some((before, _)) = hit.split_once(note) {
                hit = before;
            }
        }
        file = Some((hits.len(), line));
        hits.push(hit.to_string());
    }
    Some(hits)
}

/// The text after the string quoted as Rust quotes one at the start of
/// `text`.
fn after_quoted(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('"')?;
    let mut escaped = false;
    let end = rest.char_indices().find_map(|(at, c)| {
        let ends = c == '"' && !escaped;
        escaped = c == '\\' && !escaped;
        ends.then_some(at)
    })?;
    Some(&rest[end + 1..])
}

/// What the first line of a search's result says, as [`search_header`]
/// writes it, meaning the indexes searched and how many results there were.
pub fn read_search_header(line: &str) -> Option<(Vec<String>, usize)> {
    let names = |list: &str| -> Vec<String> {
        list.split("), ")
            .filter_map(|item| {
                item.rsplit_once(" (")
                    .map(|(name, _)| name.trim().to_string())
            })
            .filter(|name| !name.is_empty())
            .collect()
    };
    if let Some(rest) = line.strip_prefix(NO_RESULTS) {
        // Past the query.
        let rest = after_quoted(rest)?.strip_suffix(NO_RESULTS_HINT)?;
        let list = rest.strip_prefix(" in ").unwrap_or_default();
        return Some((names(list), 0));
    }
    if let Some(rest) = line.strip_prefix(NO_MORE) {
        // Past the cursor, and then the indexes before the count.
        // ("No results after \"src/a.rs:804:5\" from proj (code, lexical). 340 in all.")
        let list = after_quoted(rest)?.strip_prefix(" from ")?;
        return Some((names(list.rsplit_once(". ")?.0), 0));
    }
    let (count, rest) = line.split_once(' ')?;
    let number = |text: &str| text.replace(',', "").parse::<usize>().ok();
    // ("101-200 of 340 results" is a page after a cursor.)
    let hits = match count.split_once('-') {
        Some((first, last)) => number(last)?.checked_sub(number(first)?)? + 1,
        None => number(count)?,
    };
    // ("100 of 340 results", a cut list.)
    let rest = match rest.strip_prefix("of ") {
        Some(cut) => cut.split_once(' ')?.1,
        None => rest,
    };
    let list = rest
        .strip_prefix("results from ")
        .or_else(|| rest.strip_prefix("result from "))?
        .strip_suffix(':')?;
    Some((names(list), hits))
}

/// A search's results, meaning a header line, then per result `path:start-end · scope`
/// and its snippet, then notes.
#[cfg(test)]
pub fn format_search(query: &str, output: &SearchOutput, snippet_lines: usize) -> String {
    format_search_detail(query, output, Detail::Brief(snippet_lines))
}

/// `hits` in their order, after a blank line, with a heading where the file
/// (or the index) changes from the result before. Each group is the file
/// (`[index] path` when `several` indexes were searched), then each result as
/// its lines and scope (`  774-877 · fn x`), then the lines that hold the
/// query (`    804 | text`). The results of an exact search are in the order of
/// their places, so a file is one group unless indexes share it.
fn grouped_results(hits: &[engine::Hit], several: bool) -> String {
    let mut out = String::new();
    if !hits.is_empty() {
        out.push('\n');
    }
    let mut group: Option<(&str, &Path)> = None;
    for hit in hits {
        if group != Some((hit.index.as_str(), hit.file.as_path())) {
            group = Some((hit.index.as_str(), hit.file.as_path()));
            if several {
                out.push_str(&format!("[{}] ", hit.index));
            }
            out.push_str(&hit.display);
            out.push('\n');
        }
        out.push_str(&format!("  {}-{}", hit.start_line, hit.end_line));
        let context = if hit.scope.is_empty() {
            &hit.symbol
        } else {
            &hit.scope
        };
        if !context.is_empty() {
            out.push_str(&format!(" · {context}"));
        }
        if hit.changed {
            out.push_str(" (file changed since indexed)");
        }
        out.push('\n');
        let Some(lines) = &hit.lines else {
            continue;
        };
        for (number, line) in &lines.shown {
            out.push_str(&format!("    {number} | {line}\n"));
        }
        if lines.more > 0 {
            out.push_str(&format!(
                "    and {} more {}\n",
                fmt_count(lines.more as u64),
                crate::remote::noun(lines.more, "line", "lines")
            ));
        }
    }
    out
}

/// A search's results with `detail`, meaning a header line, then per result
/// `path:start-end · scope` and its lines, then notes.
pub fn format_search_detail(query: &str, output: &SearchOutput, detail: Detail) -> String {
    let mut text = format_search_body(query, output, detail);
    if let Some(note) = next_note(output, false) {
        text.push_str(&format!("\nnote: {note}"));
    }
    text
}

/// [`format_search_detail`] without the note that gives the next page, which
/// the command writes for the shell.
pub(super) fn format_search_body(query: &str, output: &SearchOutput, detail: Detail) -> String {
    let mut out = String::new();
    let searched: Vec<String> = output
        .searched
        .iter()
        .map(|(name, kind, mode)| format!("{name} ({}, {})", kind.as_str(), mode_name(*mode)))
        .collect();
    let several = output.searched.len() > 1;
    out.push_str(&search_header(query, &searched, output));
    out.push('\n');
    if detail == Detail::Lines {
        out.push_str(&grouped_results(&output.hits, several));
    } else if !output.hits.is_empty() {
        let kinds: HashMap<&str, Kind> = output
            .searched
            .iter()
            .map(|(name, kind, _)| (name.as_str(), *kind))
            .collect();
        let mut budget = FULL_BUDGET;
        // The whole definitions shown, meaning file and lines.
        let mut shown: Vec<(&Path, (usize, usize))> = Vec::new();
        for hit in &output.hits {
            let lines = match detail {
                Detail::Brief(lines) => lines,
                // (Shown above.)
                Detail::Lines => 0,
                Detail::Full => {
                    out.push('\n');
                    let kind = kinds.get(hit.index.as_str()).copied().unwrap_or(Kind::Code);
                    let found = whole_definition(hit, kind);
                    if let Some(found) = &found
                        && let Some((_, lines)) = shown.iter().find(|(file, lines)| {
                            *file == hit.file.as_path()
                                && lines.0 <= found.matched.0
                                && found.matched.1 <= lines.1
                        })
                    {
                        out.push_str(&format!(
                            "{}: in the lines {}-{} shown above\n",
                            result_header(hit, several, found.matched),
                            lines.0,
                            lines.1
                        ));
                        continue;
                    }
                    if budget > 0
                        && let Some(found) = &found
                        && let Some((text, lines)) = whole_result(hit, found, several)
                    {
                        if text.len() <= budget {
                            budget -= text.len();
                            shown.push((hit.file.as_path(), lines));
                            out.push_str(&text);
                            continue;
                        }
                        // The results after it show their snippets, in rank order.
                        budget = 0;
                    }
                    SNIPPET_LINES
                }
            };
            out.push_str(&result_header(hit, several, (hit.start_line, hit.end_line)));
            out.push('\n');
            for line in snippet(&hit.file, hit.start_byte, hit.end_byte, lines) {
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

/// The note of a cut exact list, which ends with the cursor of the next page.
/// A tool passes it as `after="CURSOR"`, and a command as `--after 'CURSOR'`
/// (see [`shell_word`]), quoted for POSIX shells and PowerShell. `lists all`
/// counts what the same search lists with exhaustive=true, meaning everything
/// after the cursor it was given.
pub(super) fn next_note(output: &SearchOutput, command: bool) -> Option<String> {
    let next = output.next.as_ref()?;
    let all = fmt_count((output.total.unwrap_or_default() - output.before) as u64);
    let (count, cursor) = (fmt_count(next.count as u64), next.cursor.to_string());
    Some(if command {
        format!(
            "--exhaustive lists all {all}, and --path-prefix narrows the search. For the next {count}, use --after {}",
            shell_word(&cursor, cfg!(windows))
        )
    } else {
        format!(
            "exhaustive=true lists all {all}, and path_prefix narrows the search. For the next {count}, use after=\"{cursor}\""
        )
    })
}

/// `text` as one word of a POSIX shell command or a PowerShell one, in single
/// quotes. Nothing inside single quotes is changed, run or split, except
/// their delimiters, which are written as the shell takes them literally. In
/// POSIX sh a single quote ends the quotes, is escaped and begins them again.
/// In PowerShell every one of [`SINGLE_QUOTES`] is doubled. A cursor holds
/// none of them, so the word for a cursor holds no delimiter but its own
/// wrapping quotes. Other shells, such as cmd.exe, are not covered.
pub(super) fn shell_word(text: &str, powershell: bool) -> String {
    let mut word = String::from("'");
    for c in text.chars() {
        match c {
            '\'' if !powershell => word.push_str(r"'\''"),
            c if powershell && SINGLE_QUOTES.contains(&c) => word.extend([c, c]),
            c => word.push(c),
        }
    }
    word.push('\'');
    word
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
        " in this Arko"
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
            "There are no indexes yet: index_create (or `arko index create PATH`) makes one.\n",
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
                "  {}, built {} ({:.1} s)\n",
                chunks_in_files(chunks, build.files),
                build.at.map(fmt_ago).unwrap_or_else(|| "earlier".into()),
                build.seconds
            )),
            (Some(chunks), None) => {
                out.push_str(&format!("  {}\n", fmt_counted(chunks, "chunk", "chunks")))
            }
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
                    " (the last run stopped at its cap of {} tokens, and index_manage action=embed continues)",
                    fmt_count(manifest.max_embed_tokens)
                ));
            }
            line.push_str(&format!(
                ", with {} tokens ({}) spent in all",
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
            "  monitor: {}",
            engine::monitor_words(manifest.monitor, &report.watch)
        );
        if report.pending_changes > 0 {
            monitor.push_str(", with changes waiting for a rebuild");
        }
        out.push_str(&monitor);
        out.push('\n');
        for error in [&manifest.build_error, &manifest.embed_error]
            .into_iter()
            .flatten()
        {
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
        "{} index {} ({}) of {}: {} ({:.1} s).\n",
        if outcome.updated {
            "Updated"
        } else {
            "Created"
        },
        manifest.name,
        manifest.kind.as_str(),
        manifest.root.display(),
        chunks_in_files(outcome.record.chunks, outcome.record.files),
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
            engine::monitor_words(true, &outcome.watch)
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

const SEARCH_DESCRIPTION: &str = "Searches Arko's syntax-aware code and docs indexes (keyword BM25 plus Voyage embeddings), returning whole functions, types or doc sections with line numbers. \
Its main advantage is semantic search by meaning when you don't know the exact symbols, and hybrid search for finding full definitions in one call. For exact text or lexical searches, `rg`/grep is fine as the default. \
Semantic and hybrid return ranked `path:start-end · scope` results, top results whole and the rest as previews (detail=brief returns locations and snippets only). \
Lexical lists matching lines grouped by file in path order (exhaustive=true lists all, or page with after). \
Works for any indexed directory via `index` or `path_prefix`, defaulting to indexes covering the current directory or its git repo (index_status lists all indexes).";

const STATUS_DESCRIPTION: &str = "Shows Arko's semantic search indexes (all, or the one named): root, kind, chunks, embedding coverage and cost, monitoring and pending changes, errors. Check it before index_create.";

const CREATE_DESCRIPTION: &str = "Creates a semantic search index of a directory and builds it, for index_search: local syntax-aware chunking, .gitignore respected, embeddings by Voyage (voyage-code-4 for code, voyage-4 for docs; kind auto decides by the files). \
Index a project or docs folder, not / or the home directory. The build is local (seconds for most projects); embedding costs about $0.12 per million tokens for code and $0.06 for docs: a free estimate is reported, each run is capped (5M tokens by default), and it runs in the background unless wait=true (searches meanwhile find chunks without vectors by keywords). \
With monitor=true (default) Arko keeps the index current as files change. Calling it again for the same directory and kind updates the settings.";

const MANAGE_DESCRIPTION: &str = "Maintains an index: action=rebuild (re-chunk the files now), embed (embed chunks without vectors; max_tokens raises the run's cap; wait=true waits), estimate (free cost estimate of embedding what's left), monitor_on / monitor_off (keep it current as files change), remove (delete the index; the files are untouched).";

#[derive(Debug)]
struct SearchTool {
    indexes: Arc<Indexes>,
}

#[async_trait]
impl Tool for SearchTool {
    fn definition(&self) -> ToolDefinition {
        search_definition()
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
        let detail = detail_for(args.mode, parse_detail(&params)?);
        let indexes = Arc::clone(&self.indexes);
        let query = args.query.clone();
        // Chunking the results' files again is blocking work too.
        blocking(move || {
            let output = indexes.search_blocking(&args)?;
            Ok(format_search_detail(&query, &output, detail))
        })
        .await
    }
}

#[derive(Debug)]
struct StatusTool {
    indexes: Arc<Indexes>,
}

#[async_trait]
impl Tool for StatusTool {
    fn definition(&self) -> ToolDefinition {
        status_definition()
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
        create_definition()
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        let args = parse_create(&params, context.current_working_directory)?;
        let indexes = Arc::clone(&self.indexes);
        let (stop, working) = embedding_stop(&context.stop);
        let outcome = blocking(move || {
            let _working = working;
            indexes.create_stopping_with(&args, Some(stop))
        })
        .await?;
        Ok(format_create(&outcome))
    }

    /// With `wait`, a build and its embedding can take long. The call then
    /// moves to the background, and its stop stops the embedding.
    fn background_mode(&self) -> arko_agents::ToolBackground {
        arko_agents::ToolBackground::Detach
    }
}

#[derive(Debug)]
struct ManageTool {
    indexes: Arc<Indexes>,
}

#[async_trait]
impl Tool for ManageTool {
    fn definition(&self) -> ToolDefinition {
        manage_definition()
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
        let (stop, working) = embedding_stop(&context.stop);
        blocking(move || {
            let _working = working;
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
            indexes.manage_stopping_with(&name, &action, &cwd, Some(stop))
        })
        .await
    }

    /// A rebuild, or an embedding run it waits for, can take long. The call
    /// then moves to the background, and its stop stops the embedding.
    fn background_mode(&self) -> arko_agents::ToolBackground {
        arko_agents::ToolBackground::Detach
    }
}

/// What `limit` says, meaning the results by default in each mode (`limits`)
/// and the most in semantic and hybrid mode.
fn limit_description(limits: engine::Limits) -> String {
    let ranked = if limits.semantic == limits.hybrid {
        format!("{} in semantic and hybrid mode", limits.semantic)
    } else {
        format!(
            "{} in semantic mode and {} in hybrid",
            limits.semantic, limits.hybrid
        )
    };
    format!(
        "Results, by default {} in lexical mode and {ranked}, where {} is the most.",
        limits.lexical,
        engine::MAX_LIMIT
    )
}

/// The definition of `SearchTool`'s tool.
fn search_definition() -> ToolDefinition {
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
                "An index name, or a directory or file inside an index (absolute or relative; e.g. ../other-repo). Default: the indexes covering path_prefix, else the current directory.",
                ToolParameterType::String,
                false,
            ),
            param(
                "mode",
                "Default `hybrid`: good for finding full definitions (`ARKO_HOME`), a symbol with words about it (\"Session::save error handling\"), and docs/Markdown. `semantic`: describe behaviour in your own words when you don't know the exact symbols (\"where are failed requests retried\"). `lexical`: exact matches of a name or string in path order, case-sensitive unless ignore_case=true (`rg`/grep is fine as the default for lexical, regex, punctuation or substring searches). Indexes without embeddings fall back to keyword search in semantic and hybrid modes.",
                ToolParameterType::String,
                false,
            ),
            param(
                "limit",
                &limit_description(engine::limits()),
                ToolParameterType::Integer,
                false,
            ),
            param(
                "exhaustive",
                "In lexical mode, true lists every match whatever the limit (all that follow `after`).",
                ToolParameterType::Bool,
                false,
            ),
            param(
                "after",
                "Lexical mode only. Cursor from the end of a cut list's note, such as `src/a.rs:804:5`, to list the matches that follow it.",
                ToolParameterType::String,
                false,
            ),
            param(
                "ignore_case",
                "In lexical mode, true matches ASCII letters in any case (semantic and hybrid are already case-insensitive).",
                ToolParameterType::Bool,
                false,
            ),
            param(
                "path_prefix",
                "Only results under this directory or file (absolute, or relative to the current directory or named index).",
                ToolParameterType::String,
                false,
            ),
            param(
                "detail",
                "full (default in semantic and hybrid mode): the top results' whole definitions with line numbers; brief: locations and 3-line snippets only. Lexical mode lists the lines that hold the query unless detail is given.",
                ToolParameterType::String,
                false,
            ),
            param(
                "language",
                "Only chunks in this language, such as python or rust. When none match, lexical search says so, and semantic and hybrid search list every language. Leave it out unless the project mixes languages and you want one of them.",
                ToolParameterType::String,
                false,
            ),
        ],
        true,
    )
}

/// The definition of `StatusTool`'s tool.
fn status_definition() -> ToolDefinition {
    definition(
        "index_status",
        STATUS_DESCRIPTION,
        vec![param(
            "index",
            "An index name, or a directory or file inside an index (default: all indexes).",
            ToolParameterType::String,
            false,
        )],
        true,
    )
}

/// The definition of `CreateTool`'s tool.
fn create_definition() -> ToolDefinition {
    definition(
        "index_create",
        CREATE_DESCRIPTION,
        vec![
            param(
                "path",
                "The directory to index, absolute or relative: any directory, also outside the current one, e.g. ../other-repo (default: the current directory).",
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
                ToolParameterType::Integer,
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

/// The definition of `ManageTool`'s tool.
fn manage_definition() -> ToolDefinition {
    definition(
        "index_manage",
        MANAGE_DESCRIPTION,
        vec![
            param(
                "index",
                "The index name, or a directory or file inside it (default: the one covering the current directory). remove takes the name or the index's root only.",
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
                ToolParameterType::Integer,
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

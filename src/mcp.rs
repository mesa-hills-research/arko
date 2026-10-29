//! MCP servers: programs that give the agents more tools over the Model
//! Context Protocol. Arko starts the servers set up in the files
//! `--mcp-config` names, in the project's `.arko/mcp.json` and `.mcp.json`
//! (once the user approves them) and in `mcp.json` in Arko's folder. The
//! agents reach them through one tool, `mcp` ([`McpBridgeTool`]), whose
//! definition stays the same while servers start, stop, come and go, so the
//! prompt cache holds. A conversation (or worker) saved when each MCP tool
//! was a tool of its own (`mcp__<server>__<tool>`) keeps those, called
//! through the same hub, which checks that its run may reach the server as
//! `mcp` does. Each open agent session runs its own copy of each server and stops
//! it when the session closes, except a server marked `"shared": true`,
//! which sessions open at the same time share. Chat sessions run none.
//!
//! A file is in Claude Code's format, servers by name:
//! `{"mcpServers": {"github": {"command": "npx", "args": ["-y", "..."], "env": {"TOKEN": "${GITHUB_TOKEN}"}}}}`,
//! or in arko's, a list of servers that have a `name`. `${VAR}` and
//! `${VAR:-default}` in a command, its arguments and its environment are
//! taken from Arko's environment. Servers talk over standard input and
//! output: HTTP servers (`url`, or a `type` other than `stdio`) and servers
//! marked `"disabled": true` are skipped. Arko reads two fields of its own
//! in any file, `workers` (false keeps a server to the main agent) and
//! `shared`.
//!
//! A project's `.arko/mcp.json` and `.mcp.json` would run programs as soon
//! as Arko starts in its folder, so Arko asks before starting them, and
//! remembers the answer (in `mcp-approvals.json` in its folder) until the
//! file's servers change. The main agent adds servers with the action
//! `add` ([`changes`]), which asks the user, writes the entry and remembers
//! the file as approved.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arko_agents::mcp::{McpAccess, McpBridgeTool, McpChanges, McpHub, ServerState};
use arko_agents::tool::{ToolOutput, ToolRecovery};
use arko_agents::{McpServerConfig, TaskContext, Tool, ToolDefinition, ToolError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::details::McpState;

pub mod changes;

pub use arko_agents::mcp::MCP_TOOL_NAME;

/// The servers set up for every project, in Arko's folder.
pub const USER_FILE: &str = "mcp.json";
/// A project's servers, in the working directory (as Claude Code reads it).
pub const PROJECT_FILE: &str = ".mcp.json";
/// The servers agents add for a project, in its `.arko` folder.
pub const LOCAL_FILE: &str = "mcp.json";
/// The answers about projects' servers, in Arko's folder.
const APPROVALS_FILE: &str = "mcp-approvals.json";
/// How long a server gets to start and list its tools.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long an action of `mcp` other than `add` may take before Arko stops
/// it, as long as the host gives any other tool's call. (`add` waits for the
/// user's answer without a limit, so the tool is exempt from the host's.)
const CALL_LIMIT: Duration = Duration::from_secs(20 * 60);

/// The project's `.arko/mcp.json`, where agents add its servers.
pub fn local_file(workspace: &Path) -> PathBuf {
    crate::arko_dir::dir(workspace).join(LOCAL_FILE)
}

/// What the main agent's `mcp` says after its description, the same for
/// every conversation.
pub const NOTE: &str = "Project servers go in the project's .arko/mcp.json, which git ignores until the user turns off Keep agents' project files out of git in Arko's settings.";

/// A server to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The environment as the file writes it, before `${VAR}`s are
    /// expanded.
    pub written_env: BTreeMap<String, String>,
    /// The command line as the file writes it, before `${VAR}`s are
    /// expanded: what the user is shown.
    pub shown: String,
    /// The file that sets it up.
    pub file: PathBuf,
    /// Workers may reach it (Arko's `workers` field, true unless the entry
    /// says false).
    pub workers: bool,
    /// Sessions open at the same time share one copy of it (Arko's `shared`
    /// field).
    pub shared: bool,
}

impl Server {
    /// The server for the hub, with `description` for the bridge's
    /// `servers` action (an agent's reason for adding it).
    fn config(&self, description: Option<String>) -> McpServerConfig {
        // The `${VAR}`s its file writes, with their values, which the log
        // shows as `${VAR}` should the server print them.
        let expanded: BTreeMap<&str, String> = std::iter::once(self.shown.as_str())
            .chain(self.written_env.values().map(String::as_str))
            .flat_map(variables)
            .filter_map(|name| {
                environment(name)
                    .filter(|value| !value.is_empty())
                    .map(|value| (name, value))
            })
            .collect();
        let mut config = McpServerConfig::new(self.name.clone(), self.command.clone())
            .with_args(self.args.clone())
            .with_env(self.env.clone())
            .with_expanded(expanded);
        config.workers = self.workers;
        config.shared = self.shared;
        config.description = description;
        config
    }

    /// The names of the environment variables it gets, and whether each has
    /// a value.
    pub fn env_names(&self) -> Vec<(String, bool)> {
        self.env
            .iter()
            .map(|(name, value)| (name.clone(), !value.is_empty()))
            .collect()
    }
}

/// A configuration file's servers, and why it skips the others.
#[derive(Debug, Default)]
pub struct Parsed {
    pub servers: Vec<Server>,
    pub skipped: Vec<String>,
    /// The entries not started, by name ("server 3" for one without a
    /// name), with their command line as the file writes it (empty when it
    /// has none) and why (skipped, or disabled).
    pub left_out: Vec<(String, String, String)>,
    /// The file's `mcpServers`, as written: what a project's approval is for.
    raw: Value,
}

/// Reads the servers in the file at `path`. `lookup` gives the values of
/// `${VAR}`s.
pub fn parse_file(path: &Path, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Parsed, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {} ({error})", path.display()))?;
    parse(&text, path, lookup)
        .map_err(|error| format!("could not read {} ({error})", path.display()))
}

fn parse(
    text: &str,
    file: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Parsed, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let raw = value
        .get("mcpServers")
        .ok_or("it has no `mcpServers`")?
        .clone();
    let mut parsed = Parsed::default();
    for (index, (name, entry)) in entries(&raw)?.into_iter().enumerate() {
        let Some(name) = name.filter(|name| !name.trim().is_empty()) else {
            parsed
                .skipped
                .push(format!("server {} has no name", index + 1));
            parsed.left_out.push((
                format!("server {}", index + 1),
                written_command(entry),
                "it has no name".into(),
            ));
            continue;
        };
        match server(&name, entry, file, lookup) {
            Ok(Some(server)) => parsed.servers.push(server),
            Ok(None) => parsed.left_out.push((
                name,
                written_command(entry),
                "it is disabled in its file".into(),
            )),
            Err(why) => {
                parsed.skipped.push(format!("`{name}` {why}"));
                parsed
                    .left_out
                    .push((name, written_command(entry), format!("it {why}")));
            }
        }
    }
    parsed.raw = raw;
    Ok(parsed)
}

/// The entries of a file's `mcpServers`, each with its name if it has one.
fn entries(raw: &Value) -> Result<Vec<(Option<String>, &Value)>, String> {
    Ok(match raw {
        Value::Object(servers) => servers
            .iter()
            .map(|(name, entry)| (Some(name.clone()), entry))
            .collect(),
        Value::Array(servers) => servers
            .iter()
            .map(|entry| {
                let name = entry.get("name").and_then(Value::as_str).map(String::from);
                (name, entry)
            })
            .collect(),
        _ => return Err("its `mcpServers` is neither an object nor a list".into()),
    })
}

/// Whether the file at `file` has an entry for the server `name` (enabled
/// or not), as written.
pub(crate) fn sets_up(file: &Path, name: &str) -> bool {
    names_in(file).is_some_and(|names| names.iter().any(|known| known == name))
}

/// The entry for the server `name` in the file at `file`, as written, if it
/// has one.
pub(crate) fn entry_in(file: &Path, name: &str) -> Option<Value> {
    let text = std::fs::read_to_string(file).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    entries(value.get("mcpServers")?)
        .ok()?
        .into_iter()
        .find(|(known, _)| known.as_deref() == Some(name))
        .map(|(_, entry)| entry.clone())
}

/// The names of the servers the file at `file` has entries for, or `None`
/// when it can't be read.
fn names_in(file: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(file).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let raw = value.get("mcpServers")?;
    Some(
        entries(raw)
            .ok()?
            .into_iter()
            .filter_map(|(name, _)| name)
            .collect(),
    )
}

/// The command line of a server's `entry` as the file writes it, as far
/// as it is text (empty without a `command`).
fn written_command(entry: &Value) -> String {
    let Some(command) = entry.get("command").and_then(Value::as_str) else {
        return String::new();
    };
    let args = entry
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|arg| match arg {
            Value::String(text) => Some(text.clone()),
            Value::Number(_) | Value::Bool(_) => Some(arg.to_string()),
            _ => None,
        });
    std::iter::once(command.to_string())
        .chain(args)
        .collect::<Vec<_>>()
        .join(" ")
}

/// One server's entry; `None` when it is disabled.
fn server(
    name: &str,
    entry: &Value,
    file: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Server>, String> {
    let entry = entry.as_object().ok_or("isn't an object")?;
    if entry.get("disabled").and_then(Value::as_bool) == Some(true) {
        return Ok(None);
    }
    let stdio_only = "Arko starts servers over standard input and output only";
    if let Some(kind) = entry.get("type").and_then(Value::as_str)
        && kind != "stdio"
    {
        return Err(format!("is a `{kind}` server, and {stdio_only}"));
    }
    let command = match entry.get("command").and_then(Value::as_str) {
        Some(command) if !command.trim().is_empty() => command,
        _ if entry.contains_key("url") => {
            return Err(format!("is an HTTP server (`url`), and {stdio_only}"));
        }
        _ => return Err("has no `command`".into()),
    };
    let text = |value: &Value| match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(_) | Value::Bool(_) => Some(value.to_string()),
        _ => None,
    };
    let args = match entry.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(args)) => args
            .iter()
            .map(|arg| text(arg).ok_or("has an argument that isn't text"))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("has `args` that aren't a list".into()),
    };
    let env: BTreeMap<String, String> = match entry.get("env") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(env)) => env
            .iter()
            .map(|(key, value)| {
                text(value)
                    .map(|value| (key.clone(), value))
                    .ok_or("has an `env` value that isn't text")
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("has an `env` that isn't an object".into()),
    };
    let flag = |field: &str, default: bool| match entry.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("has a `{field}` that isn't true or false")),
    };
    let workers = flag("workers", true)?;
    let shared = flag("shared", false)?;
    let shown = std::iter::once(command.to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let expand = |text: &str| expand(text, lookup);
    Ok(Some(Server {
        name: name.to_string(),
        command: expand(command)?,
        args: args
            .iter()
            .map(|arg| expand(arg))
            .collect::<Result<_, _>>()?,
        env: env
            .iter()
            .map(|(key, value)| Ok((key.clone(), expand(value)?)))
            .collect::<Result<_, String>>()?,
        written_env: env,
        shown,
        file: file.to_path_buf(),
        workers,
        shared,
    }))
}

/// `text` with each `${VAR}` replaced by the variable's value, and each
/// `${VAR:-default}` by the value, or the default when the variable is
/// unset or empty.
fn expand(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut expanded = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let Some(length) = rest[start..].find('}') else {
            break;
        };
        expanded.push_str(&rest[..start]);
        let inner = &rest[start + 2..start + length];
        let (name, default) = match inner.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (inner, None),
        };
        let value = match (lookup(name), default) {
            (Some(value), Some(default)) if value.is_empty() => default.to_string(),
            (Some(value), _) => value,
            (None, Some(default)) => default.to_string(),
            (None, None) => return Err(format!("uses ${{{name}}}, which isn't set")),
        };
        expanded.push_str(&value);
        rest = &rest[start + length + 1..];
    }
    expanded.push_str(rest);
    Ok(expanded)
}

/// The names of the variables `text` takes as `${VAR}` or
/// `${VAR:-default}`, as [`expand`] reads them.
fn variables(text: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let Some(length) = rest[start..].find('}') else {
            break;
        };
        let inner = &rest[start + 2..start + length];
        names.push(inner.split_once(":-").map_or(inner, |(name, _)| name));
        rest = &rest[start + length + 1..];
    }
    names
}

pub(crate) fn environment(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// `path` made absolute and without `.` and `..`, as the approvals and the
/// record of what agents added name files and folders.
pub(crate) fn normal(path: &Path) -> PathBuf {
    arko_agents::utils::normalize_path(
        &std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
    )
}

/// A project's servers that wait for the user's answer before they start,
/// as a front end shows them when it asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpApproval {
    /// The project's `.arko/mcp.json` or `.mcp.json`.
    pub file: PathBuf,
    /// Each server's name, and its command line as the file writes it.
    pub servers: Vec<(String, String)>,
    /// The servers the file sets up that Arko skips, and why.
    pub skipped: Vec<String>,
}

/// Which of a project's files an answer is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    /// The project's `.arko/mcp.json`, kept in the approvals' `local` map.
    Local,
    /// The project's `.mcp.json`, kept in their `projects` map.
    Project,
}

/// The servers to start, as [`plan`] finds them, before the user answers
/// about the project's files (`approvals`).
#[derive(Debug)]
pub struct Plan {
    /// The project's files whose servers wait for the user's answer, in
    /// the order [`Plan::decide`] takes the answers.
    pub approvals: Vec<McpApproval>,
    steps: Vec<Step>,
    root: PathBuf,
    workspace: PathBuf,
}

/// What [`Plan::decide`] does, in order.
#[derive(Debug)]
enum Step {
    /// Starts a file's servers.
    Add(Parsed),
    /// Starts a project file's servers if the user agrees.
    Ask(Asked, Parsed),
    Note(String),
}

/// The servers to start: those in `files` (from `--mcp-config`, or an
/// app's), in the project's `.arko/mcp.json` and `.mcp.json` when the user
/// approves them, and in Arko's own `mcp.json`. A name set up twice is
/// taken from the first of these. The user's answer about a project file's
/// servers, unless one is remembered for them, is for [`Plan::decide`]. A
/// file of `files` that can't be read is an error when they are `required`,
/// else a note (its servers aren't started), as the others are.
pub fn plan(
    root: &Path,
    workspace: &Path,
    files: &[PathBuf],
    required: bool,
) -> Result<Plan, String> {
    let mut steps = Vec::new();
    let mut approvals = Vec::new();
    let mut read = HashSet::new();
    let unread = |error: &str| {
        Step::Note(format!(
            "MCP servers not started. {}",
            crate::ui::sentence(error)
        ))
    };
    for file in files {
        let file = normal(file);
        match parse_file(&file, &environment) {
            Ok(parsed) => steps.push(Step::Add(parsed)),
            Err(error) if required => return Err(error),
            Err(error) => steps.push(unread(&error)),
        }
        read.insert(file);
    }
    for (asked, file) in [
        (Asked::Local, local_file(workspace)),
        (Asked::Project, workspace.join(PROJECT_FILE)),
    ] {
        if !file.is_file() || read.contains(&normal(&file)) {
            continue;
        }
        match parse_file(&file, &environment) {
            Ok(parsed) if parsed.servers.is_empty() && parsed.skipped.is_empty() => {}
            Ok(parsed) => match remembered_as(root, workspace, asked, &parsed) {
                Some(true) => steps.push(Step::Add(parsed)),
                Some(false) => {}
                None => {
                    approvals.push(McpApproval {
                        file: file.clone(),
                        servers: parsed
                            .servers
                            .iter()
                            .map(|server| (server.name.clone(), server.shown.clone()))
                            .collect(),
                        skipped: parsed.skipped.clone(),
                    });
                    steps.push(Step::Ask(asked, parsed));
                }
            },
            Err(error) => steps.push(unread(&error)),
        }
        read.insert(normal(&file));
    }
    let user = root.join(USER_FILE);
    if user.is_file() && !read.contains(&normal(&user)) {
        match parse_file(&user, &environment) {
            Ok(parsed) => steps.push(Step::Add(parsed)),
            Err(error) => steps.push(unread(&error)),
        }
    }
    Ok(Plan {
        approvals,
        steps,
        root: root.to_path_buf(),
        workspace: workspace.to_path_buf(),
    })
}

impl Plan {
    /// The servers to start, with `answers` about the project's files in
    /// the order of [`approvals`](Self::approvals) (`None`, or no answer
    /// at all: no one could answer, so they don't start), and the notes
    /// that say what is left out and why. An answer is remembered until the
    /// file's servers change.
    pub fn decide(self, answers: &[Option<bool>]) -> Result<(Vec<Server>, Vec<String>), String> {
        let mut servers: Vec<Server> = Vec::new();
        let mut notes = Vec::new();
        let mut add = |parsed: Parsed, notes: &mut Vec<String>| {
            for why in parsed.skipped {
                notes.push(format!("MCP server {why} (skipped)"));
            }
            for server in parsed.servers {
                match servers.iter().find(|taken| taken.name == server.name) {
                    Some(taken) => notes.push(format!(
                        "MCP server `{}` in {} is also in {}, and Arko starts the one in {}",
                        server.name,
                        server.file.display(),
                        taken.file.display(),
                        taken.file.display()
                    )),
                    None => servers.push(server),
                }
            }
        };
        let mut answers = answers.iter().copied();
        for step in self.steps {
            match step {
                Step::Add(parsed) => add(parsed, &mut notes),
                Step::Note(note) => notes.push(note),
                Step::Ask(asked, parsed) => match answers.next().flatten() {
                    Some(start) => {
                        remember_as(&self.root, &self.workspace, asked, &parsed.raw, start)?;
                        if start {
                            add(parsed, &mut notes);
                        }
                    }
                    None => notes.push(format!(
                        "the MCP servers in {} aren't started, because a project's servers start only once the user agrees, and no one here can be asked",
                        match asked {
                            Asked::Local => local_file(&self.workspace),
                            Asked::Project => self.workspace.join(PROJECT_FILE),
                        }
                        .display()
                    )),
                },
            }
        }
        Ok((servers, notes))
    }
}

/// The answers about projects' servers.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Approvals {
    /// The answers about projects' `.mcp.json`, by working directory.
    #[serde(default)]
    projects: BTreeMap<String, Approval>,
    /// The answers about projects' `.arko/mcp.json`, by working directory.
    /// A server an agent adds there is remembered as approved.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    local: BTreeMap<String, Approval>,
}

impl Approvals {
    fn of(&mut self, asked: Asked) -> &mut BTreeMap<String, Approval> {
        match asked {
            Asked::Local => &mut self.local,
            Asked::Project => &mut self.projects,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Approval {
    /// Whether to start them.
    start: bool,
    /// The `mcpServers` the answer is for.
    #[serde(rename = "mcpServers")]
    servers: Value,
}

fn read_approvals(root: &Path) -> Result<Approvals, String> {
    let path = root.join(APPROVALS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Approvals::default());
        }
        Err(error) => return Err(format!("could not read {} ({error})", path.display())),
    };
    serde_json::from_str(&text)
        .map_err(|error| format!("could not read {} ({error})", path.display()))
}

fn approvals(root: &Path) -> Approvals {
    read_approvals(root).unwrap_or_default()
}

/// The key a project's answers are kept under. `.mcp.json`'s answers have
/// been kept under the working directory as given, and `.arko/mcp.json`'s
/// are kept under it made absolute.
fn approval_key(workspace: &Path, asked: Asked) -> String {
    match asked {
        Asked::Project => workspace.display().to_string(),
        Asked::Local => normal(workspace).display().to_string(),
    }
}

pub(crate) fn project_approval(root: &Path, workspace: &Path) -> Result<Option<bool>, String> {
    file_approval(root, workspace, Asked::Project)
}

pub(crate) fn local_approval(root: &Path, workspace: &Path) -> Result<Option<bool>, String> {
    file_approval(root, workspace, Asked::Local)
}

/// The answer remembered for the project file `asked` as it is now, if
/// any. An absent file has none.
fn file_approval(root: &Path, workspace: &Path, asked: Asked) -> Result<Option<bool>, String> {
    let workspace = approval_workspace(workspace)?;
    let file = match asked {
        Asked::Local => local_file(&workspace),
        Asked::Project => workspace.join(PROJECT_FILE),
    };
    // An absent config has no current approval. Other read errors reach the caller.
    match std::fs::metadata(&file) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {} ({error})", file.display())),
        Ok(_) => {}
    }
    let parsed = parse_file(&file, &environment)?;
    Ok(read_approvals(root)?
        .of(asked)
        .get(&approval_key(&workspace, asked))
        .filter(|approval| approval.servers == parsed.raw)
        .map(|approval| approval.start))
}

fn approval_workspace(workspace: &Path) -> Result<PathBuf, String> {
    std::path::absolute(workspace)
        .map(|path| arko_agents::utils::normalize_path(&path))
        .map_err(|error| format!("could not resolve {} ({error})", workspace.display()))
}

pub(crate) fn forget_project_approval(root: &Path, workspace: &Path) -> Result<bool, String> {
    forget_approval(root, workspace, Asked::Project)
}

pub(crate) fn forget_local_approval(root: &Path, workspace: &Path) -> Result<bool, String> {
    forget_approval(root, workspace, Asked::Local)
}

fn forget_approval(root: &Path, workspace: &Path, asked: Asked) -> Result<bool, String> {
    let workspace = approval_workspace(workspace)?;
    let key = approval_key(&workspace, asked);
    if !read_approvals(root)?.of(asked).contains_key(&key) {
        return Ok(false);
    }
    let _lock = approval_lock(root)?;
    let mut approvals = read_approvals(root)?;
    let removed = approvals.of(asked).remove(&key).is_some();
    if removed {
        save_approvals(root, &approvals)?;
    }
    Ok(removed)
}

/// The answer remembered for the project's `.mcp.json` servers, if any (for
/// these servers: a change to them asks again).
pub(crate) fn remembered(root: &Path, workspace: &Path, parsed: &Parsed) -> Option<bool> {
    remembered_as(root, workspace, Asked::Project, parsed)
}

/// The answer remembered for the project's `.arko/mcp.json` servers, if any.
pub(crate) fn remembered_local(root: &Path, workspace: &Path, parsed: &Parsed) -> Option<bool> {
    remembered_as(root, workspace, Asked::Local, parsed)
}

fn remembered_as(root: &Path, workspace: &Path, asked: Asked, parsed: &Parsed) -> Option<bool> {
    remembered_raw(&approvals(root), workspace, asked, &parsed.raw)
}

fn remembered_raw(
    approvals: &Approvals,
    workspace: &Path,
    asked: Asked,
    raw: &Value,
) -> Option<bool> {
    let map = match asked {
        Asked::Local => &approvals.local,
        Asked::Project => &approvals.projects,
    };
    map.get(&approval_key(workspace, asked))
        .filter(|approval| approval.servers == *raw)
        .map(|approval| approval.start)
}

/// Remembers the user's answer about a project file's servers.
fn remember_as(
    root: &Path,
    workspace: &Path,
    asked: Asked,
    raw: &Value,
    start: bool,
) -> Result<(), String> {
    let _lock = approval_lock(root)?;
    remember_locked(root, workspace, asked, raw, start)
}

/// [`remember_as`] with the lock held.
fn remember_locked(
    root: &Path,
    workspace: &Path,
    asked: Asked,
    raw: &Value,
    start: bool,
) -> Result<(), String> {
    let mut approvals = read_approvals(root)?;
    approvals.of(asked).insert(
        approval_key(workspace, asked),
        Approval {
            start,
            servers: raw.clone(),
        },
    );
    save_approvals(root, &approvals)
}

fn approval_lock(root: &Path) -> Result<std::fs::File, String> {
    let path = root.join(format!("{APPROVALS_FILE}.lock"));
    let file = std::fs::create_dir_all(root)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
        })
        .map_err(|error| format!("could not open {} ({error})", path.display()))?;
    file.lock()
        .map_err(|error| format!("could not lock {} ({error})", path.display()))?;
    Ok(file)
}

fn save_approvals(root: &Path, approvals: &Approvals) -> Result<(), String> {
    let path = root.join(APPROVALS_FILE);
    let text = serde_json::to_string_pretty(approvals).map_err(|error| error.to_string())?;
    crate::session::write_whole(&path, text.as_bytes())
        .map_err(|error| format!("could not save {} ({error})", path.display()))
}

/// The file at `path` as JSON, or an empty one when there is none.
fn read_servers_file(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| format!("Couldn't read {} ({error}).", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::json!({"mcpServers": {}}))
        }
        Err(error) => Err(format!(
            "Couldn't read {} ({}).",
            path.display(),
            crate::profiles::reason_of(&error)
        )),
    }
}

fn save_servers_file(path: &Path, value: &Value) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    text.push('\n');
    crate::session::write_whole(path, text.as_bytes()).map_err(|error| {
        format!(
            "Couldn't save {} ({}).",
            path.display(),
            crate::profiles::reason_of(&error)
        )
    })
}

/// Writes `entry` as the server `name` in the file at `path`, keeping its
/// other entries and fields, in place of an entry of the same name. A file
/// that lists its servers keeps the list, and any other gets them by name,
/// as Claude Code writes them. Returns the file's `mcpServers` before and
/// after.
fn write_entry(path: &Path, name: &str, entry: Value) -> Result<(Value, Value), String> {
    let mut file = read_servers_file(path)?;
    let Some(object) = file.as_object_mut() else {
        return Err(format!(
            "Couldn't add to {}, because it isn't a JSON object.",
            path.display()
        ));
    };
    let servers = object
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Default::default()));
    let before = servers.clone();
    match servers {
        Value::Object(servers) => {
            servers.insert(name.to_string(), entry);
        }
        Value::Array(servers) => {
            let mut entry = entry;
            if let Value::Object(fields) = &mut entry {
                let mut named = serde_json::Map::new();
                named.insert("name".into(), Value::from(name));
                named.extend(std::mem::take(fields));
                *fields = named;
            }
            match servers
                .iter()
                .position(|known| known.get("name").and_then(Value::as_str) == Some(name))
            {
                Some(index) => servers[index] = entry,
                None => servers.push(entry),
            }
        }
        _ => {
            return Err(format!(
                "Couldn't add to {}, because its mcpServers is neither an object nor a list.",
                path.display()
            ));
        }
    }
    let after = servers.clone();
    save_servers_file(path, &file)?;
    Ok((before, after))
}

/// Takes the entry of the server `name` out of the file at `path`, keeping
/// the rest. Returns its `mcpServers` before and after, or `None` when it
/// has no such entry.
fn remove_entry(path: &Path, name: &str) -> Result<Option<(Value, Value)>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let mut file = read_servers_file(path)?;
    let Some(servers) = file.get_mut("mcpServers") else {
        return Ok(None);
    };
    let before = servers.clone();
    let removed = match servers {
        Value::Object(servers) => servers.remove(name).is_some(),
        Value::Array(servers) => {
            let count = servers.len();
            servers.retain(|known| known.get("name").and_then(Value::as_str) != Some(name));
            servers.len() != count
        }
        _ => false,
    };
    if !removed {
        return Ok(None);
    }
    let after = servers.clone();
    save_servers_file(path, &file)?;
    Ok(Some((before, after)))
}

/// The tool name of `server`'s tool `tool` as a tool of its own, as
/// conversations saved before the `mcp` tool name them:
/// `mcp__<server>__<tool>`, with the characters the providers don't take
/// replaced by `_`, and cut to 64 characters (ending in a hash of the full
/// name, to stay unique).
#[cfg(test)]
pub fn tool_name(server: &str, tool: &str) -> String {
    arko_agents::mcp::direct_tool_name(server, tool)
}

/// The server and tool an MCP tool name stands for, to show it.
pub fn split_tool_name(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix("mcp__")?.split_once("__")
}

/// A session's MCP servers: the hub its agents reach them through, with
/// what Arko read of each from its file. Clones share the servers.
#[derive(Clone)]
pub struct Servers {
    pub hub: Arc<McpHub>,
    /// Each server as its file sets it up, added ones last.
    known: Arc<Mutex<Vec<Server>>>,
    /// The servers an agent stopped.
    stopped: Arc<Mutex<BTreeSet<String>>>,
}

impl std::fmt::Debug for Servers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Servers")
            .field("hub", &self.hub)
            .finish_non_exhaustive()
    }
}

impl Default for Servers {
    fn default() -> Self {
        Self::of(Vec::new(), None)
    }
}

impl Servers {
    /// The hub of `servers`, none started yet. With Arko's folder `root`,
    /// a server an agent added has its reason as its description.
    pub fn of(servers: Vec<Server>, root: Option<&Path>) -> Self {
        let added = root.map(crate::agent_added::servers).unwrap_or_default();
        let configs = servers
            .iter()
            .map(|server| {
                let why = added
                    .iter()
                    .find(|record| {
                        record.name == server.name && record.file == normal(&server.file)
                    })
                    .map(|record| record.why.clone());
                server.config(why)
            })
            .collect();
        Self {
            hub: McpHub::new(configs),
            known: Arc::new(Mutex::new(servers)),
            stopped: Arc::default(),
        }
    }

    /// Starts `servers` at the same time, each given a minute to start.
    pub async fn start(servers: Vec<Server>, root: Option<&Path>) -> Self {
        let started = Self::of(servers, root);
        started.hub.start_all(STARTUP_TIMEOUT).await;
        started
    }

    /// Each server as its file sets it up, in the hub's order.
    pub fn known(&self) -> Vec<Server> {
        crate::lock(&self.known).clone()
    }

    /// The server `name` as its file sets it up.
    pub fn server(&self, name: &str) -> Option<Server> {
        crate::lock(&self.known)
            .iter()
            .find(|server| server.name == name)
            .cloned()
    }

    /// The state of the server `name`.
    pub fn state(&self, name: &str) -> Option<McpState> {
        let facts = self
            .hub
            .servers()
            .into_iter()
            .find(|facts| facts.name == name)?;
        Some(match facts.state {
            ServerState::Starting => McpState::Starting,
            ServerState::Running => McpState::Running,
            ServerState::Stopped if crate::lock(&self.stopped).contains(name) => {
                McpState::StoppedByAgent
            }
            ServerState::Stopped => McpState::Stopped,
            ServerState::Failed(reason) => McpState::Failed(reason),
        })
    }

    /// The names of the tools of the server `name` when they were last
    /// listed.
    pub fn tool_names(&self, name: &str) -> Option<Vec<String>> {
        self.hub
            .listed_tools(name)
            .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
    }

    /// The servers that run, by name.
    pub fn running(&self) -> Vec<String> {
        self.hub
            .servers()
            .into_iter()
            .filter(|facts| facts.state == ServerState::Running)
            .map(|facts| facts.name)
            .collect()
    }

    /// Whether workers may reach any of the servers.
    pub fn any_for_workers(&self) -> bool {
        self.hub.servers().iter().any(|facts| facts.workers)
    }

    /// The servers, in a line: `MCP servers github (12 tools), figma
    /// (failed)`.
    pub fn summary(&self) -> Option<String> {
        let servers = self.hub.servers();
        if servers.is_empty() {
            return None;
        }
        let servers = servers
            .iter()
            .map(|facts| match (&facts.state, facts.tools) {
                (ServerState::Failed(_), _) => format!("{} (failed)", facts.name),
                (_, Some(count)) => format!(
                    "{} ({count} tool{})",
                    facts.name,
                    if count == 1 { "" } else { "s" }
                ),
                (_, None) => facts.name.clone(),
            })
            .collect::<Vec<_>>();
        Some(format!("MCP servers {}", servers.join(", ")))
    }

    /// The server an agent added, after its entry was written: added to the
    /// hub and started. `Err` is a config the hub won't take.
    async fn add(
        &self,
        server: Server,
        description: Option<String>,
    ) -> Result<arko_agents::mcp::ServerFacts, String> {
        let config = server.config(description);
        {
            let mut known = crate::lock(&self.known);
            known.retain(|known| known.name != server.name);
            known.push(server);
        }
        crate::lock(&self.stopped).remove(&config.name);
        self.hub.add(config).await
    }

    /// Takes the server `name` out of the hub, stopping it.
    pub async fn remove(&self, name: &str) {
        crate::lock(&self.known).retain(|known| known.name != name);
        crate::lock(&self.stopped).remove(name);
        let _ = self.hub.remove(name).await;
    }

    /// Stops the servers, each given three seconds to exit, and lets go of
    /// the shared ones.
    pub async fn shutdown(&self) {
        self.hub.shutdown().await;
    }
}

/// What a session has of MCP: its servers, and how the main agent adds and
/// removes them.
#[derive(Default)]
pub struct Session {
    pub servers: Servers,
    /// How the main agent adds and removes servers. None in a chat, which
    /// runs no servers, and its agents' saved conversations get stand-ins.
    pub changes: Option<Arc<changes::Changes>>,
    /// Where the main agent's starts and stops show.
    pub notices: Option<changes::Notices>,
}

impl Session {
    /// The main agent's `mcp`, as a registry takes it.
    pub fn lead_tool(&self, access: Arc<dyn McpAccess>) -> Arc<dyn Tool> {
        match &self.changes {
            Some(changes) => Arc::new(Bridge {
                added: Some(changes.clone()),
                ..Bridge::lead(&self.servers, access, changes.clone(), self.notices.clone())
            }),
            None => Arc::new(UnstartedTool::bridge_in_chat(Role::Lead)),
        }
    }

    /// A worker's `mcp`, as a registry takes it.
    pub fn worker_tool(&self, access: Arc<dyn McpAccess>) -> Arc<dyn Tool> {
        match &self.changes {
            Some(_) => Arc::new(Bridge::worker(&self.servers, access)),
            None => Arc::new(UnstartedTool::bridge_in_chat(Role::Worker)),
        }
    }
}

/// Who an `mcp` tool is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Lead,
    Worker,
}

/// Arko's `mcp` tool: the framework's [`McpBridgeTool`] with Arko's time
/// limit and transcript lines. An `add` waits for the user's answer without
/// a limit, so the host's limit doesn't apply to the tool, and any other
/// action gets [`CALL_LIMIT`]. The main agent's starts and stops show in the
/// transcript, and its stops are kept for `/mcp`.
pub struct Bridge {
    inner: McpBridgeTool,
    role: Role,
    servers: Servers,
    notices: Option<changes::Notices>,
    /// The main agent's adds, whose servers that didn't start `servers`
    /// lists too.
    added: Option<Arc<changes::Changes>>,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl Bridge {
    /// The main agent's `mcp`, which adds and removes servers through
    /// `changes`.
    pub fn lead(
        servers: &Servers,
        access: Arc<dyn McpAccess>,
        changes: Arc<dyn McpChanges>,
        notices: Option<changes::Notices>,
    ) -> Self {
        Self {
            inner: McpBridgeTool::lead(servers.hub.clone(), access, changes).with_note(NOTE),
            role: Role::Lead,
            servers: servers.clone(),
            notices,
            added: None,
        }
    }

    /// A worker's `mcp`.
    pub fn worker(servers: &Servers, access: Arc<dyn McpAccess>) -> Self {
        Self {
            inner: McpBridgeTool::worker(servers.hub.clone(), access),
            role: Role::Worker,
            servers: servers.clone(),
            notices: None,
            added: None,
        }
    }

    /// The definition of `role`'s `mcp`, the same in every session.
    pub fn definition_of(role: Role) -> ToolDefinition {
        let servers = Servers::default();
        let access: Arc<dyn McpAccess> = Arc::new(arko_agents::mcp::AllServers);
        match role {
            Role::Lead => Self::lead(&servers, access, Arc::new(changes::NoChanges), None),
            Role::Worker => Self::worker(&servers, access),
        }
        .definition()
    }

    fn action(params: &std::collections::HashMap<String, Value>) -> &str {
        params
            .get("action")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
    }

    /// `servers`'s list with the servers an agent added that didn't start
    /// (they aren't the hub's), each with why.
    fn with_not_started(&self, mut output: ToolOutput, added: &changes::Changes) -> ToolOutput {
        let rows: Vec<String> = added
            .not_started()
            .into_iter()
            .map(|(record, why)| format!("{}  not started, because {why}.", record.name))
            .collect();
        if rows.is_empty() {
            return output;
        }
        let rows = rows.join("\n");
        output.text = if self.servers.hub.servers().is_empty() {
            rows
        } else {
            format!("{}\n{rows}", output.text.trim_end())
        };
        output
    }

    /// Says in the transcript that the main agent started or stopped a
    /// server, and keeps its stops.
    fn after(&self, action: &str, server: Option<&str>, result: &Result<ToolOutput, ToolError>) {
        let (Some(server), Ok(output)) = (server, result) else {
            return;
        };
        match action {
            "stop" => {
                crate::lock(&self.servers.stopped).insert(server.to_string());
                if let Some(notices) = &self.notices {
                    notices.info(&format!("The main agent stopped the MCP server {server}."));
                }
            }
            "start" => {
                crate::lock(&self.servers.stopped).remove(server);
                if output.text.starts_with("Started ")
                    && let Some(notices) = &self.notices
                {
                    notices.info(&format!("The main agent started the MCP server {server}."));
                }
            }
            "call" | "tools" => {
                crate::lock(&self.servers.stopped).remove(server);
            }
            _ => {}
        }
    }
}

#[async_trait]
impl Tool for Bridge {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    fn allow_result_interception(&self) -> bool {
        self.inner.allow_result_interception()
    }

    /// An `add` waits for the user's answer, which no host limit cuts
    /// short. The other actions keep their own limit ([`CALL_LIMIT`]).
    fn uses_host_timeout(&self) -> bool {
        false
    }

    fn interruptible(&self) -> bool {
        self.inner.interruptible()
    }

    fn recovery_mode(&self) -> ToolRecovery {
        self.inner.recovery_mode()
    }

    fn recovery_mode_for(&self, params: &std::collections::HashMap<String, Value>) -> ToolRecovery {
        self.inner.recovery_mode_for(params)
    }

    fn parallel_safe_for(&self, params: &std::collections::HashMap<String, Value>) -> Option<bool> {
        self.inner.parallel_safe_for(params)
    }

    fn background_mode(&self) -> arko_agents::ToolBackground {
        self.inner.background_mode()
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: std::collections::HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        self.execute_with_output(context, params)
            .await
            .map(|output| output.text)
    }

    async fn execute_with_output(
        &self,
        context: &TaskContext<'_>,
        params: std::collections::HashMap<String, Value>,
    ) -> Result<ToolOutput, ToolError> {
        let action = Self::action(&params).to_string();
        let server = params
            .get("server")
            .and_then(Value::as_str)
            .map(|name| name.trim().to_string());
        let result = if action == "add" {
            self.inner.execute_with_output(context, params).await
        } else {
            match tokio::time::timeout(CALL_LIMIT, self.inner.execute_with_output(context, params))
                .await
            {
                Ok(result) => result,
                Err(_) => Err(ToolError::ExecutionFailed(format!(
                    "The {action} action got no answer within {} minutes, so Arko stopped it.",
                    CALL_LIMIT.as_secs() / 60
                ))),
            }
        };
        if self.role == Role::Lead {
            self.after(&action, server.as_deref(), &result);
        }
        match (action.as_str(), result, &self.added) {
            ("servers", Ok(output), Some(added)) => Ok(self.with_not_started(output, added)),
            (_, result, _) => result,
        }
    }
}

/// The servers each run may reach: every one for the main agent's runs, and
/// a worker's own set ([`crate::crew::Crew`] records it with the run).
pub struct Access {
    crew: Option<std::sync::Weak<crate::crew::Crew>>,
}

impl Access {
    pub fn new(crew: Option<&Arc<crate::crew::Crew>>) -> Self {
        Self {
            crew: crew.map(Arc::downgrade),
        }
    }
}

impl McpAccess for Access {
    /// The run's own set as the crew recorded it, or every server for a run
    /// with no record (the main agent's).
    fn servers_for(&self, ctx: &TaskContext<'_>) -> Option<Vec<String>> {
        let crew = self.crew.as_ref()?.upgrade()?;
        crew.mcp_servers_of(ctx.task_id)
    }
}

/// Stands in for a tool a saved conversation offers that this session
/// doesn't run. A saved conversation (or worker) keeps its own
/// configuration, so it still offers the tools it was saved with, and can't
/// resume unless each is registered, for those runs only
/// (`resume_only_agent_tool`). A chat starts no servers, and in an agent's
/// session a server may be gone from the files or fail to start.
#[derive(Debug)]
pub struct UnstartedTool {
    name: String,
    said: String,
    /// The definition the conversation was offered, for the `mcp` tool.
    definition: Option<ToolDefinition>,
}

impl UnstartedTool {
    /// For a tool in a chat.
    pub fn in_chat(name: String) -> Self {
        let said = "This MCP server isn't running, because a chat starts none. The user can switch to Agent to use it.".into();
        Self {
            name,
            said,
            definition: None,
        }
    }

    /// For `role`'s `mcp` tool in a chat.
    pub fn bridge_in_chat(role: Role) -> Self {
        Self {
            name: MCP_TOOL_NAME.into(),
            said: crate::details::CHAT_MCP_NOTE.into(),
            definition: Some(Bridge::definition_of(role)),
        }
    }

    /// For a tool of Arko's own that this session doesn't have (the shell
    /// and terminal tools in a chat, the monitors with -p, the search index's
    /// without an index, the worker profile tools without workers).
    pub fn unavailable(name: String) -> Self {
        Self {
            name,
            said: "This tool isn't available in this session.".into(),
            definition: None,
        }
    }

    /// For a tool of a server that is no longer set up or didn't start.
    pub fn not_running(name: String) -> Self {
        let server = split_tool_name(&name).map_or(name.as_str(), |(server, _)| server);
        let said = format!(
            "The MCP server {server} isn't running. It's no longer set up, or it failed to start."
        );
        Self {
            name,
            said,
            definition: None,
        }
    }
}

#[async_trait]
impl Tool for UnstartedTool {
    fn definition(&self) -> ToolDefinition {
        if let Some(definition) = &self.definition {
            return definition.clone();
        }
        ToolDefinition {
            name: self.name.clone(),
            description: self.said.clone(),
            output_based_example: None,
            parameters: vec![],
            parallel_safe: true,
            tool_type_details: arko_agents::ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: Some(
                serde_json::json!({"type":"object", "properties":{}, "additionalProperties":true}),
            ),
        }
    }

    async fn execute(
        &self,
        _: &TaskContext<'_>,
        _: std::collections::HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        Err(ToolError::ExecutionFailed(self.said.clone()))
    }
}

#[cfg(test)]
pub(crate) mod tests;

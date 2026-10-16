//! MCP servers: programs that give the agents more tools over the Model
//! Context Protocol. Arko starts the servers set up in the files
//! `--mcp-config` names, in the project's `.mcp.json` (once the user
//! approves them) and in `mcp.json` in Arko's folder, and offers their tools
//! to the main agent and the workers as `mcp__<server>__<tool>`. Each Arko
//! process starts its own servers, and stops them when it exits.
//!
//! A file is in Claude Code's format, servers by name:
//! `{"mcpServers": {"github": {"command": "npx", "args": ["-y", "..."], "env": {"TOKEN": "${GITHUB_TOKEN}"}}}}`,
//! or in arko's, a list of servers that have a `name`. `${VAR}` and
//! `${VAR:-default}` in a command, its arguments and its environment are
//! taken from Arko's environment. Servers talk over standard input and
//! output: HTTP servers (`url`, or a `type` other than `stdio`) and servers
//! marked `"disabled": true` are skipped.
//!
//! A project's `.mcp.json` would run programs as soon as Arko starts in its
//! folder, so Arko asks before starting them, and remembers the answer (in
//! `mcp-approvals.json` in its folder) until the file's servers change.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arko_agents::mcp::McpTool;
use arko_agents::tool::{ToolOutput, ToolRecovery};
use arko_agents::{
    McpConfig, McpManager, McpServerConfig, TaskContext, Tool, ToolDefinition, ToolError,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The servers set up for every project, in Arko's folder.
pub const USER_FILE: &str = "mcp.json";
/// A project's servers, in the working directory (as Claude Code reads it).
pub const PROJECT_FILE: &str = ".mcp.json";
/// The answers about projects' servers, in Arko's folder.
const APPROVALS_FILE: &str = "mcp-approvals.json";
/// How long a server gets to start and list its tools.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest tool name the model providers take (`^[a-zA-Z0-9_-]{1,64}$`).
const MAX_TOOL_NAME: usize = 64;

/// A server to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The command line as the file writes it, before `${VAR}`s are
    /// expanded: what the user is shown.
    pub shown: String,
    /// The file that sets it up.
    pub file: PathBuf,
}

impl Server {
    fn config(&self) -> McpServerConfig {
        McpServerConfig {
            name: self.name.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone().into_iter().collect(),
        }
    }
}

/// A configuration file's servers, and why it skips the others.
#[derive(Debug, Default)]
pub struct Parsed {
    pub servers: Vec<Server>,
    pub skipped: Vec<String>,
    /// The file's `mcpServers`, as written: what a project's approval is for.
    raw: Value,
}

/// Reads the servers in the file at `path`. `lookup` gives the values of
/// `${VAR}`s.
pub fn parse_file(path: &Path, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Parsed, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    parse(&text, path, lookup).map_err(|error| format!("{}: {error}", path.display()))
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
    let entries: Vec<(Option<String>, &Value)> = match &raw {
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
    };
    let mut parsed = Parsed::default();
    for (index, (name, entry)) in entries.into_iter().enumerate() {
        let Some(name) = name.filter(|name| !name.trim().is_empty()) else {
            parsed
                .skipped
                .push(format!("server {} has no name", index + 1));
            continue;
        };
        match server(&name, entry, file, lookup) {
            Ok(Some(server)) => parsed.servers.push(server),
            Ok(None) => {}
            Err(why) => parsed.skipped.push(format!("`{name}` {why}")),
        }
    }
    parsed.raw = raw;
    Ok(parsed)
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
    let env = match entry.get("env") {
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
        shown,
        file: file.to_path_buf(),
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

fn environment(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// A project's servers that wait for the user's answer before they start:
/// what a front end shows when it asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpApproval {
    /// The project's `.mcp.json`.
    pub file: PathBuf,
    /// Each server's name, and its command line as the file writes it.
    pub servers: Vec<(String, String)>,
    /// The servers the file sets up that Arko skips, and why.
    pub skipped: Vec<String>,
}

/// The servers to start, as [`plan`] finds them, before the user answers
/// about the project's (`approval`).
#[derive(Debug)]
pub struct Plan {
    /// The project's servers, when they wait for the user's answer.
    pub approval: Option<McpApproval>,
    steps: Vec<Step>,
    root: PathBuf,
    workspace: PathBuf,
}

/// What [`Plan::decide`] does, in order.
#[derive(Debug)]
enum Step {
    /// Starts a file's servers.
    Add(Parsed),
    /// Starts the project's servers if the user agrees.
    Ask(Parsed),
    Note(String),
}

/// The servers to start: those in `files` (from `--mcp-config`), in the
/// project's `.mcp.json` when the user approves them, and in Arko's own
/// `mcp.json`. A name set up twice is taken from the first of these. The
/// user's answer about the project's servers, unless one is remembered for
/// them, is for [`Plan::decide`].
pub fn plan(root: &Path, workspace: &Path, files: &[PathBuf]) -> Result<Plan, String> {
    let mut steps = Vec::new();
    let mut approval = None;
    let mut read = HashSet::new();
    for file in files {
        let file = arko_agents::utils::normalize_path(
            &std::path::absolute(file).unwrap_or_else(|_| file.clone()),
        );
        steps.push(Step::Add(parse_file(&file, &environment)?));
        read.insert(file);
    }
    let project = workspace.join(PROJECT_FILE);
    if project.is_file() && !read.contains(&project) {
        match parse_file(&project, &environment) {
            Ok(parsed) if parsed.servers.is_empty() && parsed.skipped.is_empty() => {}
            Ok(parsed) => match remembered(root, workspace, &parsed) {
                Some(true) => steps.push(Step::Add(parsed)),
                Some(false) => {}
                None => {
                    approval = Some(McpApproval {
                        file: project.clone(),
                        servers: parsed
                            .servers
                            .iter()
                            .map(|server| (server.name.clone(), server.shown.clone()))
                            .collect(),
                        skipped: parsed.skipped.clone(),
                    });
                    steps.push(Step::Ask(parsed));
                }
            },
            Err(error) => steps.push(Step::Note(format!("MCP servers not started: {error}"))),
        }
    }
    let user = root.join(USER_FILE);
    if user.is_file() && !read.contains(&user) {
        match parse_file(&user, &environment) {
            Ok(parsed) => steps.push(Step::Add(parsed)),
            Err(error) => steps.push(Step::Note(format!("MCP servers not started: {error}"))),
        }
    }
    Ok(Plan {
        approval,
        steps,
        root: root.to_path_buf(),
        workspace: workspace.to_path_buf(),
    })
}

impl Plan {
    /// The servers to start, with `answer` about the project's servers
    /// (`None`: no one could answer, so they don't start), and the notes
    /// that say what is left out and why. An answer is remembered until the
    /// file's servers change.
    pub fn decide(self, answer: Option<bool>) -> Result<(Vec<Server>, Vec<String>), String> {
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
        for step in self.steps {
            match step {
                Step::Add(parsed) => add(parsed, &mut notes),
                Step::Note(note) => notes.push(note),
                Step::Ask(parsed) => match answer {
                    Some(start) => {
                        remember(&self.root, &self.workspace, &parsed, start)?;
                        if start {
                            add(parsed, &mut notes);
                        }
                    }
                    None => notes.push(format!(
                        "the MCP servers in {} aren't started: Arko asks before starting a project's servers, and needs a terminal to ask",
                        self.workspace.join(PROJECT_FILE).display()
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
    /// By working directory.
    #[serde(default)]
    projects: BTreeMap<String, Approval>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Approval {
    /// Whether to start them.
    start: bool,
    /// The `mcpServers` the answer is for.
    #[serde(rename = "mcpServers")]
    servers: Value,
}

fn approvals(root: &Path) -> Approvals {
    std::fs::read_to_string(root.join(APPROVALS_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The answer remembered for the project's servers, if any (for these
/// servers: a change to them asks again).
fn remembered(root: &Path, workspace: &Path, parsed: &Parsed) -> Option<bool> {
    approvals(root)
        .projects
        .get(&workspace.display().to_string())
        .filter(|approval| approval.servers == parsed.raw)
        .map(|approval| approval.start)
}

/// Remembers the user's answer about the project's servers.
fn remember(root: &Path, workspace: &Path, parsed: &Parsed, start: bool) -> Result<(), String> {
    let path = root.join(APPROVALS_FILE);
    let mut approvals = approvals(root);
    approvals.projects.insert(
        workspace.display().to_string(),
        Approval {
            start,
            servers: parsed.raw.clone(),
        },
    );
    let text = serde_json::to_string_pretty(&approvals).map_err(|error| error.to_string())?;
    let temporary = root.join(format!("{APPROVALS_FILE}.tmp"));
    std::fs::create_dir_all(root)
        .and_then(|()| std::fs::write(&temporary, text))
        .and_then(|()| std::fs::rename(&temporary, &path))
        .map_err(|error| format!("could not save {}: {error}", path.display()))
}

/// The tool name of `server`'s tool `tool`: `mcp__<server>__<tool>`, with
/// the characters the providers don't take replaced by `_`, and cut to 64
/// characters (ending in a hash of the full name, to stay unique).
pub fn tool_name(server: &str, tool: &str) -> String {
    let clean = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let name = format!("mcp__{}__{}", clean(server), clean(tool));
    if name.len() <= MAX_TOOL_NAME {
        return name;
    }
    // FNV-1a: stable across Rust versions, as names saved in a session's
    // checkpoints must be.
    let hash = format!("{server}\n{tool}")
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    let hash = format!("{:08x}", hash as u32);
    format!("{}_{hash}", &name[..MAX_TOOL_NAME - hash.len() - 1])
}

/// The server and tool an MCP tool name stands for, to show it.
pub fn split_tool_name(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix("mcp__")?.split_once("__")
}

/// An MCP tool under a name the providers take.
#[derive(Debug)]
struct ServerTool {
    name: String,
    tool: Arc<McpTool>,
}

#[async_trait]
impl Tool for ServerTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            ..self.tool.definition()
        }
    }

    fn allow_result_interception(&self) -> bool {
        self.tool.allow_result_interception()
    }

    fn uses_host_timeout(&self) -> bool {
        self.tool.uses_host_timeout()
    }

    fn interruptible(&self) -> bool {
        self.tool.interruptible()
    }

    fn recovery_mode(&self) -> ToolRecovery {
        self.tool.recovery_mode()
    }

    async fn execute(
        &self,
        context: &TaskContext<'_>,
        params: std::collections::HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        self.tool.execute(context, params).await
    }

    async fn execute_with_output(
        &self,
        context: &TaskContext<'_>,
        params: std::collections::HashMap<String, Value>,
    ) -> Result<ToolOutput, ToolError> {
        self.tool.execute_with_output(context, params).await
    }
}

/// A server Arko tried to start.
pub struct Started {
    pub server: Server,
    /// Its tools' names, or why it didn't start.
    pub result: Result<Vec<String>, String>,
}

/// The servers running for this Arko process.
#[derive(Default)]
pub struct Running {
    pub servers: Vec<Started>,
    tools: Vec<Arc<dyn Tool>>,
    managers: Vec<McpManager>,
}

impl Running {
    /// Starts `servers` at the same time, each given a minute to start.
    pub async fn start(servers: Vec<Server>) -> Self {
        let starting: Vec<_> = servers
            .into_iter()
            .map(|server| {
                let config = McpConfig {
                    mcp_servers: vec![server.config()],
                };
                let task = tokio::spawn(async move {
                    match tokio::time::timeout(STARTUP_TIMEOUT, McpManager::from_config(&config))
                        .await
                    {
                        Ok(Ok(manager)) => Ok(manager),
                        Ok(Err(error)) => Err(error.to_string()),
                        Err(_) => Err(format!(
                            "it didn't start within {} s",
                            STARTUP_TIMEOUT.as_secs()
                        )),
                    }
                });
                (server, task)
            })
            .collect();
        let mut running = Self::default();
        let mut taken = HashSet::new();
        for (server, task) in starting {
            let manager = task.await.unwrap_or_else(|error| Err(error.to_string()));
            let result = manager.map(|manager| {
                let mut names = Vec::new();
                for tool in manager.tools() {
                    let name = tool_name(&server.name, tool.remote_tool_name());
                    if !taken.insert(name.clone()) {
                        log::warn!(
                            "MCP server '{}': tool '{}' is left out, as its name {name} is taken",
                            server.name,
                            tool.remote_tool_name()
                        );
                        continue;
                    }
                    running.tools.push(Arc::new(ServerTool {
                        name: name.clone(),
                        tool: tool.clone(),
                    }));
                    names.push(name);
                }
                running.managers.push(manager);
                names
            });
            running.servers.push(Started { server, result });
        }
        running
    }

    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// The names of the tools, in the servers' order.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    /// The names of the servers that started.
    pub fn started(&self) -> Vec<String> {
        self.servers
            .iter()
            .filter(|started| started.result.is_ok())
            .map(|started| started.server.name.clone())
            .collect()
    }

    /// The servers, one line each: `github (12 tools)`.
    pub fn summary(&self) -> Option<String> {
        if self.servers.is_empty() {
            return None;
        }
        let servers = self
            .servers
            .iter()
            .map(|started| match &started.result {
                Ok(tools) => format!(
                    "{} ({} tool{})",
                    started.server.name,
                    tools.len(),
                    if tools.len() == 1 { "" } else { "s" }
                ),
                Err(_) => format!("{} (failed)", started.server.name),
            })
            .collect::<Vec<_>>();
        Some(format!("MCP: {}", servers.join(", ")))
    }

    /// Stops the servers: each gets three seconds to exit, then is killed.
    pub async fn shutdown(&mut self) {
        let stopping: Vec<_> = self
            .managers
            .drain(..)
            .map(|manager| tokio::spawn(manager.shutdown()))
            .collect();
        for task in stopping {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(name: &str) -> Option<String> {
        match name {
            "TOKEN" => Some("secret".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    fn parsed(text: &str) -> Parsed {
        parse(text, Path::new("/x/.mcp.json"), &lookup).unwrap()
    }

    #[test]
    fn both_formats_are_read() {
        let claude = parsed(
            r#"{"mcpServers": {"github": {"command": "npx", "args": ["-y", "server-github"], "env": {"GITHUB_TOKEN": "${TOKEN}"}}}}"#,
        );
        assert!(claude.skipped.is_empty());
        let server = &claude.servers[0];
        assert_eq!(server.name, "github");
        assert_eq!(server.command, "npx");
        assert_eq!(server.args, ["-y", "server-github"]);
        assert_eq!(server.env["GITHUB_TOKEN"], "secret");
        assert_eq!(server.shown, "npx -y server-github");
        assert_eq!(server.file, Path::new("/x/.mcp.json"));

        let arko = parsed(
            r#"{"mcpServers": [{"name": "files", "command": "fs-server", "args": ["/tmp", 8080]}, {"command": "nameless"}]}"#,
        );
        assert_eq!(arko.servers.len(), 1);
        assert_eq!(arko.servers[0].name, "files");
        assert_eq!(arko.servers[0].args, ["/tmp", "8080"]);
        assert_eq!(arko.skipped, ["server 2 has no name"]);
    }

    #[test]
    fn http_disabled_and_broken_servers_are_skipped() {
        let parsed = parsed(
            r#"{"mcpServers": {
                "remote": {"type": "http", "url": "https://example.com/mcp"},
                "bare": {"url": "https://example.com/sse"},
                "off": {"command": "x", "disabled": true},
                "unset": {"command": "x", "args": ["--token=${MISSING}"]},
                "empty": {"command": " "},
                "ok": {"type": "stdio", "command": "run", "args": ["${MISSING:-fallback}", "${EMPTY:-used}", "a${TOKEN}b"]}
            }}"#,
        );
        assert_eq!(parsed.servers.len(), 1);
        assert_eq!(parsed.servers[0].args, ["fallback", "used", "asecretb"]);
        let skipped = parsed.skipped.join("\n");
        assert!(skipped.contains("`remote` is a `http` server"), "{skipped}");
        assert!(skipped.contains("`bare` is an HTTP server"), "{skipped}");
        assert!(
            skipped.contains("`unset` uses ${MISSING}, which isn't set"),
            "{skipped}"
        );
        assert!(skipped.contains("`empty` has no `command`"), "{skipped}");
        assert!(!skipped.contains("off"), "{skipped}");
        assert!(parse("{}", Path::new("f"), &lookup).is_err());
        assert!(parse("not json", Path::new("f"), &lookup).is_err());
    }

    #[test]
    fn variables_expand_and_text_without_them_stays() {
        assert_eq!(
            expand("plain $HOME {x}", &lookup).unwrap(),
            "plain $HOME {x}"
        );
        assert_eq!(expand("${TOKEN}${TOKEN}", &lookup).unwrap(), "secretsecret");
        assert_eq!(expand("${EMPTY}", &lookup).unwrap(), "");
        assert_eq!(expand("open ${TOKEN", &lookup).unwrap(), "open ${TOKEN");
    }

    #[test]
    fn tool_names_are_ones_the_providers_take() {
        assert_eq!(
            tool_name("github", "create_issue"),
            "mcp__github__create_issue"
        );
        assert_eq!(
            tool_name("my server", "files.read"),
            "mcp__my_server__files_read"
        );
        let long = tool_name(
            "a-server-with-a-long-name",
            "a_tool_with_an_even_longer_name_than_that",
        );
        let other = tool_name(
            "a-server-with-a-long-name",
            "a_tool_with_an_even_longer_name_than_this",
        );
        assert_eq!(long.len(), 64);
        assert_ne!(long, other);
        // The same name every time: sessions keep tool names.
        assert_eq!(
            long,
            tool_name(
                "a-server-with-a-long-name",
                "a_tool_with_an_even_longer_name_than_that"
            )
        );
        let valid = |name: &str| {
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        };
        assert!(valid(&long) && valid(&tool_name("ü s", "ö.t")));
        assert_eq!(
            split_tool_name("mcp__github__create_issue"),
            Some(("github", "create_issue"))
        );
        assert_eq!(split_tool_name("shell"), None);
    }

    /// The files are read in order, a name is taken from the first file
    /// that has it, and a project's servers start only when approved.
    #[test]
    fn servers_come_from_the_flag_the_project_and_fins_folder() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            root.join(USER_FILE),
            r#"{"mcpServers": {"shared": {"command": "user-shared"}, "mine": {"command": "user-mine"}}}"#,
        )
        .unwrap();
        let flag = temp.path().join("extra.json");
        std::fs::write(
            &flag,
            r#"{"mcpServers": {"shared": {"command": "flag-shared"}}}"#,
        )
        .unwrap();
        let (servers, notes) = servers_of(&root, &workspace, std::slice::from_ref(&flag));
        assert_eq!(servers, ["shared=flag-shared", "mine=user-mine"]);
        assert!(notes[0].contains("`shared`"), "{notes:?}");

        // A project's servers wait for an answer, which needs a terminal.
        let project = r#"{"mcpServers": {"proj": {"command": "project-server"}}}"#;
        std::fs::write(workspace.join(PROJECT_FILE), project).unwrap();
        let (servers, notes) = servers_of(&root, &workspace, &[]);
        // A file's servers by name come in the order of their names.
        assert_eq!(servers, ["mine=user-mine", "shared=user-shared"]);
        assert!(
            notes.iter().any(|note| note.contains("aren't started")),
            "{notes:?}"
        );

        // Approved, they start; changed, they wait again.
        let mut approvals = Approvals::default();
        approvals.projects.insert(
            workspace.display().to_string(),
            Approval {
                start: true,
                servers: serde_json::from_str::<Value>(project).unwrap()["mcpServers"].clone(),
            },
        );
        std::fs::write(
            root.join(APPROVALS_FILE),
            serde_json::to_string(&approvals).unwrap(),
        )
        .unwrap();
        let (servers, _) = servers_of(&root, &workspace, &[]);
        assert_eq!(
            servers,
            [
                "proj=project-server",
                "mine=user-mine",
                "shared=user-shared"
            ]
        );
        std::fs::write(
            workspace.join(PROJECT_FILE),
            r#"{"mcpServers": {"proj": {"command": "something-else"}}}"#,
        )
        .unwrap();
        let (servers, _) = servers_of(&root, &workspace, &[]);
        assert_eq!(servers, ["mine=user-mine", "shared=user-shared"]);
        // Named with --mcp-config, they start without an answer.
        let (servers, _) = servers_of(&root, &workspace, &[workspace.join(PROJECT_FILE)]);
        assert_eq!(
            servers,
            [
                "proj=something-else",
                "mine=user-mine",
                "shared=user-shared"
            ]
        );
        // A file --mcp-config names must be readable.
        assert!(plan(&root, &workspace, &[temp.path().join("missing.json")]).is_err());
    }

    /// The project's servers wait for the front end's answer: the plan
    /// says what it would ask, the answer starts them or not, and is
    /// remembered until the file's servers change. With no answer (no one
    /// can ask), they don't start.
    #[test]
    fn project_mcp_servers_wait_for_the_prompters_answer() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join(PROJECT_FILE),
            r#"{"mcpServers": {"proj": {"command": "project-server", "args": ["--fast"]}, "web": {"url": "https://example.com"}}}"#,
        )
        .unwrap();
        let names = |servers: &[Server]| {
            servers
                .iter()
                .map(|server| server.name.clone())
                .collect::<Vec<_>>()
        };
        let asked = plan(&root, &workspace, &[]).unwrap();
        let approval = asked.approval.clone().unwrap();
        assert_eq!(approval.file, workspace.join(PROJECT_FILE));
        assert_eq!(
            approval.servers,
            [("proj".to_string(), "project-server --fast".to_string())]
        );
        assert_eq!(approval.skipped.len(), 1, "{approval:?}");
        // No one to ask: they don't start, and nothing is remembered.
        let (servers, notes) = asked.decide(None).unwrap();
        assert!(servers.is_empty());
        assert!(
            notes.iter().any(|note| note.contains("aren't started")),
            "{notes:?}"
        );
        assert!(plan(&root, &workspace, &[]).unwrap().approval.is_some());
        // Yes: they start, and the next session doesn't ask.
        let (servers, _) = plan(&root, &workspace, &[])
            .unwrap()
            .decide(Some(true))
            .unwrap();
        assert_eq!(names(&servers), ["proj"]);
        let remembered = plan(&root, &workspace, &[]).unwrap();
        assert!(remembered.approval.is_none());
        assert_eq!(names(&remembered.decide(None).unwrap().0), ["proj"]);
        // Changed, it asks again; no is remembered too.
        std::fs::write(
            workspace.join(PROJECT_FILE),
            r#"{"mcpServers": {"proj": {"command": "other-server"}}}"#,
        )
        .unwrap();
        let changed = plan(&root, &workspace, &[]).unwrap();
        assert!(changed.approval.is_some());
        assert!(changed.decide(Some(false)).unwrap().0.is_empty());
        let refused = plan(&root, &workspace, &[]).unwrap();
        assert!(refused.approval.is_none());
        assert!(refused.decide(None).unwrap().0.is_empty());
    }

    fn servers_of(root: &Path, workspace: &Path, files: &[PathBuf]) -> (Vec<String>, Vec<String>) {
        let (servers, notes) = plan(root, workspace, files).unwrap().decide(None).unwrap();
        (
            servers
                .into_iter()
                .map(|server| format!("{}={}", server.name, server.command))
                .collect(),
            notes,
        )
    }

    /// A stub server over standard input and output (Python's standard
    /// library): its tools are offered under their MCP names, and called.
    #[tokio::test]
    async fn a_server_starts_and_its_tools_are_called_and_one_that_fails_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("stub.py");
        std::fs::write(&script, STUB_SERVER).unwrap();
        let python = std::env::var("ARKO_TEST_PYTHON")
            .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.into());
        let stub = Server {
            name: "stub server".into(),
            command: python,
            args: vec![script.display().to_string()],
            env: BTreeMap::from([("STUB_PREFIX".into(), "echo".into())]),
            shown: String::new(),
            file: PathBuf::new(),
        };
        let missing = Server {
            name: "missing".into(),
            command: "arko-no-such-mcp-server".into(),
            ..stub.clone()
        };
        let mut running = Running::start(vec![stub, missing]).await;
        assert_eq!(
            running.tool_names(),
            ["mcp__stub_server__echo", "mcp__stub_server__write_note"]
        );
        assert_eq!(running.started(), ["stub server"]);
        assert!(
            running.servers[1]
                .result
                .as_ref()
                .unwrap_err()
                .contains("arko-no-such-mcp-server")
        );
        assert_eq!(
            running.summary().unwrap(),
            "MCP: stub server (2 tools), missing (failed)"
        );
        let echo = running.tools()[0].clone();
        let definition = echo.definition();
        assert!(definition.description.contains("Echoes text"));
        assert!(definition.parallel_safe, "the stub marks echo read-only");
        let context = TaskContext::new(uuid::Uuid::new_v4(), "main", "test", temp.path());
        let output = echo
            .execute(
                &context,
                [("text".to_string(), Value::from("hi"))]
                    .into_iter()
                    .collect(),
            )
            .await
            .unwrap();
        assert_eq!(output, "echo: hi");
        running.shutdown().await;
    }

    const STUB_SERVER: &str = r#"
import json, os, sys
TOOLS = [
    {"name": "echo", "description": "Echoes text.", "annotations": {"readOnlyHint": True},
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "write.note", "description": "Writes a note.",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
]
def reply(id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id, "result": result}) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message.get("method")
    if method == "initialize":
        reply(message["id"], {"protocolVersion": message["params"]["protocolVersion"],
              "capabilities": {"tools": {}}, "serverInfo": {"name": "stub", "version": "1"}})
    elif method == "tools/list":
        reply(message["id"], {"tools": TOOLS})
    elif method == "tools/call":
        text = message["params"].get("arguments", {}).get("text", "")
        reply(message["id"], {"content": [{"type": "text", "text": os.environ["STUB_PREFIX"] + ": " + text}]})
    else:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": message["id"],
                         "error": {"code": -32601, "message": "unknown"}}) + "\n")
        sys.stdout.flush()
"#;
}

//! The main agent adds and removes MCP servers (the `mcp` actions `add`
//! and `remove`).
//!
//! An add is checked first, so the user is never asked about a server that
//! can't be written: its name, a command rather than a URL, and environment
//! variables by name only. A name another file sets up is refused, unless an
//! agent added it in the same file, which the add then replaces. Then the
//! user is asked, unless they turned off Ask before adding MCP servers. The
//! entry goes in the project's `.arko/mcp.json` (made through
//! [`crate::arko_dir::ensure`]) or in Arko's `mcp.json`, in Claude Code's
//! form, with each variable as `"NAME": "${NAME}"`, so its value comes from
//! Arko's environment when the server starts and no value is ever written,
//! shown or returned. The project file is remembered as approved, so the
//! next session starts it without asking, the record of what agents added
//! gets the server, and the hub starts it.
//!
//! A remove works only on a server the record says an agent added.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arko_agents::TaskContext;
use arko_agents::mcp::{AddServer, McpChanges, McpScope, ServerState};
use async_trait::async_trait;
use serde_json::Value;

use super::{Asked, Server, Servers, local_file, normal};
use crate::agent_added::{self, Scope, ServerRecord};
use crate::approvals::{Answer, Approval, Approvals, EnvName, ServerApproval};
use crate::remote::{Feed, Level};
use crate::ui::Output;

/// The longest name an agent may give a server.
const MAX_NAME: usize = 40;

/// Where a session's lines about servers go: the terminal and the feed.
#[derive(Clone)]
pub struct Notices {
    output: Arc<Output>,
    feed: Option<Feed>,
}

impl Notices {
    pub fn new(output: Arc<Output>, feed: Option<Feed>) -> Self {
        Self { output, feed }
    }

    pub(crate) fn info(&self, text: &str) {
        self.output.echo(&self.output.dim(text));
        if let Some(feed) = &self.feed {
            feed.notice(Level::Info, text);
        }
    }
}

/// Puts a server's card in the approval tray. (A seam for the tests, which
/// answer it themselves.)
#[async_trait]
pub trait Ask: Send + Sync {
    /// Whether anyone can answer.
    fn can_answer(&self) -> bool;
    /// Asks about `server` for the call of `context`, and waits for the
    /// answer.
    async fn ask(&self, context: &TaskContext<'_>, server: ServerApproval) -> Answer;
}

#[async_trait]
impl Ask for Approvals {
    fn can_answer(&self) -> bool {
        Approvals::can_answer(self)
    }

    async fn ask(&self, context: &TaskContext<'_>, server: ServerApproval) -> Answer {
        self.request(context, Approval::McpServer(server)).await
    }
}

/// Adds and removes nothing, for a definition.
pub(crate) struct NoChanges;

#[async_trait]
impl McpChanges for NoChanges {
    async fn add(&self, _: &TaskContext<'_>, _: AddServer) -> Result<String, String> {
        Err("No one can approve an MCP server in this session.".into())
    }

    async fn remove(&self, _: &TaskContext<'_>, _: &str, _: &str) -> Result<String, String> {
        Err("No one can remove MCP servers in this session.".into())
    }
}

/// Adds and removes the session's servers for the main agent.
pub struct Changes {
    /// Arko's folder.
    pub root: PathBuf,
    /// The project.
    pub workspace: PathBuf,
    /// The files `--mcp-config` (or an app) names, read before the others.
    pub given: Vec<PathBuf>,
    pub servers: Servers,
    pub ask: Arc<dyn Ask>,
    pub notices: Option<Notices>,
    /// The session, by ID.
    pub session: String,
    /// The session's name now.
    pub title: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

/// Why a server can't be taken out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotRemoved {
    /// No agent added it. The file is where it is set up.
    NotAdded(PathBuf),
    /// Its file couldn't be changed, with why.
    Failed(String),
}

/// Takes the server `name` an agent added out of `file`, and out of the
/// record. A project's `.arko/mcp.json` stays approved when it was. The
/// server keeps running in sessions open now.
pub fn take_out(root: &Path, file: &Path, name: &str) -> Result<(), NotRemoved> {
    let file = normal(file);
    if agent_added::server_in(root, &file, name).is_none() {
        return Err(NotRemoved::NotAdded(file));
    }
    {
        let _lock = super::approval_lock(root).map_err(NotRemoved::Failed)?;
        let changed = super::remove_entry(&file, name).map_err(NotRemoved::Failed)?;
        if let (Some((before, after)), Some(workspace)) = (changed, local_workspace(&file)) {
            let approvals = super::read_approvals(root).map_err(NotRemoved::Failed)?;
            if super::remembered_raw(&approvals, &workspace, Asked::Local, &before) == Some(true) {
                super::remember_locked(root, &workspace, Asked::Local, &after, true)
                    .map_err(NotRemoved::Failed)?;
            }
        }
    }
    agent_added::forget_server(root, &file, name).map_err(NotRemoved::Failed)?;
    Ok(())
}

/// The project whose `.arko/mcp.json` `file` is, if it is one.
fn local_workspace(file: &Path) -> Option<PathBuf> {
    let folder = file.parent()?;
    (file.file_name()? == super::LOCAL_FILE && folder.file_name()? == crate::arko_dir::DIR)
        .then(|| folder.parent().map(Path::to_path_buf))
        .flatten()
}

/// The name's problem, if it has one.
fn name_problem(name: &str) -> Option<String> {
    if name.chars().count() > MAX_NAME {
        return Some(format!(
            "A server's name is at most {MAX_NAME} characters, and {name} is longer."
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Some(format!(
            "A server's name takes letters, digits, - and _ only, so {name} won't do."
        ));
    }
    None
}

/// Whether `name` is an environment variable's name as `env` takes it.
fn env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The day an agent added a server, as a card says it ("26 Oct").
fn day(record: &ServerRecord) -> String {
    record
        .added
        .with_timezone(&chrono::Local)
        .format("%-d %b")
        .to_string()
}

/// What the scope reaches, as the results and lines say it.
fn reach(scope: Scope) -> &'static str {
    match scope {
        Scope::Project => "this project",
        Scope::User => "every project",
    }
}

impl Changes {
    /// The file a server of `scope` goes in.
    fn target(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::Project => local_file(&self.workspace),
            Scope::User => self.root.join(super::USER_FILE),
        }
    }

    /// The files a session reads servers from, in order.
    fn files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = Vec::new();
        for file in self
            .given
            .iter()
            .cloned()
            .chain([
                local_file(&self.workspace),
                self.workspace.join(super::PROJECT_FILE),
                self.root.join(super::USER_FILE),
            ])
            .map(|file| normal(&file))
        {
            if !files.contains(&file) {
                files.push(file);
            }
        }
        files
    }

    /// The checks before anyone is asked: the record of the server an add
    /// replaces, if any, or why it can't be added.
    fn check(&self, request: &AddServer, target: &Path) -> Result<Option<ServerRecord>, String> {
        let name = request.name.trim();
        if let Some(problem) = name_problem(name) {
            return Err(problem);
        }
        let command = request.command.trim();
        if command.contains("://") {
            return Err(
                "Arko talks to MCP servers over standard input and output only, so it can't add a server by its URL. Give the program to run in command."
                    .into(),
            );
        }
        for variable in &request.env {
            if variable.contains('=') {
                return Err(format!(
                    "env takes names only, such as GITHUB_TOKEN, and never values. {} has one.",
                    variable.split('=').next().unwrap_or_default().trim()
                ));
            }
            if !env_name(variable.trim()) {
                return Err(format!(
                    "{} isn't an environment variable's name as env takes it (capitals, digits and _, not starting with a digit).",
                    variable.trim()
                ));
            }
        }
        if request.why.trim().is_empty() {
            return Err(
                "Say in why what the server is for, why this scope, and whether workers get it. The user reads it on the card."
                    .into(),
            );
        }
        let replaced = agent_added::server_in(&self.root, target, name);
        for file in self.files() {
            if file.as_path() == target && replaced.is_some() {
                continue;
            }
            if super::sets_up(&file, name) {
                return Err(format!(
                    "{} sets up a server named {name} already. Choose another name.",
                    file.display()
                ));
            }
        }
        if replaced.is_none() && self.servers.hub.config(name).is_some() {
            return Err(format!(
                "This session has a server named {name} already. Choose another name."
            ));
        }
        Ok(replaced)
    }

    /// The card for `request`.
    fn card(
        &self,
        request: &AddServer,
        scope: Scope,
        replaced: Option<&ServerRecord>,
    ) -> ServerApproval {
        let ignored = scope == Scope::Project
            && if crate::arko_dir::dir(&self.workspace).is_dir() {
                crate::arko_dir::has_own_ignore(&self.workspace)
            } else {
                crate::profiles::Preferences::load(&self.root).keep_out_of_git
            };
        ServerApproval {
            name: request.name.trim().to_string(),
            scope: match scope {
                Scope::Project => crate::approvals::Scope::Project,
                Scope::User => crate::approvals::Scope::User,
            },
            command: std::iter::once(request.command.trim().to_string())
                .chain(request.args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" "),
            env: env_names(&request.env)
                .into_iter()
                .map(|name| EnvName {
                    set: super::environment(&name).is_some_and(|value| !value.is_empty()),
                    name,
                })
                .collect(),
            workers: request.workers,
            ignored,
            // The agent says what it checked in its reason, which the card
            // shows under Why.
            shared: request.shared.then(String::new),
            replaces: replaced.map(day),
            why: request.why.trim().to_string(),
        }
    }

    /// Writes the entry of `request` in `target`, and remembers a project's
    /// file as approved when it was before (or had no servers). Says whether
    /// it did.
    fn write(&self, request: &AddServer, scope: Scope) -> Result<(PathBuf, bool), String> {
        let name = request.name.trim();
        let mut entry = serde_json::Map::new();
        entry.insert("command".into(), Value::from(request.command.trim()));
        if !request.args.is_empty() {
            entry.insert("args".into(), Value::from(request.args.clone()));
        }
        let env = env_names(&request.env);
        if !env.is_empty() {
            entry.insert(
                "env".into(),
                Value::Object(
                    env.iter()
                        .map(|name| (name.clone(), Value::from(format!("${{{name}}}"))))
                        .collect(),
                ),
            );
        }
        if !request.workers {
            entry.insert("workers".into(), Value::Bool(false));
        }
        if request.shared {
            entry.insert("shared".into(), Value::Bool(true));
        }
        let _lock = super::approval_lock(&self.root)?;
        match scope {
            Scope::Project => {
                let file =
                    crate::arko_dir::ensure(&self.workspace, &self.root)?.join(super::LOCAL_FILE);
                let (before, after) = super::write_entry(&file, name, Value::Object(entry))?;
                let approvals = super::read_approvals(&self.root)?;
                let empty = super::entries(&before).map_or(true, |entries| entries.is_empty());
                let approved = empty
                    || super::remembered_raw(&approvals, &self.workspace, Asked::Local, &before)
                        == Some(true);
                if approved {
                    super::remember_locked(
                        &self.root,
                        &self.workspace,
                        Asked::Local,
                        &after,
                        true,
                    )?;
                }
                Ok((normal(&file), approved))
            }
            Scope::User => {
                std::fs::create_dir_all(&self.root).map_err(|error| {
                    format!(
                        "Couldn't make {} ({}).",
                        self.root.display(),
                        crate::profiles::reason_of(&error)
                    )
                })?;
                let file = self.root.join(super::USER_FILE);
                super::write_entry(&file, name, Value::Object(entry))?;
                Ok((normal(&file), true))
            }
        }
    }

    /// The server as the hub starts it, read back from its entry in `file`.
    fn written(&self, file: &Path, name: &str) -> Result<Server, String> {
        let parsed = super::parse_file(file, &super::environment)?;
        if let Some(server) = parsed
            .servers
            .into_iter()
            .find(|server| server.name == name)
        {
            return Ok(server);
        }
        let why = parsed
            .left_out
            .into_iter()
            .find(|(left, ..)| left == name)
            .map(|(.., why)| why)
            .unwrap_or_else(|| "it isn't in the file".into());
        Err(why)
    }

    fn notice(&self, text: &str) {
        if let Some(notices) = &self.notices {
            notices.info(text);
        }
    }
}

/// The names in `env`, trimmed, each once, in order.
fn env_names(env: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in env.iter().map(|name| name.trim().to_string()) {
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

#[async_trait]
impl McpChanges for Changes {
    async fn add(&self, context: &TaskContext<'_>, request: AddServer) -> Result<String, String> {
        let scope = match request.scope {
            McpScope::User => Scope::User,
            _ => Scope::Project,
        };
        let target = normal(&self.target(scope));
        let replaced = self.check(&request, &target)?;
        let name = request.name.trim().to_string();
        let asks = crate::extension_prefs::load(&self.root).ask_before_adding_mcp;
        let mut words = None;
        if asks {
            if !self.ask.can_answer() {
                return Err("No one can approve an MCP server in this session.".into());
            }
            let card = self.card(&request, scope, replaced.as_ref());
            match self.ask.ask(context, card).await {
                Answer::Approved { words: said, .. } => words = said,
                Answer::Unanswered => {
                    return Ok(format!("The user didn't answer, so {name} wasn't added."));
                }
                Answer::Rejected { words: None } => {
                    return Ok(format!(
                        "The user chose not to add {name} for {}, so don't ask again unless they want it.",
                        reach(scope)
                    ));
                }
                Answer::Rejected { words: Some(said) } => {
                    // What they wrote is only the agent's to read.
                    self.notice("Not added. Arko asks again.");
                    return Ok(format!(
                        "The user didn't add {name} for {} and wrote “{}”. If those words ask for changes to the server, make them and call add again, which shows them the changed server. If they don't (a new request, or words about something else), they are a message to you, not changes. Respond to it, call add again with the same server, and end with a short reminder that it is still waiting for their decision.",
                        reach(scope),
                        said.trim()
                    ));
                }
            }
        }
        let (file, approved) = self.write(&request, scope)?;
        agent_added::record_server(
            &self.root,
            ServerRecord {
                file: file.clone(),
                name: name.clone(),
                session: self.session.clone(),
                session_title: (self.title)(),
                added: chrono::Utc::now(),
                why: request.why.trim().to_string(),
            },
        )?;
        let started = match self.written(&file, &name) {
            Ok(server) => self
                .servers
                .add(server, Some(request.why.trim().to_string()))
                .await
                .map_err(|error| error.trim_end_matches('.').to_string()),
            Err(why) => Err(why),
        };
        let mut result = format!("Added {name} for {}.", reach(scope));
        match &started {
            Ok(facts) if facts.state == ServerState::Running => {
                self.notice(&format!(
                    "The main agent added the MCP server {name} for {}.",
                    reach(scope)
                ));
                result.push_str(&match facts.tools {
                    Some(1) => " It started with 1 tool. List it with action tools.".to_string(),
                    Some(count) => {
                        format!(" It started with {count} tools. List them with action tools.")
                    }
                    None => " It started. List its tools with action tools.".to_string(),
                });
            }
            Ok(facts) => {
                let reason = match &facts.state {
                    ServerState::Failed(reason) => reason.clone(),
                    _ => "it isn't running".to_string(),
                };
                self.notice(&format!(
                    "The main agent added the MCP server {name}, but it didn't start ({reason})."
                ));
                result.push_str(&format!(
                    " It didn't start, because {}. Action start tries again.",
                    reason.trim_end_matches('.')
                ));
            }
            Err(reason) => {
                self.notice(&format!(
                    "The main agent added the MCP server {name}, but it didn't start ({reason})."
                ));
                result.push_str(&format!(
                    " It didn't start, because {}. The next session starts it once that's fixed.",
                    reason.trim_end_matches('.')
                ));
            }
        }
        if !approved {
            result.push_str(&format!(
                " The user hasn't approved the other servers in {}, so the next session asks them before it starts the file's servers.",
                file.display()
            ));
        }
        if let Some(words) = words {
            result.push_str(&format!(
                " The user approved it and wrote “{}”. Act on what they wrote.",
                words.trim()
            ));
        }
        Ok(result)
    }

    async fn remove(
        &self,
        _context: &TaskContext<'_>,
        name: &str,
        _why: &str,
    ) -> Result<String, String> {
        let Some(server) = self.servers.server(name) else {
            return Err(format!("There's no MCP server named {name}."));
        };
        match take_out(&self.root, &server.file, name) {
            Ok(()) => {}
            Err(NotRemoved::NotAdded(file)) => {
                return Err(format!(
                    "{name} wasn't added by an agent, so it stays. The user can remove it from {}.",
                    file.display()
                ));
            }
            Err(NotRemoved::Failed(why)) => return Err(why),
        }
        self.servers.remove(name).await;
        self.notice(&format!("The main agent removed the MCP server {name}."));
        Ok(format!("Removed {name} and stopped it."))
    }
}

#[cfg(test)]
mod tests;

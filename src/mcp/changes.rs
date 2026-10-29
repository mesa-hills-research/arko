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
//! gets the server, and the hub starts it as the user approved it. The name's
//! checks are made again as the entry is written, and a change made while
//! the user decided (an entry of that name, or a change to the agent's own)
//! writes nothing. The record, the entry and the approval are written in
//! that order as one change, and a step that fails puts back the others.
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

/// What an add found of its server's name in the file it writes, before the
/// user was asked, which must still be so when it writes.
#[derive(Debug, PartialEq)]
struct Found {
    /// The name's entry there, as written.
    entry: Option<Value>,
    /// The record of the agent's server the add replaces.
    replaced: Option<ServerRecord>,
}

/// Where a name an add can't take is set up.
enum Taken {
    File(PathBuf),
    /// The session has a server of that name, from elsewhere.
    Session,
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

    /// The checks before anyone is asked: what the add found of the name in
    /// its file (an agent's server it replaces), or why it can't be added.
    fn check(&self, request: &AddServer, target: &Path) -> Result<Found, String> {
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
        self.found(name, target).map_err(|taken| match taken {
            Taken::File(file) => format!(
                "{} sets up a server named {name} already. Choose another name.",
                file.display()
            ),
            Taken::Session => {
                format!("This session has a server named {name} already. Choose another name.")
            }
        })
    }

    /// What the name `name` is in `target` now: free, or an agent's server
    /// there that an add replaces. Taken when another file sets it up (or
    /// `target` does, and no agent added it), or the session has a server of
    /// that name from elsewhere.
    fn found(&self, name: &str, target: &Path) -> Result<Found, Taken> {
        let replaced = agent_added::server_in(&self.root, target, name);
        for file in self.files() {
            if file.as_path() == target && replaced.is_some() {
                continue;
            }
            if super::sets_up(&file, name) {
                return Err(Taken::File(file));
            }
        }
        if replaced.is_none() && self.servers.hub.config(name).is_some() {
            return Err(Taken::Session);
        }
        Ok(Found {
            entry: super::entry_in(target, name),
            replaced,
        })
    }

    /// Checks again, with the approvals' lock held, that what the add
    /// found before the user was asked (`before`) is still so, or says what
    /// changed while they decided.
    fn unchanged(&self, name: &str, target: &Path, before: &Found) -> Result<(), String> {
        let wrote_nothing = "while the user decided, so Arko wrote nothing";
        match self.found(name, target) {
            Ok(now) if now == *before => Ok(()),
            Ok(_) => Err(format!(
                "{name} changed in {} {wrote_nothing}. Call add again, which asks them about it as it is now.",
                target.display()
            )),
            Err(Taken::File(file)) => Err(format!(
                "A server named {name} was set up in {} {wrote_nothing}. Choose another name, and call add again.",
                file.display()
            )),
            Err(Taken::Session) => Err(format!(
                "This session got a server named {name} {wrote_nothing}. Choose another name, and call add again."
            )),
        }
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

    /// Writes `entry` as the server `name` in the file of `scope`, as one
    /// change with the approvals' lock held, once it checked that what the
    /// add found there (`found`) didn't change while the user decided. The
    /// record of what agents added gets `record` first, so an entry is never
    /// written without it. Then the entry is written, and a project's file is
    /// remembered as approved when it was before (or had no servers). A step
    /// that fails puts back what those before it changed. Says where it
    /// wrote, and whether the file is approved.
    fn write(
        &self,
        entry: Value,
        scope: Scope,
        found: &Found,
        mut record: ServerRecord,
    ) -> Result<(PathBuf, bool), String> {
        let name = record.name.clone();
        let _lock = super::approval_lock(&self.root)?;
        self.unchanged(&name, &normal(&self.target(scope)), found)?;
        let file = match scope {
            Scope::Project => {
                crate::arko_dir::ensure(&self.workspace, &self.root)?.join(super::LOCAL_FILE)
            }
            Scope::User => {
                std::fs::create_dir_all(&self.root).map_err(|error| {
                    format!(
                        "Couldn't make {} ({}).",
                        self.root.display(),
                        crate::profiles::reason_of(&error)
                    )
                })?;
                self.root.join(super::USER_FILE)
            }
        };
        // What it changes is read first, so that it can be put back.
        let previous = match std::fs::read(&file) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "Couldn't read {} ({}).",
                    file.display(),
                    crate::profiles::reason_of(&error)
                ));
            }
        };
        let approvals = match scope {
            Scope::Project => Some(super::read_approvals(&self.root)?),
            Scope::User => None,
        };
        record.file = normal(&file);
        agent_added::record_server(&self.root, record.clone())
            .map_err(|why| format!("{} Arko didn't add {name}.", as_sentence(&why)))?;
        let undo = Undo {
            changes: self,
            name: &name,
            file: &file,
            scope,
            replaced: found.replaced.as_ref(),
        };
        let (before, after) =
            super::write_entry(&file, &name, entry).map_err(|why| undo.all(&why, None))?;
        // Another session's change to the record, made before the entry was
        // there to keep it, drops it. It goes in again.
        if agent_added::server_in(&self.root, &record.file, &name).as_ref() != Some(&record) {
            agent_added::record_server(&self.root, record)
                .map_err(|why| undo.all(&why, Some(previous.as_deref())))?;
        }
        let Some(approvals) = approvals else {
            return Ok((normal(&file), true));
        };
        let empty = super::entries(&before).map_or(true, |entries| entries.is_empty());
        let approved = empty
            || super::remembered_raw(&approvals, &self.workspace, Asked::Local, &before)
                == Some(true);
        if approved {
            super::remember_locked(&self.root, &self.workspace, Asked::Local, &after, true)
                .map_err(|why| undo.all(&why, Some(previous.as_deref())))?;
        }
        Ok((normal(&file), approved))
    }

    fn notice(&self, text: &str) {
        if let Some(notices) = &self.notices {
            notices.info(text);
        }
    }
}

/// Puts back what an add changed when a later step of it failed.
struct Undo<'a> {
    changes: &'a Changes,
    name: &'a str,
    /// The file the entry goes in.
    file: &'a Path,
    scope: Scope,
    /// The record of the agent's server the add replaces.
    replaced: Option<&'a ServerRecord>,
}

impl Undo<'_> {
    /// Puts back the file as it was when `previous` is given (the entry was
    /// written, and `None` is a file that wasn't there), and the record of
    /// what agents added, after a step failed with `why`. Says what failed,
    /// and what is left as it is. An entry that can't be taken out keeps its
    /// record, so an agent's remove takes it out.
    fn all(&self, why: &str, previous: Option<Option<&[u8]>>) -> String {
        let mut said = format!("{} Arko didn't add {}", as_sentence(why), self.name);
        if let Some(previous) = previous {
            let put_back = match previous {
                Some(text) => crate::session::write_whole(self.file, text),
                None => std::fs::remove_file(self.file),
            };
            if let Err(error) = put_back {
                said.push_str(&format!(
                    ", but couldn't put {} back as it was ({}), so the new entry is there. It's recorded as an agent's, so remove takes it out.",
                    self.file.display(),
                    crate::profiles::reason_of(&error)
                ));
                said.push_str(match self.scope {
                    Scope::Project => " The next session asks the user before it starts it.",
                    Scope::User => " The next session starts it unless it's removed.",
                });
                return said;
            }
        }
        let root = &self.changes.root;
        let record = match self.replaced {
            Some(replaced) => agent_added::record_server(root, replaced.clone()),
            None => agent_added::forget_server(root, &normal(self.file), self.name).map(|_| ()),
        };
        match record {
            Ok(()) => said.push_str(", and changed nothing."),
            Err(error) => said.push_str(&format!(
                ", and left {} as it was, but couldn't put back the record of what agents added ({}).",
                self.file.display(),
                error.trim_end_matches('.')
            )),
        }
        said
    }
}

/// `why` as a sentence of its own, starting with a capital and ending with
/// a period.
fn as_sentence(why: &str) -> String {
    let why = crate::ui::sentence(why.trim());
    if why.ends_with(['.', '!', '?']) {
        why
    } else {
        format!("{why}.")
    }
}

/// The entry `request` writes in its file, in Claude Code's form, each
/// variable as `${NAME}`.
fn entry_of(request: &AddServer) -> Value {
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
    Value::Object(entry)
}

/// The server the user approved, as the hub starts it: its `entry` read as
/// a file's entry is, with its variables' values taken now. (The file it
/// was written to may change before then.)
fn approved_server(name: &str, entry: &Value, file: &Path) -> Result<Server, String> {
    match super::server(name, entry, file, &super::environment) {
        Ok(Some(server)) => Ok(server),
        Ok(None) => Err("it is disabled".into()),
        Err(why) => Err(format!("it {why}")),
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
        let found = self.check(&request, &target)?;
        let name = request.name.trim().to_string();
        let asks = crate::extension_prefs::load(&self.root).ask_before_adding_mcp;
        let mut words = None;
        if asks {
            if !self.ask.can_answer() {
                return Err("No one can approve an MCP server in this session.".into());
            }
            let card = self.card(&request, scope, found.replaced.as_ref());
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
        let entry = entry_of(&request);
        let record = ServerRecord {
            file: target,
            name: name.clone(),
            session: self.session.clone(),
            session_title: (self.title)(),
            added: chrono::Utc::now(),
            why: request.why.trim().to_string(),
        };
        let (file, approved) = self.write(entry.clone(), scope, &found, record)?;
        let started = match approved_server(&name, &entry, &file) {
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
        // A server of the session, or one an agent added that it doesn't
        // run (one that didn't start).
        let file = match self.servers.server(name) {
            Some(server) => server.file,
            None => match self
                .not_started()
                .into_iter()
                .find(|(record, _)| record.name == name)
            {
                Some((record, _)) => record.file,
                None => return Err(format!("There's no MCP server named {name}.")),
            },
        };
        match take_out(&self.root, &file, name) {
            Ok(()) => {}
            Err(NotRemoved::NotAdded(file)) => {
                return Err(format!(
                    "{name} wasn't added by an agent, so it stays. The user can remove it from {}.",
                    file.display()
                ));
            }
            Err(NotRemoved::Failed(why)) => return Err(why),
        }
        let ran = self.servers.hub.config(name).is_some();
        self.servers.remove(name).await;
        self.notice(&format!("The main agent removed the MCP server {name}."));
        Ok(if ran {
            format!("Removed {name} and stopped it.")
        } else {
            format!("Removed {name}.")
        })
    }
}

impl Changes {
    /// The servers an agent added to a file this session reads that it
    /// doesn't run, each with why: one whose variable isn't set doesn't
    /// start, and one added since the session started starts in the next.
    pub(crate) fn not_started(&self) -> Vec<(ServerRecord, String)> {
        let files = self.files();
        agent_added::servers(&self.root)
            .into_iter()
            .filter(|record| {
                files.contains(&record.file) && self.servers.hub.config(&record.name).is_none()
            })
            .map(|record| {
                let why = match super::entry_in(&record.file, &record.name)
                    .map(|entry| approved_server(&record.name, &entry, &record.file))
                {
                    Some(Err(why)) => {
                        format!("{why}. The next session starts it once that's fixed")
                    }
                    _ => "it was added since this session started. The next session starts it"
                        .to_string(),
                };
                (record, why)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;

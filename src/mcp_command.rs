//! `/mcp`: the session's MCP servers. `/mcp` lists them with their tools
//! or state, where each is set up and who added it, `/mcp NAME` shows one
//! whole, and `/mcp remove NAME` removes a server an agent added.

use std::path::{Path, PathBuf};

use crate::details::{McpFacts, McpServerFacts, McpState};
use crate::mcp::changes::{NotRemoved, take_out};
use crate::session::Mode;
use crate::ui::Output;
use crate::{Harness, agent_added, mcp};

/// The longest line of tool names `/mcp` shows for a server.
const TOOLS_WIDTH: usize = 100;

/// `path`, with the home directory written as `~`.
fn tilde(path: &Path) -> String {
    let home = std::env::home_dir();
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

/// The day an agent added a server: "28 Oct".
fn day(record: &agent_added::ServerRecord) -> String {
    record
        .added
        .with_timezone(&chrono::Local)
        .format("%-d %b")
        .to_string()
}

/// A server's first line after its name: its command line, its file, and
/// what's unusual about it.
fn about(server: &McpServerFacts) -> String {
    let mut parts = vec![crate::ui::cut(&server.shown, 100), tilde(&server.file)];
    if !server.workers {
        parts.push("main agent only".into());
    }
    if server.shared {
        parts.push("shared".into());
    }
    if let Some(added) = &server.added {
        parts.push(format!("added by the agent {}", day(added)));
    }
    parts.join(" · ")
}

/// The names of `tools`, cut to fit a line, with how many there are when
/// some are cut.
fn tool_names(tools: &[String]) -> String {
    let all = tools.join(", ");
    if all.chars().count() <= TOOLS_WIDTH {
        return all;
    }
    let mut shown = String::new();
    for name in tools {
        if shown.chars().count() + name.chars().count() + 2 > TOOLS_WIDTH {
            break;
        }
        shown.push_str(name);
        shown.push_str(", ");
    }
    format!("{shown}… ({} tools)", tools.len())
}

/// A server's state in words, for `/mcp` and `/mcp NAME`.
fn state_words(server: &McpServerFacts) -> String {
    match &server.state {
        McpState::Starting => "starting".into(),
        McpState::Running => match &server.tools {
            Ok(tools) if tools.len() == 1 => "running, 1 tool".into(),
            Ok(tools) => format!("running, {} tools", tools.len()),
            Err(_) => "running".into(),
        },
        McpState::Stopped => "stopped".into(),
        McpState::StoppedByAgent => "stopped by the agent".into(),
        McpState::Failed(reason) => format!("didn't start ({reason})"),
    }
}

/// What `/mcp` prints, a line and its look each.
pub(crate) fn list_lines(facts: &McpFacts, chat: bool) -> Vec<(Look, String)> {
    let mut lines = Vec::new();
    // (A chat's note says why it has none.)
    if facts.servers.is_empty() && !chat {
        lines.push((
            Look::Plain,
            format!(
                "No MCP servers. Arko starts those set up in {}, in the project's .arko/{} and {} (once you approve them) and in --mcp-config files. The main agent can add one with your approval.",
                facts.user_file.display(),
                mcp::LOCAL_FILE,
                mcp::PROJECT_FILE
            ),
        ));
    }
    for server in &facts.servers {
        lines.push((Look::Server(server.name.clone()), about(server)));
        lines.push(match (&server.state, &server.tools) {
            (McpState::Failed(_), _) => (Look::Error, format!("  {}", state_words(server))),
            (McpState::Running, Ok(tools)) if tools.is_empty() => (Look::Dim, "  no tools".into()),
            (McpState::Running, Ok(tools)) => (Look::Plain, format!("  {}", tool_names(tools))),
            _ => (Look::Dim, format!("  {}", state_words(server))),
        });
    }
    for note in &facts.notes {
        lines.push((Look::Dim, note.clone()));
    }
    if let Some(note) = &facts.unoffered {
        lines.push((Look::Warn, note.clone()));
    }
    if facts.adds_without_asking && !chat {
        lines.push((
            Look::Warn,
            crate::extension_prefs::ASK_MCP_OFF_LINE.to_string(),
        ));
    }
    lines
}

/// What `/mcp NAME` prints of `server`.
pub(crate) fn server_lines(server: &McpServerFacts) -> Vec<(Look, String)> {
    let mut lines = vec![
        (Look::Server(server.name.clone()), String::new()),
        (Look::Plain, server.shown.clone()),
        (Look::Dim, tilde(&server.file)),
    ];
    let row = |label: &str, text: String| (Look::Plain, format!("{label:<8} {text}"));
    if !server.env.is_empty() {
        let env: Vec<String> = server
            .env
            .iter()
            .map(|(name, set)| format!("{name} {}", if *set { "set" } else { "not set" }))
            .collect();
        lines.push(row("env", env.join(", ")));
    }
    lines.push(row(
        "workers",
        if server.workers {
            "can use it"
        } else {
            "can't use it"
        }
        .into(),
    ));
    if server.shared {
        lines.push(row(
            "shared",
            "sessions open at the same time share one copy".into(),
        ));
    }
    lines.push(row("state", state_words(server)));
    if let Ok(tools) = &server.tools
        && !tools.is_empty()
    {
        lines.push(row("tools", tools.join(", ")));
    }
    if let Some(added) = &server.added {
        let date = added
            .added
            .with_timezone(&chrono::Local)
            .format("%-d %b %Y");
        lines.push((
            Look::Plain,
            match &added.session_title {
                Some(title) => format!("Added by the agent in {title} on {date}"),
                None => format!("Added by the agent on {date}"),
            },
        ));
        lines.push(row("why", added.why.clone()));
        lines.push((
            Look::Dim,
            format!("/mcp remove {} removes it.", server.name),
        ));
    }
    lines
}

/// How a line of `/mcp` looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Look {
    Plain,
    Dim,
    Warn,
    Error,
    /// A server's first line, its name in bold before the text.
    Server(String),
}

fn print(out: &Output, lines: Vec<(Look, String)>) {
    for (look, text) in lines {
        out.line(&match look {
            Look::Plain => text,
            Look::Dim => out.dim(&text),
            Look::Warn => out.warn(&text),
            Look::Error => out.error(&text),
            Look::Server(name) if text.is_empty() => out.bold(&name),
            Look::Server(name) => format!("{}  {}", out.bold(&name), out.dim(&text)),
        });
    }
}

impl Harness {
    /// `/mcp`, `/mcp NAME` and `/mcp remove NAME`.
    pub(crate) fn mcp_command(&mut self, argument: &str) {
        let argument = argument.trim();
        if argument.is_empty() {
            self.list_mcp();
            return;
        }
        if let Some(name) = argument
            .strip_prefix("remove ")
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let said = self.remove_mcp_server(name);
            self.output.line(&said);
            return;
        }
        let facts = self.details.mcp();
        match facts.servers.iter().find(|server| server.name == argument) {
            Some(server) => print(&self.output, server_lines(server)),
            None => self.output.line(&no_server(argument, &facts)),
        }
    }

    /// `/mcp`: each server with its tools, or why it didn't start.
    pub(crate) fn list_mcp(&self) {
        let facts = self.details.mcp();
        print(
            &self.output,
            list_lines(&facts, self.settings.mode == Mode::Chat),
        );
    }

    /// `/mcp remove NAME`: removes the server `name` if an agent added it,
    /// and stops it in this session. Returns what to say.
    fn remove_mcp_server(&mut self, name: &str) -> String {
        let facts = self.details.mcp();
        // A server of this session, or one an agent added elsewhere (such
        // as another session, in Arko's mcp.json).
        let file: Option<PathBuf> = facts
            .servers
            .iter()
            .find(|server| server.name == name)
            .map(|server| server.file.clone())
            .or_else(|| {
                let records: Vec<_> = agent_added::servers(&self.root)
                    .into_iter()
                    .filter(|record| record.name == name)
                    .collect();
                match records.as_slice() {
                    [only] => Some(only.file.clone()),
                    _ => None,
                }
            });
        let Some(file) = file else {
            return no_server(name, &facts);
        };
        match take_out(&self.root, &file, name) {
            Ok(()) => {
                let running = self.mcp.servers.server(name).is_some();
                if running {
                    let servers = self.mcp.servers.clone();
                    let stopping = name.to_string();
                    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                        runtime.spawn(async move { servers.remove(&stopping).await });
                    }
                    format!("Removed the MCP server {name} and stopped it.")
                } else {
                    format!("Removed the MCP server {name}.")
                }
            }
            Err(NotRemoved::NotAdded(file)) => format!(
                "{name} wasn't added by an agent. Edit {} to remove it.",
                tilde(&file)
            ),
            Err(NotRemoved::Failed(why)) => self.output.error(&why),
        }
    }
}

/// What `/mcp NAME` says of a name no server has.
fn no_server(name: &str, facts: &McpFacts) -> String {
    let names: Vec<&str> = facts
        .servers
        .iter()
        .map(|server| server.name.as_str())
        .collect();
    match names.as_slice() {
        [] => format!("There's no MCP server named {name}."),
        names => format!(
            "There's no MCP server named {name}. The servers are {}.",
            crate::and_list(names)
        ),
    }
}

#[cfg(test)]
mod tests;

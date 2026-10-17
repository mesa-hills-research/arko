//! What a session asks its front end as it opens: the credentials it lacks,
//! and whether to start a project's MCP servers. The front end also hears
//! what Arko has to tell the user before the session's feed exists. The
//! command line asks at the terminal (`terminal::TerminalPrompter`); Arko
//! Studio asks in its own sheets.

use std::path::Path;

use async_trait::async_trait;

use crate::credentials::Need;
pub use crate::mcp::McpApproval;
use crate::remote::Level;

/// What a session asks its front end as it opens.
#[async_trait]
pub trait Prompter: Send + Sync {
    /// The session can't open without `need` (a key, a login). The front
    /// end lets the user add it, then returns `Ok` for Arko to check again,
    /// or why the session doesn't open: the user cancelled, or no one can be
    /// asked.
    async fn need_credentials(&self, need: &Need) -> Result<(), String>;

    /// Whether to start the project's MCP servers `approval` lists: the
    /// user's answer, which Arko remembers until the file's servers change;
    /// `None` when no one can answer (they don't start, and a note says
    /// so); `Err` with why the session doesn't open (the user cancelled).
    async fn approve_project_mcp(&self, approval: &McpApproval) -> Result<Option<bool>, String>;

    /// What Arko tells the user as the session opens, before its feed
    /// exists: a flag a resumed session ignores, a model it chose.
    fn note(&self, level: Level, text: &str);

    /// The session's folder is known, before its agents, MCP servers and
    /// workflow start: the front end can start logging there.
    fn session_found(&self, _dir: &Path) {}

    /// The session's workflow loads now (the questions are over): what the
    /// user does from now on, until the session takes the front end's
    /// input, is for the session (a terminal's Ctrl-C waits for it).
    fn opening(&self) {}
}

/// A front end with no one to ask: missing credentials keep the session
/// from opening (with what they are), a project's MCP servers don't start,
/// and the notes go to the log.
pub struct NoPrompter;

#[async_trait]
impl Prompter for NoPrompter {
    async fn need_credentials(&self, need: &Need) -> Result<(), String> {
        Err(need.message())
    }

    async fn approve_project_mcp(&self, _approval: &McpApproval) -> Result<Option<bool>, String> {
        Ok(None)
    }

    fn note(&self, level: Level, text: &str) {
        match level {
            Level::Info => log::info!("{text}"),
            Level::Warn => log::warn!("{text}"),
            Level::Error => log::error!("{text}"),
        }
    }
}

//! The terminal's side of a session's start: the setup screen for missing
//! credentials, the question about a project's MCP servers, and the notes on
//! standard error.

use std::path::Path;

use async_trait::async_trait;

use crate::credentials::{self, Need};
use crate::mcp::McpApproval;
use crate::prompter::Prompter;
use crate::remote::Level;
use crate::ui::Output;

/// Asks at the terminal, when standard input and output are one
/// (`can_ask`); else nothing is asked, as with [`NoPrompter`](crate::prompter::NoPrompter),
/// and the notes still go to standard error.
pub(crate) struct TerminalPrompter {
    can_ask: bool,
}

impl TerminalPrompter {
    pub(crate) fn new(can_ask: bool) -> Self {
        Self { can_ask }
    }
}

#[async_trait]
impl Prompter for TerminalPrompter {
    async fn need_credentials(&self, need: &Need) -> Result<(), String> {
        if !self.can_ask {
            return Err(need.message());
        }
        credentials::setup(Some(need))
    }

    async fn approve_project_mcp(&self, approval: &McpApproval) -> Result<Option<bool>, String> {
        if !self.can_ask {
            return Ok(None);
        }
        ask_to_start(approval).map(Some)
    }

    fn note(&self, _level: Level, text: &str) {
        eprintln!("arko: {text}");
    }

    fn session_found(&self, dir: &Path) {
        crate::cli::start_log(dir);
    }
}

/// Asks at the terminal whether to start the project's MCP servers.
fn ask_to_start(approval: &McpApproval) -> Result<bool, String> {
    let out = Output::new(None, false);
    out.line("");
    out.line(&format!(
        "{} sets up MCP servers. Arko would start these programs on your computer, and give the agents their tools:",
        approval.file.display()
    ));
    let width = approval
        .servers
        .iter()
        .map(|(name, _)| name.chars().count())
        .max()
        .unwrap_or(0);
    for (name, shown) in &approval.servers {
        out.line(&format!(
            "  {}  {}",
            out.bold(&format!("{name:width$}")),
            crate::ui::cut(shown, 160)
        ));
    }
    for why in &approval.skipped {
        out.line(&out.dim(&format!("  (skipped: {why})")));
    }
    out.line(&out.dim(
        "Arko keeps your answer until the file's servers change. `--mcp-config .mcp.json` starts them whatever the answer.",
    ));
    let mut editor = rustyline::DefaultEditor::new().map_err(|error| error.to_string())?;
    loop {
        match editor.readline("Start them? [y/n] ") {
            Ok(line) => match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => {}
            },
            Err(
                rustyline::error::ReadlineError::Interrupted | rustyline::error::ReadlineError::Eof,
            ) => {
                return Err("cancelled".into());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

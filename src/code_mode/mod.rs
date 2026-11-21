//! Code mode. The agent writes a short JavaScript program that calls its
//! tools (`run_code`), so loops, filtering and chaining happen in the
//! program and only its result goes back to the model.
//!
//! The program runs in a child process of its own, the code runner (this
//! same program, started with [`RUNNER_ARGUMENT`]), which holds no key or
//! tool. Its calls come to Arko as messages ([`protocol`]), and Arko runs
//! each through the gate that judges a call of the model's
//! ([`arko_agents::NestedCalls`]). The runner sets its limits with the
//! operating system before any code runs ([`os_limits`]), so a runaway or
//! crashing program, or a fault in the engine, ends the child and never
//! Arko.

#[cfg(feature = "code-mode")]
mod child;
#[cfg(feature = "code-mode")]
mod engine;
#[cfg(feature = "code-mode")]
mod host;
#[cfg(feature = "code-mode")]
mod os_limits;
#[cfg(feature = "code-mode")]
mod protocol;
#[cfg(feature = "code-mode")]
mod quickjs;
#[cfg(feature = "code-mode")]
mod runner;
mod tool;

#[cfg(all(test, feature = "code-mode"))]
mod engine_tests;

use serde::{Deserialize, Serialize};

#[cfg(feature = "code-mode")]
pub use runner::run_if_asked;
pub use tool::RunCodeTool;

/// The tool's name.
pub const RUN_CODE: &str = "run_code";

/// The argument that starts this program as the code runner.
pub const RUNNER_ARGUMENT: &str = "code-runner";

/// The tools code may call, and no others. The first four only read. Jobs
/// that outlive a run, workers, questions, messages, skills, notes and MCP
/// servers stay direct calls. (A run calls only those its agent has, so
/// sessions saved while the agents edited with `apply_diff` call that, and
/// new ones `edit_file`.) Under [`Tools::Code`] the agents reach these
/// only from code.
pub const CALLABLE: [&str; 8] = [
    arko_agents::builtins::READ_FILE_TOOL_NAME,
    arko_agents::builtins::LIST_FILES_TOOL_NAME,
    crate::index::SEARCH_TOOL_NAME,
    crate::index::STATUS_TOOL_NAME,
    crate::shell::SHELL_TOOL_NAME,
    arko_agents::builtins::WRITE_FILE_TOOL_NAME,
    arko_agents::builtins::EDIT_FILE_TOOL_NAME,
    arko_agents::builtins::APPLY_DIFF_TOOL_NAME,
];

/// The definitions of arko_agents' file tools of [`CALLABLE`], which the
/// runtime registers itself, for [`RunCodeTool::new`] beside the session's
/// own tools.
pub fn file_tool_definitions() -> Vec<arko_agents::ToolDefinition> {
    use arko_agents::Tool;
    use arko_agents::builtins::{
        ApplyDiffTool, EditFileTool, ListFilesTool, ReadFileTool, WriteFileTool,
    };
    vec![
        ReadFileTool.definition(),
        ListFilesTool.definition(),
        WriteFileTool.definition(),
        EditFileTool.definition(),
        ApplyDiffTool.definition(),
    ]
}

/// The definitions of the tools Arko gives `run_code` to call, for tests,
/// with a bash shell whose commands move to the background after 10
/// minutes, as in a session the user takes part in.
#[cfg(test)]
pub(crate) fn arko_definitions() -> Vec<arko_agents::ToolDefinition> {
    use crate::shell::{Background, DEFAULT_BACKGROUND_AFTER, Shell, ShellKind, ShellTool};
    use arko_agents::Tool;
    let jobs_dir = tempfile::tempdir().unwrap();
    let jobs = arko_agents::terminal::TerminalManager::new(
        arko_agents::terminal::TerminalConfig::new(jobs_dir.path().to_path_buf()),
    )
    .unwrap();
    let bash = Shell {
        program: "bash".into(),
        args: Vec::new(),
        job_args: Vec::new(),
        name: "bash",
        kind: ShellKind::Posix,
    };
    let shell = ShellTool::new(
        jobs.clone(),
        bash,
        Background::new(DEFAULT_BACKGROUND_AFTER, &jobs),
    );
    let mut definitions = file_tool_definitions();
    definitions.push(shell.definition());
    definitions.extend(crate::index::tool_definitions());
    definitions
}

// The limits of a run, set before any code runs.

/// Computing time, waits for tools not counted.
pub const COMPUTE_SECONDS: u64 = 30;
/// The engine's heap.
pub const HEAP_BYTES: u64 = 64 * 1024 * 1024;
/// The engine's stack, on a thread of [`THREAD_STACK_BYTES`].
pub const STACK_BYTES: u64 = 512 * 1024;
/// The stack of the runner's interpreter thread.
pub const THREAD_STACK_BYTES: usize = 2 * 1024 * 1024;
/// The memory the operating system lets the runner have, the heap and room
/// for the runner itself and the results coming in.
pub const OS_MEMORY_BYTES: u64 = 512 * 1024 * 1024;
/// The address space the runner may add to what it has as it starts: the
/// memory above, and the reservations of its threads' allocators.
pub const OS_ADDRESS_SPACE_BYTES: u64 = 1024 * 1024 * 1024;
/// Processor time the operating system lets the runner have, above the
/// computing time, for reading results and writing frames.
pub const CPU_SECONDS: u64 = COMPUTE_SECONDS + 30;
/// Tool calls in a run.
pub const MAX_CALLS: u32 = 500;
/// Refused calls in a run, after which it ends.
pub const MAX_REFUSALS: u32 = 3;
/// The code's size.
pub const MAX_CODE_BYTES: usize = 100 * 1024;
/// What the model gets of a run's result, in characters.
pub const MAX_RESULT_CHARS: usize = 30_000;
/// One call's result as the code gets it.
pub const MAX_ANSWER_BYTES: usize = 8 * 1024 * 1024;
/// One call's arguments as the code sends them.
pub const MAX_CALL_BYTES: usize = 8 * 1024 * 1024;
/// A message the runner keeps, such as a thrown error's, in characters.
pub const MAX_MESSAGE_CHARS: usize = 4_000;
/// The failed calls the code never read that a result lists.
pub const MAX_UNREAD: usize = 20;
/// What the runner keeps of the code's `console` lines.
pub const MAX_LOG_BYTES: usize = 64 * 1024;

/// Which tools a session's agents are offered (`--tools`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "tui", derive(clap::ValueEnum))]
#[serde(rename_all = "lowercase")]
pub enum Tools {
    /// Their tools, called one at a time. Sessions saved before code mode
    /// have these.
    #[default]
    Classic,
    /// Their tools and `run_code`, which calls them from JavaScript.
    Both,
    /// `run_code`, and their tools less those of [`CALLABLE`], which they
    /// reach only from code.
    Code,
}

impl Tools {
    pub const ALL: [Tools; 3] = [Tools::Classic, Tools::Both, Tools::Code];

    /// What a new session has unless it's told otherwise.
    pub const NEW_SESSION: Tools = Tools::Classic;

    /// The name the flag and `ARKO_TOOLS` take.
    pub fn name(self) -> &'static str {
        match self {
            Tools::Classic => "classic",
            Tools::Both => "both",
            Tools::Code => "code",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|tools| tools.name().eq_ignore_ascii_case(text.trim()))
    }

    pub fn is_classic(&self) -> bool {
        *self == Tools::Classic
    }

    /// The agents are offered `run_code`.
    pub fn offers_code(self) -> bool {
        matches!(self, Tools::Both | Tools::Code)
    }

    /// The agents reach the tools of [`CALLABLE`] only from code.
    pub fn calls_only_from_code(self) -> bool {
        self == Tools::Code
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Code calls only tools that read, write files or run commands.
    #[test]
    fn code_calls_only_the_eight_tools() {
        assert_eq!(
            CALLABLE,
            [
                "read_file",
                "list_files",
                "index_search",
                "index_status",
                "shell",
                "write_file",
                "edit_file",
                "apply_diff"
            ]
        );
    }

    #[test]
    fn the_setting_reads_its_names_in_any_case() {
        assert_eq!(Tools::parse(" Both "), Some(Tools::Both));
        assert_eq!(Tools::parse("classic"), Some(Tools::Classic));
        assert_eq!(Tools::parse("CODE"), Some(Tools::Code));
        assert_eq!(Tools::parse("direct"), None);
        assert_eq!(Tools::default(), Tools::Classic);
        assert!(Tools::Code.offers_code() && Tools::Code.calls_only_from_code());
        assert!(Tools::Both.offers_code() && !Tools::Both.calls_only_from_code());
        assert!(!Tools::Classic.offers_code());
    }
}

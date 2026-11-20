//! Which tools' calls can move to the background while they run, by name,
//! from each tool's own `background_mode`. That is what the runtime asks
//! when it offers a running call to move (arko_agents' `run_tool` holds a
//! call whose tool's mode isn't `Never`, and [`jobs::movable`](crate::jobs::movable)
//! lists those). A call's line keeps the room of Move to background from the
//! start where its tool's mode says it can move ([`remote::Act::moves`](crate::remote::Act::moves)).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use arko_agents::{LoadSkillTool, SkillsConfig, Tool, ToolBackground, ToolRegistry};
use arko_workflows::subagents::{SUBAGENT_TOOL_NAMES, WAIT_FOR_AGENTS_TOOL_NAME};

/// The background mode of each tool an agent may call, by name.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ToolModes(BTreeMap<String, ToolBackground>);

impl ToolModes {
    /// The tools the framework gives a run. Its built-in tools come from
    /// [`ToolRegistry::new`] and `load_skill` from [`LoadSkillTool`], each
    /// asked its mode. The subagent tools and `run_workflow_agent` are
    /// arko_workflows' own, which it keeps to itself, and the runtime
    /// answers `handover_report` with no tool, so those stand here as their
    /// tools declare their mode. Of them only `wait_for_agents` can move
    /// (`Own`). The session test `every_offered_tool_moves_as_the_runtime_offers_it`
    /// calls each subagent tool and checks it against what the runtime
    /// offers to move.
    pub(crate) fn framework() -> Self {
        let mut modes = Self::default();
        let builtins = ToolRegistry::new();
        for name in builtins.list_tool_names() {
            if let Some(tool) = builtins.get(&name) {
                modes.note(&tool);
            }
        }
        let skills: Arc<dyn Tool> = Arc::new(LoadSkillTool::new(SkillsConfig::default(), "."));
        modes.note(&skills);
        for name in SUBAGENT_TOOL_NAMES {
            let mode = match *name {
                WAIT_FOR_AGENTS_TOOL_NAME => ToolBackground::Own,
                _ => ToolBackground::Never,
            };
            modes.0.insert(name.to_string(), mode);
        }
        for name in [
            arko_workflows::services::RUN_WORKFLOW_AGENT_TOOL_NAME,
            arko_agents::handover::HANDOVER_REPORT_TOOL_NAME,
        ] {
            modes.0.insert(name.to_string(), ToolBackground::Never);
        }
        modes
    }

    /// Notes `tool`'s mode, over what was noted of a tool of its name.
    pub(crate) fn note(&mut self, tool: &Arc<dyn Tool>) {
        self.0.insert(tool.name(), tool.background_mode());
    }

    /// The mode of the tool `name`, if it's one of these.
    #[cfg(all(test, unix))]
    pub(crate) fn mode(&self, name: &str) -> Option<ToolBackground> {
        self.0.get(name).copied()
    }

    /// The names of these tools.
    #[cfg(test)]
    pub(crate) fn names(&self) -> BTreeSet<String> {
        self.0.keys().cloned().collect()
    }

    /// The names of the tools whose calls can move.
    pub(crate) fn moving(&self) -> BTreeSet<String> {
        self.0
            .iter()
            .filter(|(_, mode)| **mode != ToolBackground::Never)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// The modes of the tools a session's main agent and its workers may call
/// ([`ToolModes`]), as the session registers them.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SessionToolModes {
    pub(crate) main: ToolModes,
    pub(crate) worker: ToolModes,
}

impl SessionToolModes {
    /// Each kind of agent's tools start with the framework's.
    pub(crate) fn framework() -> Self {
        Self {
            main: ToolModes::framework(),
            worker: ToolModes::framework(),
        }
    }

    /// The modes of a session's tools for a test that drives a feed as a
    /// session would (the page's shift meter, the state gallery). Those are
    /// the shell, terminal and monitor tools of a session in `dir`, the
    /// search index's and the framework's, each asked its mode.
    #[cfg(test)]
    pub(crate) fn for_a_session(dir: &std::path::Path) -> Self {
        use arko_agents::terminal::{MonitorTool, TerminalConfig, TerminalManager, TerminalTool};
        std::fs::create_dir_all(dir).unwrap();
        let shell = crate::shell::Shell::detect(crate::shell::WindowsShell::Cmd);
        let mut config = TerminalConfig::new(dir.join("jobs"));
        config.shell = shell.program.clone();
        config.shell_args = shell.job_args.clone();
        let jobs = TerminalManager::new(config).unwrap();
        let background = crate::shell::Background::new(0, &jobs);
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(crate::shell::ShellTool::new(
            jobs.clone(),
            shell,
            background,
        ))];
        tools.extend(
            TerminalTool::all(jobs.clone())
                .into_iter()
                .map(|tool| Arc::new(tool) as Arc<dyn Tool>),
        );
        tools.extend(
            MonitorTool::all(jobs)
                .into_iter()
                .map(|tool| Arc::new(tool) as Arc<dyn Tool>),
        );
        let indexes = crate::index::Indexes::open(&dir.join("indexes"), dir, None, false).unwrap();
        tools.extend(indexes.tools());
        let mut modes = Self::framework();
        for tool in &tools {
            modes.main.note(tool);
            modes.worker.note(tool);
        }
        modes
    }
}

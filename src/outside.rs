//! Outside access is whether the agents may delete, move and overwrite paths
//! outside the working directory. The user turns it on and off while the
//! session runs (`/outside on|off`, from the terminal or the web page, and
//! `--allow-outside-workspace` when resuming). The command guard, the file
//! tools' writes and the workers' workspaces follow it at once, and the
//! agents get a note (see [`note`]), since what they were told at the start
//! no longer holds.
//!
//! The file tools follow it through a `PreToolExecution` hook, not their
//! configuration, because an agent's configuration is saved with the session and
//! must stay the same for a resume. Sessions saved before the switch keep
//! the refusal in their configuration, so there the switch reaches the
//! shell only ([`Outside::files_follow`]).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use arko_agents::agent_config::PathSafetyPolicy;
use arko_agents::builtins::{
    APPLY_DIFF_TOOL_NAME, ApplyDiffTool, EDIT_FILE_TOOL_NAME, EditFileTool, WRITE_FILE_TOOL_NAME,
    WriteFileTool,
};
use arko_agents::terminal::TerminalManager;
use arko_agents::{AgentConfig, Tool};
use arko_workflows::hooks::context::HookContext;
use arko_workflows::prelude::*;
use serde_json::Value;
use uuid::Uuid;

use crate::crew::START_AGENT;

/// The `start_agent` parameter naming the directory a worker works in.
const WORKSPACE_PARAM: &str = "workspace";

/// The switch, and what follows it.
pub struct Outside {
    on: AtomicBool,
    /// The terminal managers whose command guard follows the switch. (The
    /// session owns them, so once it closes, their folders' locks go, also
    /// while a front end still holds its status.)
    managers: Vec<Weak<TerminalManager>>,
    /// The session's working directory (the main agent's).
    workspace: PathBuf,
    /// The workflow, for the workers' working directories.
    workflow: Mutex<Weak<Workflow>>,
    /// Whether the file tools' writes follow the switch, which is false when the
    /// agents' saved configuration refuses writes outside the working
    /// directory.
    files_follow: AtomicBool,
}

impl Outside {
    pub fn new(on: bool, managers: Vec<Arc<TerminalManager>>, workspace: PathBuf) -> Self {
        for manager in &managers {
            manager.set_allow_outside_workspace(on);
        }
        Self {
            on: AtomicBool::new(on),
            managers: managers.iter().map(Arc::downgrade).collect(),
            workspace,
            workflow: Mutex::new(Weak::new()),
            files_follow: AtomicBool::new(true),
        }
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    /// Turns outside access on or off, from the agents' next calls on.
    /// Returns whether that changed it.
    pub fn set(&self, on: bool) -> bool {
        let was = self.on.swap(on, Ordering::SeqCst);
        for manager in self.managers.iter().filter_map(Weak::upgrade) {
            manager.set_allow_outside_workspace(on);
        }
        was != on
    }

    /// Whether the file tools' writes follow the switch (they do in
    /// sessions started since it exists).
    pub fn files_follow(&self) -> bool {
        self.files_follow.load(Ordering::SeqCst)
    }

    /// The workflow the agents run in, and the configurations it runs them
    /// with (the saved ones in a resumed session).
    pub fn attach<'a>(
        &self,
        workflow: &Arc<Workflow>,
        configs: impl IntoIterator<Item = &'a AgentConfig>,
    ) {
        *crate::lock(&self.workflow) = Arc::downgrade(workflow);
        self.files_follow
            .store(configs.into_iter().all(files_follow), Ordering::SeqCst);
    }

    /// The `PreToolExecution` hook that refuses, while outside access is
    /// off, the file tools' writes outside the calling agent's working
    /// directory and workers started in a directory outside it.
    pub fn hook(self: &Arc<Self>) -> HookCallback {
        let outside = Arc::downgrade(self);
        Arc::new(move |hook| match outside.upgrade() {
            Some(outside) => outside.check(hook),
            None => HookResult::Continue,
        })
    }

    fn check(&self, hook: &HookContext) -> HookResult {
        if self.is_on() {
            return HookResult::Continue;
        }
        let (Some(tool), Some(params)) = (hook.tool_name.as_deref(), hook.tool_params.as_ref())
        else {
            return HookResult::Continue;
        };
        let directory = self.directory_of(hook.agent_run_id);
        if tool == START_AGENT {
            // As the tool reads it, which takes a number as its digits.
            let path = match params.get(WORKSPACE_PARAM) {
                Some(Value::String(path)) => path.clone(),
                Some(Value::Number(number)) => number.to_string(),
                _ => return HookResult::Continue,
            };
            let dir = directory.display();
            return match leaves(&path, &directory) {
                None => HookResult::Continue,
                Some(Leaves::Outside) => HookResult::Reject(format!(
                    "Refused: the workspace `{path}` is outside your working directory `{dir}`, and the user hasn't turned on outside access. Give the worker a workspace inside your working directory, or say why the task needs that one."
                )),
                Some(Leaves::Maybe(reason)) => HookResult::Reject(format!(
                    "Refused: the workspace `{path}` may lead outside your working directory `{dir}` ({reason}), and the user hasn't turned on outside access. Give the worker a workspace inside your working directory, or say why the task needs that one."
                )),
            };
        }
        // (`apply_diff` for sessions saved while the agents edited with it.)
        let definition = match tool {
            WRITE_FILE_TOOL_NAME => WriteFileTool.definition(),
            EDIT_FILE_TOOL_NAME => EditFileTool.definition(),
            APPLY_DIFF_TOOL_NAME => ApplyDiffTool.definition(),
            _ => return HookResult::Continue,
        };
        // (The tool itself refuses a path that isn't a string.)
        for parameter in definition.parameters.iter().filter(|p| p.is_path) {
            let Some(path) = params.get(&parameter.name).and_then(Value::as_str) else {
                continue;
            };
            let dir = directory.display();
            match leaves(path, &directory) {
                None => {}
                Some(Leaves::Outside) => {
                    return HookResult::Reject(format!(
                        "Refused: `{path}` is outside the working directory `{dir}`, and the user hasn't turned on outside access. Keep the files you write inside the working directory, or say what you need to change there and why."
                    ));
                }
                // (Refused with outside access on too, by the tool.)
                Some(Leaves::Maybe(reason)) => {
                    return HookResult::Reject(format!(
                        "Refused: `{path}` may lead outside the working directory `{dir}` ({reason}). Name the file by its real path, without links."
                    ));
                }
            }
        }
        HookResult::Continue
    }

    /// The working directory of run `run`, meaning a worker's own, else the
    /// session's.
    fn directory_of(&self, run: Option<Uuid>) -> PathBuf {
        let workflow = crate::lock(&self.workflow).upgrade();
        run.zip(workflow)
            .and_then(|(run, workflow)| {
                workflow
                    .subagents()
                    .into_iter()
                    .find(|record| record.run_id == run)
                    .map(|record| record.workspace)
            })
            .unwrap_or_else(|| self.workspace.clone())
    }

    /// What the agents are told of outside access as it is now, posted as
    /// a note when it changes, and when a session starts with it on.
    pub fn note(&self) -> String {
        if !self.is_on() {
            return "The user turned off outside access: commands and the file tools can no longer delete, move or overwrite paths outside the working directory (commands still may in temporary directories), and new workers work inside it.".to_string();
        }
        let protected = "Protected paths (the filesystem root, the home directory itself, system directories) stay refused. Before you change things outside the working directory that the user didn't ask for, ask them.";
        if self.files_follow() {
            format!(
                "The user turned on outside access: commands and the file tools may now delete, move and overwrite paths outside the working directory, and workers may work in other directories. {protected}"
            )
        } else {
            format!(
                "The user turned on outside access for commands: they may now delete, move and overwrite paths outside the working directory, and workers may work in other directories. In this session the file tools still can't write outside the working directory, so make those changes with the shell. {protected}"
            )
        }
    }

    /// What the user is told of outside access as it is now.
    pub fn describe(&self) -> String {
        if !self.is_on() {
            return "Outside access is off, so the agents can't delete, move or overwrite paths outside the working directory. /outside on allows it.".to_string();
        }
        let mut text = String::from(
            "Outside access is on, so the agents may delete, move and overwrite paths outside the working directory (protected paths, such as the home directory itself, stay refused). /outside off turns it off.",
        );
        if !self.files_follow() {
            text.push_str(FILES_FIXED);
        }
        text
    }

    /// The managers' guards, as last set (for tests).
    #[cfg(test)]
    fn guards(&self) -> Vec<bool> {
        self.managers
            .iter()
            .filter_map(Weak::upgrade)
            .map(|manager| manager.allows_outside_workspace())
            .collect()
    }
}

/// Why a path isn't inside a working directory.
enum Leaves {
    Outside,
    /// The check couldn't place it inside, for this reason. It may leave
    /// through a link, go through a link whose target is missing, or be
    /// inside or outside depending on how a tool reads it.
    Maybe(String),
}

/// Whether `path`, relative to `directory`, may lead outside it. A blank
/// path is the directory itself, and a path is inside only when the check
/// finds it inside.
fn leaves(path: &str, directory: &std::path::Path) -> Option<Leaves> {
    if path.trim().is_empty() {
        return None;
    }
    match arko_agents::utils::is_path_in_workspace(path, directory) {
        Ok(true) => None,
        Ok(false) => Some(Leaves::Outside),
        Err(arko_agents::ArkoAgentsError::Io(error)) => Some(Leaves::Maybe(error.to_string())),
        Err(error) => Some(Leaves::Maybe(error.to_string())),
    }
}

/// What the user is told in a session whose file tools don't follow the
/// switch.
pub const FILES_FIXED: &str = " In this session it reaches commands only. Its agents were saved before outside access could change, so their file tools still write only inside the working directory. A new session has both.";

/// Whether an agent configuration lets the file tools' writes follow the
/// switch, meaning it doesn't refuse writes outside the working directory itself.
/// `write_file` is judged in any configuration, and the editing tool it
/// offers (`edit_file`, or `apply_diff` in sessions saved while the agents
/// edited with it).
pub fn files_follow(config: &AgentConfig) -> bool {
    // (Reached only from code, a tool is the agent's as well.)
    let offered = |tool: &str| {
        let tools = &config.tool_config;
        tools.available_tool_names.iter().any(|name| name == tool) || tools.is_nested_only(tool)
    };
    [
        WRITE_FILE_TOOL_NAME,
        EDIT_FILE_TOOL_NAME,
        APPLY_DIFF_TOOL_NAME,
    ]
    .into_iter()
    .filter(|tool| *tool == WRITE_FILE_TOOL_NAME || offered(tool))
    .all(|tool| config.get_path_safety_policy(tool) != PathSafetyPolicy::AutoReject)
}

/// Reads `/outside`'s argument, which is `Some(on)`, or `None` for none.
pub fn parse(argument: &str) -> Result<Option<bool>, String> {
    match argument.trim().to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "on" => Ok(Some(true)),
        "off" => Ok(Some(false)),
        other => Err(format!("`{other}`: use /outside on or /outside off.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn hook(tool: &str, params: Value) -> HookContext {
        HookContext {
            timestamp: chrono::Utc::now(),
            hook_point: HookPoint::PreToolExecution,
            workflow_run_id: Uuid::new_v4(),
            workflow_name: "arko".into(),
            agent_run_id: Some(Uuid::new_v4()),
            agent_name: Some("main".into()),
            tool_name: Some(tool.into()),
            tool_call_id: Some(Uuid::new_v4()),
            tool_params: Some(serde_json::from_value::<HashMap<String, Value>>(params).unwrap()),
            tool_result: None,
            original_tool_result: None,
            effective_tool_result: None,
            agent_summary: None,
            approval_reason: None,
            error_message: None,
            budget_spent: 0.0,
            budget_limit: None,
            metadata: HashMap::new(),
        }
    }

    fn refused(result: HookResult) -> Option<String> {
        match result {
            HookResult::Reject(reason) => Some(reason),
            HookResult::Continue => None,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// While it's off, the file tools write only inside the working
    /// directory and workers start only inside it, and on, both may go
    /// anywhere. Other tools aren't the hook's business.
    #[test]
    fn the_hook_follows_the_switch() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("work");
        let outside = Arc::new(Outside::new(false, Vec::new(), workspace.clone()));
        let check = |tool: &str, params: Value| refused(outside.check(&hook(tool, params)));
        let elsewhere = temp.path().join("elsewhere.txt").display().to_string();
        let reason = check(
            WRITE_FILE_TOOL_NAME,
            serde_json::json!({"path": elsewhere, "content": "x"}),
        )
        .unwrap();
        assert!(reason.contains("outside the working directory"), "{reason}");
        assert!(
            check(
                EDIT_FILE_TOOL_NAME,
                serde_json::json!({"path": "../x.txt", "old_string": "a", "new_string": "b"})
            )
            .unwrap()
            .contains("outside the working directory")
        );
        // A path that starts with ~ is the home directory's, not the working
        // directory's.
        assert!(
            check(
                WRITE_FILE_TOOL_NAME,
                serde_json::json!({"path": "~/x.txt", "content": "x"})
            )
            .is_some_and(|reason| reason.contains("outside the working directory"))
        );
        assert!(
            check(
                EDIT_FILE_TOOL_NAME,
                serde_json::json!({"path": "src/x.txt", "old_string": "a", "new_string": "b"})
            )
            .is_none()
        );
        // Sessions saved while the agents edited with apply_diff have it.
        assert!(
            check(
                APPLY_DIFF_TOOL_NAME,
                serde_json::json!({"path": "../x.txt", "diff": ""})
            )
            .is_some()
        );
        assert!(
            check(
                WRITE_FILE_TOOL_NAME,
                serde_json::json!({"path": "src/x.txt", "content": "x"})
            )
            .is_none()
        );
        assert!(
            check(
                START_AGENT,
                serde_json::json!({"agent": "worker", "task": "t", "workspace": "../other"})
            )
            .is_some()
        );
        assert!(
            check(
                START_AGENT,
                serde_json::json!({"agent": "worker", "task": "t", "workspace": "sub"})
            )
            .is_none()
        );
        assert!(
            check(
                START_AGENT,
                serde_json::json!({"agent": "worker", "task": "t"})
            )
            .is_none()
        );
        // Reading anywhere is allowed, and the shell has its own guard.
        assert!(check("read_file", serde_json::json!({"path": elsewhere})).is_none());
        assert!(check("shell", serde_json::json!({"command": "rm -rf ../x"})).is_none());

        assert!(outside.set(true));
        assert!(!outside.set(true), "no change");
        assert!(
            check(
                WRITE_FILE_TOOL_NAME,
                serde_json::json!({"path": elsewhere, "content": "x"})
            )
            .is_none()
        );
        assert!(
            check(
                EDIT_FILE_TOOL_NAME,
                serde_json::json!({"path": elsewhere, "old_string": "a", "new_string": "b"})
            )
            .is_none()
        );
        assert!(
            check(
                START_AGENT,
                serde_json::json!({"agent": "worker", "task": "t", "workspace": "../other"})
            )
            .is_none()
        );
        assert!(
            outside
                .note()
                .starts_with("The user turned on outside access: commands and the file tools")
        );
        assert!(outside.set(false));
        assert!(
            check(
                WRITE_FILE_TOOL_NAME,
                serde_json::json!({"path": elsewhere, "content": "x"})
            )
            .is_some()
        );
        assert!(
            outside
                .note()
                .starts_with("The user turned off outside access")
        );
    }

    /// A working directory `work` holding `src`, `source -> src`, and the
    /// links `tools` and `7` to the directory `elsewhere` beside it, and
    /// `up -> ..`, with outside access off.
    #[cfg(unix)]
    fn linked() -> (tempfile::TempDir, Arc<Outside>) {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("work");
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::create_dir(&elsewhere).unwrap();
        symlink("src", workspace.join("source")).unwrap();
        symlink(&elsewhere, workspace.join("tools")).unwrap();
        symlink(&elsewhere, workspace.join("7")).unwrap();
        symlink("..", workspace.join("up")).unwrap();
        let outside = Arc::new(Outside::new(false, Vec::new(), workspace));
        (temp, outside)
    }

    #[cfg(unix)]
    fn start_in(outside: &Outside, workspace: Value) -> Option<String> {
        refused(outside.check(&hook(
            START_AGENT,
            serde_json::json!({"agent": "worker", "task": "t", "workspace": workspace}),
        )))
    }

    /// While outside access is off, a worker can't start in a link to a
    /// directory outside the working directory, however the link is
    /// written. On, it may. A link that stays inside is fine.
    #[cfg(unix)]
    #[test]
    fn a_workspace_through_a_link_to_elsewhere_is_refused() {
        let (_temp, outside) = linked();
        for workspace in ["tools", "tools/", "./tools", " tools "] {
            let reason = start_in(&outside, workspace.into())
                .unwrap_or_else(|| panic!("`{workspace}` was let through"));
            assert!(reason.contains("symbolic link"), "{reason}");
            assert!(reason.contains("outside access"), "{reason}");
        }
        // The tool reads a number as its digits.
        assert!(start_in(&outside, 7.into()).is_some());
        assert!(start_in(&outside, "source".into()).is_none());
        assert!(start_in(&outside, "src".into()).is_none());
        outside.set(true);
        assert!(start_in(&outside, "tools".into()).is_none());
    }

    /// A link to the directory above is refused as a workspace, also on the
    /// way to a directory beside the working directory.
    #[cfg(unix)]
    #[test]
    fn a_workspace_through_a_link_up_is_refused() {
        let (_temp, outside) = linked();
        for workspace in ["up", "up/", "up/elsewhere"] {
            assert!(
                start_in(&outside, workspace.into()).is_some(),
                "`{workspace}` was let through"
            );
        }
        // Back into the working directory is inside it.
        assert!(start_in(&outside, "up/work/src".into()).is_none());
    }

    /// The file tools' writes through a link that leaves are refused too.
    #[cfg(unix)]
    #[test]
    fn writes_through_a_link_that_leaves_are_refused() {
        let (_temp, outside) = linked();
        let write = |path: &str| {
            refused(outside.check(&hook(
                WRITE_FILE_TOOL_NAME,
                serde_json::json!({"path": path, "content": "x"}),
            )))
        };
        for path in ["tools/x.txt", "up/x.txt", "up/elsewhere/x.txt"] {
            let reason = write(path).unwrap_or_else(|| panic!("`{path}` was let through"));
            assert!(reason.contains("symbolic link"), "{reason}");
        }
        assert!(write("source/x.txt").is_none());
    }

    /// The command guards of the managers follow the switch, from the
    /// start.
    #[tokio::test]
    async fn the_command_guards_follow_the_switch() {
        let temp = tempfile::tempdir().unwrap();
        let open = |name: &str| {
            TerminalManager::new(arko_agents::terminal::TerminalConfig::new(
                temp.path().join(name),
            ))
            .unwrap()
        };
        // (The session holds them.)
        let managers = vec![open("a"), open("b")];
        let outside = Outside::new(true, managers.clone(), temp.path().into());
        assert_eq!(outside.guards(), [true, true]);
        outside.set(false);
        assert_eq!(outside.guards(), [false, false]);
    }

    /// Configurations saved before the switch refuse the file tools'
    /// writes outside the working directory themselves, and the user is told
    /// that the switch reaches commands only there.
    #[test]
    fn saved_configurations_that_refuse_outside_writes_are_told_apart() {
        let model = crate::agents::ModelSettings {
            model: crate::agents::resolve_model("opus").unwrap().id,
            effort: crate::agents::Effort::High,
            fast: false,
            cache_1h: false,
            handover_at: None,
        };
        let settings = crate::agents::AgentSettings {
            max_workers: 0,
            ..crate::agents::AgentSettings::for_test(&model, &model)
        };
        let mut config = crate::agents::main_config(&settings).unwrap();
        assert!(files_follow(&config));
        let refusing = |config: &AgentConfig, tool: &str| {
            let mut config = config.clone();
            config
                .tool_config
                .tool_specific_configs
                .get_mut(tool)
                .unwrap()
                .out_of_workspace_path_policy = Some(PathSafetyPolicy::AutoReject);
            config
        };
        assert!(!files_follow(&refusing(&config, EDIT_FILE_TOOL_NAME)));
        // A session saved while the agents edited with apply_diff offers it,
        // with the same overrides. One saved before the switch refuses both
        // writes.
        let tools = &mut config.tool_config;
        for name in &mut tools.available_tool_names {
            if name == EDIT_FILE_TOOL_NAME {
                *name = APPLY_DIFF_TOOL_NAME.into();
            }
        }
        let edit = tools
            .tool_specific_configs
            .remove(EDIT_FILE_TOOL_NAME)
            .unwrap();
        tools
            .tool_specific_configs
            .insert(APPLY_DIFF_TOOL_NAME.into(), edit);
        assert!(files_follow(&config));
        assert!(!files_follow(&refusing(&config, APPLY_DIFF_TOOL_NAME)));
        let config = refusing(
            &refusing(&config, APPLY_DIFF_TOOL_NAME),
            WRITE_FILE_TOOL_NAME,
        );
        assert!(!files_follow(&config));
        let outside = Outside::new(true, Vec::new(), "/work".into());
        let workflow = Arc::new(
            Workflow::builder()
                .name("arko")
                .workspace(std::env::temp_dir())
                .llm_client(Arc::new(arko_ai::LLMClient::new()))
                .agent("main", config.clone())
                .build()
                .unwrap(),
        );
        outside.attach(&workflow, [&config]);
        assert!(!outside.files_follow());
        assert!(outside.describe().ends_with(FILES_FIXED));
        assert!(
            outside
                .note()
                .contains("the file tools still can't write outside")
        );
        assert_eq!(parse(" ON ").unwrap(), Some(true));
        assert_eq!(parse("").unwrap(), None);
        assert!(parse("maybe").is_err());
    }
}

//! Outside access: whether the agents may delete, move and overwrite paths
//! outside the working directory. The user turns it on and off while the
//! session runs (`/outside on|off`, from the terminal or the web page, and
//! `--allow-outside-workspace` when resuming). The command guard, the file
//! tools' writes and the workers' workspaces follow it at once, and the
//! agents get a note (see [`note`]), since what they were told at the start
//! no longer holds.
//!
//! The file tools follow it through a `PreToolExecution` hook, not their
//! configuration: an agent's configuration is saved with the session and
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
    /// session owns them: once it closes, their folders' locks go, also
    /// while a front end still holds its status.)
    managers: Vec<Weak<TerminalManager>>,
    /// The session's working directory (the main agent's).
    workspace: PathBuf,
    /// The workflow, for the workers' working directories.
    workflow: Mutex<Weak<Workflow>>,
    /// Whether the file tools' writes follow the switch: false when the
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
        let outside = |path: &str| {
            !path.trim().is_empty()
                && matches!(
                    arko_agents::utils::is_path_in_workspace(path, &directory),
                    Ok(false)
                )
        };
        if tool == START_AGENT {
            return match params.get(WORKSPACE_PARAM).and_then(Value::as_str) {
                Some(path) if outside(path) => HookResult::Reject(format!(
                    "Refused: the workspace `{path}` is outside your working directory `{}`, and the user hasn't turned on outside access. Give the worker a workspace inside your working directory, or say why the task needs that one.",
                    directory.display()
                )),
                _ => HookResult::Continue,
            };
        }
        // (`apply_diff` for sessions saved before `edit_file`.)
        let definition = match tool {
            WRITE_FILE_TOOL_NAME => WriteFileTool.definition(),
            EDIT_FILE_TOOL_NAME => EditFileTool.definition(),
            APPLY_DIFF_TOOL_NAME => ApplyDiffTool.definition(),
            _ => return HookResult::Continue,
        };
        for parameter in definition.parameters.iter().filter(|p| p.is_path) {
            if let Some(path) = params.get(&parameter.name).and_then(Value::as_str)
                && outside(path)
            {
                return HookResult::Reject(format!(
                    "Refused: `{path}` is outside the working directory `{}`, and the user hasn't turned on outside access. Keep the files you write inside the working directory, or say what you need to change there and why.",
                    directory.display()
                ));
            }
        }
        HookResult::Continue
    }

    /// The working directory of run `run`: a worker's own, else the
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

    /// What the agents are told of outside access as it is now: posted as
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
            return "Outside access is off: the agents can't delete, move or overwrite paths outside the working directory. /outside on allows it.".to_string();
        }
        let mut text = String::from(
            "Outside access is on: the agents may delete, move and overwrite paths outside the working directory (protected paths, such as the home directory itself, stay refused). /outside off turns it off.",
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

/// What the user is told in a session whose file tools don't follow the
/// switch.
pub const FILES_FIXED: &str = " In this session it reaches commands only: its agents were saved before outside access could change, so their file tools still write only inside the working directory. A new session has both.";

/// Whether an agent configuration lets the file tools' writes follow the
/// switch: it doesn't refuse writes outside the working directory itself.
/// `write_file` is judged in any configuration, and the editing tool it
/// offers (`edit_file`, or `apply_diff` in sessions saved before it).
pub fn files_follow(config: &AgentConfig) -> bool {
    let offered = |tool: &str| {
        config
            .tool_config
            .available_tool_names
            .iter()
            .any(|name| name == tool)
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

/// Reads `/outside`'s argument: `Some(on)`, or `None` for none.
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
    /// directory and workers start only inside it; on, both may go
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
        assert!(
            check(
                EDIT_FILE_TOOL_NAME,
                serde_json::json!({"path": "src/x.txt", "old_string": "a", "new_string": "b"})
            )
            .is_none()
        );
        // Sessions saved before edit_file have apply_diff.
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
    /// writes outside the working directory themselves; the user is told
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
        // A session saved before edit_file offers apply_diff, with the
        // same overrides. One saved before the switch refuses both writes.
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

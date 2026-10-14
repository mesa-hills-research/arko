//! Native subagent configuration and the session's worker status display.
//! arko owns the runs, mail, checkpoints, costs and isolated workspaces.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use arko_agents::{
    AgentConfig, TaskContext, Tool, ToolDefinition, ToolError, ToolParameterDefinition,
    ToolParameterType, ToolTypeDetails,
};
use arko_ai::AnthropicCacheTtl;
use arko_ai::prompt_cache::PromptCacheLifetime;
use arko_workflows::prelude::*;
use arko_workflows::subagents::{
    GitWorktreeProvider, StartRequest, SubagentEventKind, SubagentState, SubagentsConfig,
};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agents::{MAIN, NO_LIMIT};
use crate::remote;
use crate::ui::{self, Output};

pub use arko_workflows::subagents::{
    LIST_AGENTS_TOOL_NAME as LIST_AGENTS, MESSAGE_AGENT_TOOL_NAME as MESSAGE_AGENT,
    START_AGENT_TOOL_NAME as START_AGENT, STOP_AGENT_TOOL_NAME as STOP_AGENT,
    WAIT_FOR_AGENTS_TOOL_NAME as WAIT_FOR_AGENTS,
};
pub const LEAD_TOOL_NAMES: &[&str] = &[
    START_AGENT,
    MESSAGE_AGENT,
    WAIT_FOR_AGENTS,
    LIST_AGENTS,
    STOP_AGENT,
    "link_agents",
];
pub const WORKER_TOOL_NAMES: &[&str] = &[MESSAGE_AGENT, WAIT_FOR_AGENTS];
pub const CACHE_PARAM: &str = "cache";
pub const LEGACY_TOOL_NAMES: &[&str] = &[
    "start_worker",
    "message_worker",
    "wait_for_workers",
    "list_workers",
    "stop_worker",
    "message_lead",
    "wait_for_lead",
];
pub const LEGACY_NOTICE: &str = "This session uses the old worker tools. Its old workers and worktrees remain archived, but cannot be continued here. The old worker tools are unavailable. Continue the task yourself, or ask the user to start a new fin session for native workers. /new resets this conversation but keeps its old tools.";

pub struct Setup {
    pub session_id: String,
    pub session_dir: PathBuf,
    pub output: Arc<Output>,
    pub max_running: u32,
    pub cache_1h: bool,
    /// The client's cache lifetime for the worker model and authentication.
    pub cache_lifetime: PromptCacheLifetime,
    pub allow_outside_workspace: bool,
    pub feed: Option<remote::Feed>,
}

/// A view of one workflow's subagents. It has no worker runtime or mailbox.
pub struct Crew {
    setup: Setup,
    workflow: Mutex<Weak<Workflow>>,
    lead: Mutex<Option<Uuid>>,
    wake: mpsc::UnboundedSender<()>,
    events: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Crew {
    pub fn new(setup: Setup) -> (Arc<Self>, mpsc::UnboundedReceiver<()>) {
        let (wake, woken) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                setup,
                workflow: Mutex::new(Weak::new()),
                lead: Mutex::new(None),
                wake,
                events: Mutex::new(None),
            }),
            woken,
        )
    }

    pub fn config(&self) -> SubagentsConfig {
        let default_1h = self.setup.cache_1h;
        let mut config = SubagentsConfig::default()
            .offer_blocking_tool(false)
            .questions_to_lead(true)
            .workspace_provider(Arc::new(GitWorktreeProvider::new(self.setup.session_dir.join("worktrees"))
                .branch_prefix(format!("fin/{}/", self.setup.session_id))))
            .hook(Arc::new(move |request: &StartRequest, config: &mut AgentConfig| {
                let chosen = request.parameters.get(CACHE_PARAM).and_then(Value::as_str);
                let one_hour = match chosen {
                    Some("1h") => true,
                    Some("5m") => false,
                    Some(other) => return Err(format!("cache is 5m or 1h, not {other}")),
                    None => default_1h,
                };
                if crate::providers::takes_cache_ttl(&config.llm_config.model) {
                    config.llm_config.generation_config.anthropic_cache_ttl = one_hour.then_some(AnthropicCacheTtl::OneHour);
                }
                Ok(())
            }))
            .start_parameter(ToolParameterDefinition {
                name: CACHE_PARAM.into(),
                description: "Claude workers on an API key or OpenRouter: 1h for later follow-up, 5m for a short task. Others ignore it: ChatGPT keeps its own 30-minute cache, Claude Code an hour.".into(),
                param_type: ToolParameterType::String, required: false, is_path: false, flag: None, default_value: None,
            });
        config = if self.setup.max_running == NO_LIMIT {
            config.unlimited()
        } else {
            config.max_running(self.setup.max_running as usize)
        };
        if self.setup.allow_outside_workspace {
            config = config.workspaces_anywhere();
        }
        config
    }

    pub fn attach(self: &Arc<Self>, workflow: &Arc<Workflow>, lead: Option<Uuid>) {
        *self.workflow.lock().unwrap() = Arc::downgrade(workflow);
        // A crash before the fresh lead's first tool can leave its workers
        // owned by the previous lead. Keep that owner until lead_run transfers it.
        *self.lead.lock().unwrap() = workflow
            .subagents()
            .last()
            .map(|run| run.caller_run_id)
            .or(lead);
        if let Some(old) = self.events.lock().unwrap().take() {
            old.abort();
        }
        let mut events = workflow.subscribe_subagents();
        if let Some(feed) = &self.setup.feed {
            // The workers of a resumed session, for the clients' lists.
            for run in workflow.subagents() {
                let state = match run.state {
                    SubagentState::Queued => remote::WorkerState::Queued,
                    SubagentState::Running => remote::WorkerState::Running,
                    SubagentState::Finished => remote::WorkerState::Finished,
                    SubagentState::Failed => remote::WorkerState::Failed,
                    SubagentState::Stopped | SubagentState::Interrupted => {
                        remote::WorkerState::Stopped
                    }
                    SubagentState::Closed if run.report.is_some() => remote::WorkerState::Finished,
                    SubagentState::Closed => remote::WorkerState::Stopped,
                };
                feed.worker_restored(
                    &run.handle,
                    &run.task,
                    state,
                    run.report.as_deref(),
                    run.since.timestamp_millis(),
                );
            }
        }
        if self.has_mail() {
            let _ = self.wake.send(());
        }
        let weak = Arc::downgrade(self);
        *self.events.lock().unwrap() = Some(tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(crew) = weak.upgrade() {
                            let _ = crew.wake.send(());
                        }
                        continue;
                    }
                    Err(_) => break,
                };
                let Some(crew) = weak.upgrade() else { break };
                let text = match event.kind.clone() {
                    SubagentEventKind::Queued => Some(format!("{} queued", event.handle)),
                    SubagentEventKind::Started => Some(format!("{} started", event.handle)),
                    SubagentEventKind::Resumed => Some(format!("{} resumed", event.handle)),
                    SubagentEventKind::Finished { summary } => Some(format!(
                        "{} reported: {}",
                        event.handle,
                        ui::cut(&summary, 180)
                    )),
                    SubagentEventKind::Failed { error } => {
                        Some(format!("{} failed: {error}", event.handle))
                    }
                    SubagentEventKind::Stopped { reason, .. } => {
                        Some(format!("{} stopped: {reason}", event.handle))
                    }
                    SubagentEventKind::WorkspaceRemoved => {
                        Some(format!("{} worktree removed", event.handle))
                    }
                    _ => None,
                };
                if let Some(text) = text {
                    crew.setup
                        .output
                        .line(&crew.setup.output.dim(&format!("  ↳ {text}")));
                }
                if let Some(feed) = &crew.setup.feed {
                    crew.tell_feed(feed, &event.handle, event.run_id, &event.kind);
                }
                let _ = crew.wake.send(());
            }
        }));
    }

    /// What became of a worker, for the feed: its summary and placeholder
    /// follow its runs.
    fn tell_feed(&self, feed: &remote::Feed, handle: &str, run: Uuid, kind: &SubagentEventKind) {
        use remote::WorkerChange;
        let change = match kind {
            SubagentEventKind::Queued => WorkerChange::Queued,
            SubagentEventKind::Started | SubagentEventKind::Resumed => WorkerChange::Started,
            SubagentEventKind::Finished { summary } => WorkerChange::Finished { report: summary },
            SubagentEventKind::Failed { error } => WorkerChange::Failed { error },
            SubagentEventKind::Stopped { reason, .. } => WorkerChange::Stopped { reason },
            SubagentEventKind::WorkspaceRemoved => {
                feed.notice(remote::Level::Info, &format!("{handle} worktree removed"));
                return;
            }
            _ => return,
        };
        let task = self
            .workflow()
            .and_then(|workflow| {
                workflow
                    .subagents()
                    .into_iter()
                    .find(|record| record.run_id == run)
            })
            .map(|record| record.task)
            .unwrap_or_default();
        feed.worker(handle, &task, change);
    }

    fn workflow(&self) -> Option<Arc<Workflow>> {
        self.workflow.lock().unwrap().upgrade()
    }

    /// Called before the lead's first tool execution, before it can act on
    /// workers transferred by /new. Continuations keep the same run id.
    pub fn lead_run(&self, run: Uuid) {
        let previous = self.lead.lock().unwrap().replace(run);
        if let Some(previous) = previous.filter(|old| *old != run)
            && let Some(workflow) = self.workflow()
        {
            workflow.transfer_subagents(previous, run);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.workflow()
            .is_none_or(|workflow| workflow.subagents().is_empty())
    }
    pub fn running(&self) -> Vec<String> {
        self.workflow()
            .map(|workflow| {
                workflow
                    .active_subagents()
                    .into_iter()
                    .map(|run| run.handle)
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn has_mail(&self) -> bool {
        let lead = *self.lead.lock().unwrap();
        lead.zip(self.workflow()).is_some_and(|(lead, workflow)| {
            workflow
                .pending_news(lead)
                .iter()
                .any(|item| item.kind.wakes())
        })
    }
    pub fn describe(&self) -> String {
        self.workflow()
            .map(|workflow| {
                workflow
                    .subagents()
                    .iter()
                    .map(|run| {
                        let status = format!("{:?}", run.state).to_lowercase();
                        let mut line =
                            format!("{}  {status}  {}", run.handle, ui::cut(&run.task, 100));
                        let mut lifetime = self.setup.cache_lifetime;
                        if workflow
                            .context()
                            .agent_configs
                            .get(&run.agent)
                            .is_some_and(|config| {
                                crate::providers::takes_cache_ttl(&config.llm_config.model)
                            })
                        {
                            lifetime = match run.parameters.get(CACHE_PARAM).and_then(Value::as_str)
                            {
                                Some("1h") => PromptCacheLifetime::Known(Duration::from_secs(3600)),
                                Some("5m") => PromptCacheLifetime::Known(Duration::from_secs(300)),
                                _ => lifetime,
                            };
                        }
                        if let PromptCacheLifetime::Known(lifetime) = lifetime {
                            line.push_str(&format!(
                                "\n  prompt cache: {} minutes",
                                lifetime.as_secs() / 60
                            ));
                        }
                        if let Some(worktree) = &run.isolated {
                            line.push_str(&format!(
                                "\n  {} ({})",
                                worktree.path.display(),
                                worktree.branch.as_deref().unwrap_or("worktree")
                            ));
                        }
                        if let Some(report) = run.report.as_ref().or(run.error.as_ref()) {
                            line.push_str(&format!("\n  {}", ui::cut(report, 240)));
                        }
                        line
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
    pub fn stop_all(&self, reason: &str) -> usize {
        self.workflow().map_or(0, |workflow| {
            workflow
                .active_subagents()
                .iter()
                .filter(|run| workflow.stop_subagent(run.run_id, StopRequest::operator(reason)))
                .count()
        })
    }
    pub fn spent(&self) -> (f64, usize, usize) {
        self.workflow().map_or((0.0, 0, 0), |workflow| {
            let (input, output) = workflow
                .result()
                .agent_results
                .iter()
                .filter(|run| run.agent_name != MAIN)
                .fold((0, 0), |(input, output), run| {
                    (
                        input + run.total_input_tokens(),
                        output + run.total_output_tokens(),
                    )
                });
            (
                workflow.context().budget.agent_spent(crate::agents::WORKER),
                input,
                output,
            )
        })
    }
    pub async fn shutdown(&self) {
        if let Some(workflow) = self.workflow() {
            // Stopping the workflow marks its unfinished subagents for resume.
            workflow.stop(StopRequest::operator("fin closed"));
            let _ = tokio::time::timeout(Duration::from_secs(12), async {
                while !workflow.active_subagents().is_empty() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            // Mail publication schedules its own checkpoint. Await one here so
            // exiting immediately after a report preserves that report on resume.
            if let Err(error) = workflow.checkpoint().await {
                let text = format!("Could not save the final worker state: {error}");
                self.setup.output.line(&self.setup.output.warn(&text));
                if let Some(feed) = &self.setup.feed {
                    feed.notice(remote::Level::Warn, &text);
                }
            }
        }
        if let Some(events) = self.events.lock().unwrap().take() {
            events.abort();
        }
    }
}

impl Drop for Crew {
    fn drop(&mut self) {
        if let Some(events) = self.events.get_mut().unwrap().take() {
            events.abort();
        }
    }
}

/// Saved legacy conversations still offer these names. Successful tool
/// results explain the migration instead of triggering error recovery.
#[derive(Debug)]
pub struct LegacyTool(pub &'static str);
#[async_trait]
impl Tool for LegacyTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.0.into(),
            description: LEGACY_NOTICE.into(),
            output_based_example: None,
            parameters: vec![],
            parallel_safe: true,
            tool_type_details: ToolTypeDetails::DeveloperDefined,
            raw_json_schema_override: Some(
                serde_json::json!({"type":"object", "properties":{}, "additionalProperties":true}),
            ),
        }
    }
    async fn execute(
        &self,
        _: &TaskContext<'_>,
        _: HashMap<String, Value>,
    ) -> Result<String, ToolError> {
        Ok(LEGACY_NOTICE.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arko_agents::AgentLLMConfig;

    #[test]
    fn native_worker_options_preserve_cache_choices_and_queue_limits() {
        let temp = tempfile::tempdir().unwrap();
        let (crew, _) = Crew::new(Setup {
            session_id: "example".into(),
            session_dir: temp.path().into(),
            output: Arc::new(Output::new(None, true)),
            max_running: 2,
            cache_1h: true,
            cache_lifetime: PromptCacheLifetime::Known(Duration::from_secs(3600)),
            allow_outside_workspace: false,
            feed: None,
        });
        let native = crew.config();
        assert_eq!(native.max_running.unwrap().get(), 2);
        assert!(native.questions_to_lead);
        assert!(!native.offer_blocking_tool);
        let hook = native.hook.unwrap();
        let mut config = AgentConfig::builder(
            "worker".into(),
            "worker".into(),
            AgentLLMConfig {
                model: crate::agents::resolve_model("opus").unwrap(),
                generation_config: arko_ai::GenerationConfig::default(),
            },
        )
        .role_description("A test worker".into())
        .instructions("Carry out the test task.".into())
        .build()
        .unwrap();
        let mut request = StartRequest {
            workflow_run_id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            handle: "r1".into(),
            agent: "worker".into(),
            caller_run_id: Uuid::new_v4(),
            caller_agent: MAIN.into(),
            task: "Check it".into(),
            parameters: Default::default(),
        };
        hook.prepare(&request, &mut config).unwrap();
        assert_eq!(
            config.llm_config.generation_config.anthropic_cache_ttl,
            Some(AnthropicCacheTtl::OneHour)
        );
        request.parameters.insert(CACHE_PARAM.into(), "5m".into());
        hook.prepare(&request, &mut config).unwrap();
        assert_eq!(
            config.llm_config.generation_config.anthropic_cache_ttl,
            None
        );
        request.parameters.insert(CACHE_PARAM.into(), "1h".into());
        hook.prepare(&request, &mut config).unwrap();
        assert_eq!(
            config.llm_config.generation_config.anthropic_cache_ttl,
            Some(AnthropicCacheTtl::OneHour)
        );
        request.parameters.insert(CACHE_PARAM.into(), "2h".into());
        assert!(hook.prepare(&request, &mut config).is_err());
    }
}

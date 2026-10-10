//! The agents: the main agent the user talks with, and the workers it can
//! hand tasks to. Their configurations depend only on the session's
//! settings, so a resumed session builds them again unchanged.

use arko_agents::terminal::TerminalTool;
use arko_agents::{
    AgentConfig, AgentLLMConfig, AgentToolConfig, BuiltInEnvironmentDetail,
    EnvironmentDetailPlacement, EnvironmentDetailsConfig, HandoverConfig, PerToolConfigOverride,
};
use arko_ai::{
    AnthropicCacheTtl, AnthropicReasoningEffort, GenerationConfig, Model, OpenAIEffort,
    OpenAIServiceTier, Provider, ReasoningConfig,
};
use arko_workflows::services::RUN_WORKFLOW_AGENT_TOOL_NAME;
use serde::{Deserialize, Serialize};

use crate::shell::SHELL_TOOL_NAME;

/// The agent the user talks with.
pub const MAIN: &str = "main";
/// The agent `main` hands tasks to.
pub const WORKER: &str = "worker";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl From<Effort> for AnthropicReasoningEffort {
    fn from(effort: Effort) -> Self {
        match effort {
            Effort::Low => Self::Low,
            Effort::Medium => Self::Medium,
            Effort::High => Self::High,
            Effort::Xhigh => Self::XHigh,
            Effort::Max => Self::Max,
        }
    }
}

impl From<Effort> for OpenAIEffort {
    fn from(effort: Effort) -> Self {
        match effort {
            Effort::Low => Self::Low,
            Effort::Medium => Self::Medium,
            Effort::High => Self::High,
            Effort::Xhigh => Self::XHigh,
            Effort::Max => Self::Max,
        }
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        })
    }
}

/// Short names for the models people use most.
const ALIASES: [(&str, &str); 6] = [
    ("opus", "claude-opus-5-5"),
    ("fable", "claude-fable-5-1"),
    ("sonnet", "claude-sonnet-5-5"),
    ("sol", "gpt-6.1-sol"),
    ("astra", "gpt-6-astra"),
    ("luna", "gpt-6-luna"),
];

/// The registered Anthropic or OpenAI model `name` names: an ID or an
/// alias.
pub fn resolve_model(name: &str) -> Result<Model, String> {
    let id = ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map_or(name, |(_, id)| id);
    let model = Model::find(id).ok_or_else(|| {
        format!(
            "`{name}` isn't a model fin knows. Use a model ID, or one of: {}",
            ALIASES
                .iter()
                .map(|(alias, id)| format!("{alias} ({id})"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    match model.provider {
        Provider::Anthropic | Provider::OpenAI => Ok(model),
        provider => Err(format!(
            "`{name}` is a {provider:?} model; fin runs Anthropic models (API key) and OpenAI models (ChatGPT login)"
        )),
    }
}

/// One agent's model settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSettings {
    /// A registered model ID.
    pub model: String,
    pub effort: Effort,
    /// Anthropic fast mode, or OpenAI's Fast mode over ChatGPT OAuth.
    pub fast: bool,
    /// Anthropic's 1-hour prompt cache instead of the 5-minute one (Claude
    /// models only). Off in sessions saved before it existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_1h: bool,
}

impl ModelSettings {
    pub fn describe(&self) -> String {
        format!(
            "{} ({} effort{}{})",
            self.model,
            self.effort,
            if self.fast { ", fast" } else { "" },
            if self.cache_1h { ", 1h cache" } else { "" }
        )
    }

    pub fn provider(&self) -> Result<Provider, String> {
        Ok(resolve_model(&self.model)?.provider)
    }

    fn llm_config(&self, show_thinking: bool) -> Result<AgentLLMConfig, String> {
        let model = resolve_model(&self.model)?;
        let mut generation = GenerationConfig::default().with_prompt_caching(true);
        match model.provider {
            Provider::Anthropic => {
                if self.fast && !model.supports_anthropic_fast_mode() {
                    return Err(format!(
                        "{} has no fast mode; Anthropic offers it on Claude Opus 5.5, Opus 5 and Opus 4.8",
                        model.id
                    ));
                }
                generation = generation.with_reasoning(
                    ReasoningConfig::anthropic_adaptive_with_effort(self.effort.into()),
                );
                if self.fast {
                    generation = generation.with_anthropic_fast_mode(true);
                }
                if self.cache_1h {
                    generation = generation.with_anthropic_cache_ttl(AnthropicCacheTtl::OneHour);
                }
            }
            _ => {
                generation = generation
                    .with_reasoning(ReasoningConfig::openai(self.effort.into(), show_thinking));
                if self.fast {
                    generation = generation.with_openai_service_tier(OpenAIServiceTier::Fast);
                }
            }
        }
        Ok(AgentLLMConfig {
            model,
            generation_config: generation,
        })
    }
}

/// What the agent configurations are built from.
pub struct AgentSettings<'a> {
    pub main: &'a ModelSettings,
    pub worker: &'a ModelSettings,
    /// How many workers may run at once; 0 turns them off.
    pub max_workers: u32,
    pub handover_at: usize,
    pub show_thinking: bool,
    /// What the agents are told the shell is.
    pub shell_name: &'a str,
    pub allow_outside_workspace: bool,
}

/// The tools both agents have: the shell and the terminal tools.
pub fn terminal_tool_names() -> Vec<String> {
    std::iter::once(SHELL_TOOL_NAME)
        .chain(TerminalTool::NAMES)
        .map(String::from)
        .collect()
}

fn environment() -> EnvironmentDetailsConfig {
    let detail = BuiltInEnvironmentDetail {
        include: true,
        placement: EnvironmentDetailPlacement::SystemPrompt,
        depth: None,
    };
    EnvironmentDetailsConfig {
        current_working_directory: detail.clone(),
        current_datetime: detail.clone(),
        operating_system: detail,
        ..EnvironmentDetailsConfig::default()
    }
}

pub fn main_config(settings: &AgentSettings) -> Result<AgentConfig, String> {
    let mut tools = terminal_tool_names();
    let mut overrides = std::collections::HashMap::new();
    if settings.max_workers > 0 {
        tools.push(RUN_WORKFLOW_AGENT_TOOL_NAME.into());
        // Several workers in one response run at the same time.
        overrides.insert(
            RUN_WORKFLOW_AGENT_TOOL_NAME.to_string(),
            PerToolConfigOverride {
                parallel_safe: Some(true),
                ..PerToolConfigOverride::default()
            },
        );
    }
    let mut instructions = main_instructions(settings);
    instructions.push_str(&tool_instructions(settings));
    if settings.max_workers > 0 {
        instructions.push_str(&worker_instructions(settings.max_workers));
    }
    AgentConfig::builder(
        MAIN.into(),
        "fin".into(),
        settings.main.llm_config(settings.show_thinking)?,
    )
    .role_description(
        "You are fin, a general-purpose agent working in a terminal on the user's computer. You help with software engineering and anything else the user asks: you investigate, run commands, create and change files, and report back.".into(),
    )
    .instructions(instructions)
    .tool_config(AgentToolConfig {
        available_tool_names: tools,
        default_auto_approve_tool_use: true,
        tool_specific_configs: overrides,
        ..AgentToolConfig::default()
    })
    .environment_details_config(environment())
    .complete_task_summary_prompt_override(
        "Your reply to the user's latest message, in full: what you did, what you found, and anything you need from them. They read it as your message (Markdown, shown as plain text)."
            .into(),
    )
    .handover(HandoverConfig::at_input_tokens(settings.handover_at))
    .build()
    .map_err(|error| error.to_string())
}

pub fn worker_config(settings: &AgentSettings) -> Result<AgentConfig, String> {
    let mut instructions = String::from(
        "# Your task\n\
         A lead agent gave you one task, in your first message. It can't see your work and can't answer questions while you work, so make reasonable choices yourself and say which in your report; if something blocks you, report what and why.\n\n\
         When the task is done, or you can't go further, call `complete_task`. Its summary is your report to the lead, and the only thing it sees of your work: what you did and found, with the evidence (file paths and line numbers, the commands you ran and what they showed), every file you changed, and anything unfinished. Mark what you couldn't confirm, and say where you looked.\n",
    );
    instructions.push_str(&tool_instructions(settings));
    AgentConfig::builder(
        WORKER.into(),
        "worker".into(),
        settings.worker.llm_config(settings.show_thinking)?,
    )
    .role_description(
        "A subagent that carries out one task the lead agent hands it (an investigation, a change, a review) in the same working directory, with a shell and terminal, and reports back.".into(),
    )
    .instructions(instructions)
    .tool_config(AgentToolConfig {
        available_tool_names: terminal_tool_names(),
        default_auto_approve_tool_use: true,
        ..AgentToolConfig::default()
    })
    .environment_details_config(environment())
    .complete_task_summary_prompt_override(
        "Your report to the lead agent: what you did and found, with evidence (paths, line numbers, commands and results), the files you changed, and anything unfinished or unconfirmed."
            .into(),
    )
    .handover(HandoverConfig::at_input_tokens(settings.handover_at))
    .build()
    .map_err(|error| error.to_string())
}

fn main_instructions(_settings: &AgentSettings) -> String {
    "# How this conversation works\n\
     The user talks with you in a terminal. Each of their messages starts a turn: work on it with your tools for as long as it takes, then end the turn by calling `complete_task`. Its summary is your reply: the user reads it as your message, so write your answer there in full. Text you write outside tool calls is shown to the user as you work; keep it to short progress notes, and don't repeat your reply in it.\n\n\
     When a step doesn't need the user, keep going. End the turn to ask only when you can't continue without them, or before something destructive or hard to undo that they didn't ask for: deleting data they may want, force-pushing, publishing, or changing things outside the working directory.\n\n\
     The user can type while you work. What they type reaches you as a note from the operator: take it into account in the work you're doing.\n"
        .to_string()
}

fn tool_instructions(settings: &AgentSettings) -> String {
    let outside = if settings.allow_outside_workspace {
        "the filesystem root, the home directory, system directories or devices"
    } else {
        "the filesystem root, the home directory, system directories, devices, or paths outside the working directory"
    };
    format!(
        "\n# Tools\n\
         `shell` runs a command in {shell} in the working directory and returns its exit code and output once it finishes. Use it for everything you would do in a terminal: searching and reading files (prefer `rg`, and read large files in parts), creating and editing files, building, testing, git. Each call starts in the working directory, so a `cd` doesn't carry over to the next call; to run a command somewhere else, give the call that directory as `cwd` instead of a `cd`. Edit files with a short script or a heredoc, and check the result.\n\n\
         The terminal tools run programs in the background: servers, watchers, long builds you want to keep an eye on, and programs that ask questions. `terminal_start` returns a job ID at once, `terminal_status` reads a job's output, `terminal_input` answers a prompt, and `terminal_control` waits for a job, lists jobs or cancels one. Use them only when `shell` doesn't fit, and cancel the jobs you started once you no longer need them.\n\n\
         A command guard refuses commands that would delete, move or overwrite {outside}. A refusal is the guard working, not an obstacle: don't rephrase, split or encode the command to get past it. Do the work another way, or say what was refused and why you needed it.\n",
        shell = settings.shell_name,
    )
}

fn worker_instructions(max_workers: u32) -> String {
    format!(
        "\n# Workers\n\
         `run_workflow_agent` with agent `worker` hands a task to a subagent that has the same shell and terminal tools and works in the same directory, and returns its report when it finishes. A worker doesn't see this conversation: give it everything it needs in `prompt`, namely the goal, the context and constraints, what done looks like, and what to report. Several calls in one response run at the same time, up to {max_workers} at once.\n\n\
         Use workers for independent parts of a large task (exploring separate areas of a codebase, an audit split by area, changes to separate files), or to keep a long investigation out of your own context. Do small things yourself. Don't let workers running at the same time edit the same files, and check what a worker reports before you rely on it.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings<'a>(main: &'a ModelSettings, worker: &'a ModelSettings) -> AgentSettings<'a> {
        AgentSettings {
            main,
            worker,
            max_workers: 4,
            handover_at: 200_000,
            show_thinking: false,
            shell_name: "bash",
            allow_outside_workspace: false,
        }
    }

    fn model(name: &str, fast: bool) -> ModelSettings {
        ModelSettings {
            model: resolve_model(name).unwrap().id,
            effort: Effort::Xhigh,
            fast,
            cache_1h: false,
        }
    }

    #[test]
    fn the_one_hour_cache_is_asked_for_only_when_enabled() {
        let opus = model("opus", false);
        let config = main_config(&settings(&opus, &opus)).unwrap();
        assert_eq!(
            config.llm_config.generation_config.anthropic_cache_ttl,
            None
        );
        let opus = ModelSettings {
            cache_1h: true,
            ..opus
        };
        let config = main_config(&settings(&opus, &opus)).unwrap();
        assert_eq!(
            config.llm_config.generation_config.anthropic_cache_ttl,
            Some(AnthropicCacheTtl::OneHour)
        );
        let config = worker_config(&settings(&opus, &opus)).unwrap();
        assert_eq!(
            config.llm_config.generation_config.anthropic_cache_ttl,
            Some(AnthropicCacheTtl::OneHour)
        );
        assert_eq!(opus.describe(), "claude-opus-5-5 (xhigh effort, 1h cache)");
    }

    #[test]
    fn model_settings_saved_without_the_cache_setting_still_load() {
        let old = r#"{"model":"claude-opus-5-5","effort":"xhigh","fast":false}"#;
        let settings: ModelSettings = serde_json::from_str(old).unwrap();
        assert!(!settings.cache_1h);
        assert_eq!(serde_json::to_string(&settings).unwrap(), old);
    }

    #[test]
    fn aliases_and_ids_name_models_of_the_two_providers() {
        assert_eq!(resolve_model("opus").unwrap().id, "claude-opus-5-5");
        assert_eq!(resolve_model("Sol").unwrap().id, "gpt-6.1-sol");
        assert_eq!(
            resolve_model("claude-fable-5-1").unwrap().id,
            "claude-fable-5-1"
        );
        assert!(resolve_model("gemini-3.5-flash").is_err());
        assert!(resolve_model("no-such-model").is_err());
    }

    #[test]
    fn fast_mode_on_a_model_without_it_is_refused() {
        let opus = model("opus", true);
        let fable = model("fable", true);
        assert!(main_config(&settings(&opus, &opus)).is_ok());
        let error = main_config(&settings(&fable, &fable)).unwrap_err();
        assert!(error.contains("has no fast mode"), "{error}");
        let sol = model("sol", true);
        let config = main_config(&settings(&sol, &sol)).unwrap();
        assert_eq!(
            config.llm_config.generation_config.openai_service_tier,
            Some(OpenAIServiceTier::Fast)
        );
    }

    #[test]
    fn workers_are_offered_only_when_enabled_and_run_in_parallel() {
        let opus = model("opus", false);
        let mut with = settings(&opus, &opus);
        let config = main_config(&with).unwrap();
        assert!(
            config
                .tool_config
                .available_tool_names
                .iter()
                .any(|name| name == RUN_WORKFLOW_AGENT_TOOL_NAME)
        );
        assert_eq!(
            config.tool_config.tool_specific_configs[RUN_WORKFLOW_AGENT_TOOL_NAME].parallel_safe,
            Some(true)
        );
        with.max_workers = 0;
        let config = main_config(&with).unwrap();
        assert!(
            !config
                .tool_config
                .available_tool_names
                .iter()
                .any(|name| name == RUN_WORKFLOW_AGENT_TOOL_NAME)
        );
        assert!(!config.instructions.contains("# Workers"));
    }

    /// A resumed session builds the same configuration, which the
    /// workflow requires of a task it resumes.
    #[test]
    fn the_same_settings_build_the_same_configuration() {
        let opus = model("opus", false);
        let first = serde_json::to_value(main_config(&settings(&opus, &opus)).unwrap()).unwrap();
        let second = serde_json::to_value(main_config(&settings(&opus, &opus)).unwrap()).unwrap();
        assert_eq!(first, second);
    }
}

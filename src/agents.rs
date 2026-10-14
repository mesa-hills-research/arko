//! The agents: the main agent the user talks with, and the workers it can
//! hand tasks to. Their configurations depend only on the session's
//! settings, so a resumed session builds them again unchanged.

use arko_agents::agent_config::PathSafetyPolicy;
use arko_agents::builtins::{
    APPLY_DIFF_TOOL_NAME, ASK_USER_TOOL_NAME, LIST_FILES_TOOL_NAME, READ_FILE_TOOL_NAME,
    WRITE_FILE_TOOL_NAME,
};
use arko_agents::terminal::TerminalTool;
use arko_agents::{
    AgentConfig, AgentLLMConfig, AgentToolConfig, BuiltInEnvironmentDetail,
    EnvironmentDetailPlacement, EnvironmentDetailsConfig, HandoverConfig, PerToolConfigOverride,
};
use arko_ai::input_limit::effective_max_input_tokens;
use arko_ai::{
    AnthropicCacheTtl, AnthropicReasoningEffort, GenerationConfig, Model, OpenAIEffort,
    OpenAIServiceTier, Provider, ReasoningConfig,
};
use serde::{Deserialize, Serialize};

use crate::crew;
use crate::index;
use crate::shell::SHELL_TOOL_NAME;
use crate::skills;

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
    /// The input size, in tokens, at which the agent hands over. `None` in
    /// sessions saved before it was set per model: the session's
    /// `handover_at` applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handover_at: Option<usize>,
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

    fn is_claude(&self) -> bool {
        self.provider()
            .is_ok_and(|provider| provider == Provider::Anthropic)
    }

    /// An OpenAI model, which fin reaches over the ChatGPT login only.
    fn is_chatgpt(&self) -> bool {
        self.provider()
            .is_ok_and(|provider| provider == Provider::OpenAI)
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
    /// How many workers may run at once; 0 turns them off, and
    /// [`NO_LIMIT`] sets no limit.
    pub max_workers: u32,
    pub handover_at: usize,
    pub show_thinking: bool,
    /// What the agents are told the shell is.
    pub shell_name: &'a str,
    /// The tools of the MCP servers running, offered to both agents.
    pub mcp_tools: &'a [String],
    /// The MCP servers running, by name.
    pub mcp_servers: &'a [String],
    /// The skill folders, read at the start of each run.
    pub skill_folders: &'a [std::path::PathBuf],
    /// The search index tools, offered to both agents.
    pub index_tools: &'a [&'a str],
    /// The indexes when the session started, named in the instructions
    /// when there are index tools.
    pub indexes: &'a [index::Known],
}

/// How many indexes the instructions name; `index_status` lists the rest.
const INDEXES_NAMED: usize = 20;

/// `max_workers` for no limit on how many workers run at once.
pub const NO_LIMIT: u32 = u32::MAX;

/// What the handover size leaves free below the input limit of the
/// ChatGPT login's backend: the report request and the report the agent
/// writes (a few thousand tokens), and the tool results of a step past
/// the size. The size is checked before each request with its new tool
/// results counted, so a step goes past it only by what the estimate
/// misses, or when fin asks after a request reported its size.
pub const CHATGPT_HANDOVER_ROOM: usize = 22_000;

/// The input size, in tokens, at which an agent on `model` hands over
/// unless `--handover-at` says otherwise: 500k on Claude models; over the
/// ChatGPT login (`chatgpt_login`), what its backend accepts
/// ([`CHATGPT_OAUTH_MAX_INPUT_TOKENS`](arko_ai::input_limit::CHATGPT_OAUTH_MAX_INPUT_TOKENS), 272k, which is less than the
/// API's limit) less [`CHATGPT_HANDOVER_ROOM`], so 250k; 200k on others.
/// Except over the ChatGPT login, at most 80% of what the model accepts.
pub fn default_handover(model: &Model, chatgpt_login: bool) -> usize {
    if chatgpt_login
        && model.provider == Provider::OpenAI
        && let Some(limit) = effective_max_input_tokens(model, true)
    {
        return limit.saturating_sub(CHATGPT_HANDOVER_ROOM);
    }
    let tokens = match model.provider {
        Provider::Anthropic => 500_000,
        _ => 200_000,
    };
    match model.max_input_tokens {
        0 => tokens,
        limit => tokens.min(limit / 5 * 4),
    }
}

/// arko's built-in file tools, which both agents have.
pub const FILE_TOOL_NAMES: [&str; 4] = [
    READ_FILE_TOOL_NAME,
    LIST_FILES_TOOL_NAME,
    WRITE_FILE_TOOL_NAME,
    APPLY_DIFF_TOOL_NAME,
];

/// The tools both agents have: the shell and the terminal tools.
pub fn terminal_tool_names() -> Vec<String> {
    std::iter::once(SHELL_TOOL_NAME)
        .chain(TerminalTool::NAMES)
        .map(String::from)
        .collect()
}

/// The tools both agents have: the shell, the terminal tools, the file
/// tools, the search index tools and the MCP servers' tools. (`load_skill`
/// is added for each run when there are skills to load.)
fn shared_tool_names(settings: &AgentSettings) -> Vec<String> {
    let mut tools = terminal_tool_names();
    tools.extend(FILE_TOOL_NAMES.iter().map(|name| name.to_string()));
    tools.extend(settings.index_tools.iter().map(|name| name.to_string()));
    tools.extend(settings.mcp_tools.iter().cloned());
    tools
}

/// Both agents' tool configuration: `tools` run without approval, and
/// `read_file` reads line ranges. The file tools may read and write
/// anywhere as far as the configuration goes: whether they write outside
/// the working directory follows outside access, which can change while
/// the session runs, so a hook decides it (see `outside`), as the command
/// guard does for commands.
fn tool_config(tools: Vec<String>) -> AgentToolConfig {
    let anywhere = PerToolConfigOverride {
        out_of_workspace_path_policy: Some(PathSafetyPolicy::AutoApprove),
        ..PerToolConfigOverride::default()
    };
    let overrides = [
        READ_FILE_TOOL_NAME,
        LIST_FILES_TOOL_NAME,
        WRITE_FILE_TOOL_NAME,
        APPLY_DIFF_TOOL_NAME,
    ]
    .into_iter()
    .map(|name| (name.to_string(), anywhere.clone()))
    .collect();
    AgentToolConfig {
        available_tool_names: tools,
        default_auto_approve_tool_use: true,
        tool_specific_configs: overrides,
        read_file_line_ranges: true,
        ..AgentToolConfig::default()
    }
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
    let mut tools = shared_tool_names(settings);
    tools.push(ASK_USER_TOOL_NAME.into());
    tools.push(crate::message::MESSAGE_USER.into());
    if settings.max_workers > 0 {
        tools.extend(crew::LEAD_TOOL_NAMES.iter().map(|name| name.to_string()));
    }
    let mut instructions = main_instructions(settings);
    instructions.push_str(&tool_instructions(settings));
    if settings.max_workers > 0 {
        instructions.push_str(&worker_instructions(settings));
    }
    let mut builder = AgentConfig::builder(
        MAIN.into(),
        "fin".into(),
        settings.main.llm_config(settings.show_thinking)?,
    )
    .role_description(
        "You are fin, a general-purpose agent working in a terminal on the user's computer. You help with software engineering and anything else the user asks: you investigate, run commands, create and change files, and report back.".into(),
    )
    .instructions(instructions)
    .tool_config(tool_config(tools))
    .environment_details_config(environment())
    .complete_task_summary_prompt_override(
        "Your reply to the user's latest message, in full: what you did, what you found, and anything you need from them. They read it as your message (Markdown, shown as plain text)."
            .into(),
    )
    .handover(HandoverConfig::at_input_tokens(
        settings.main.handover_at.unwrap_or(settings.handover_at),
    ));
    if let Some(skills) = skills::config(settings.skill_folders) {
        builder = builder.skills_config(skills);
    }
    builder.build().map_err(|error| error.to_string())
}

pub fn worker_config(settings: &AgentSettings) -> Result<AgentConfig, String> {
    let mut instructions = String::from(
        "# Your task\n\
         A lead agent gave you one task, in your first message, and works on other things while you work on it. It can't see your work. Make reasonable choices yourself and say which in your report; if something blocks you, report what and why.\n\n\
         When the task is done, or you can't go further, call `complete_task`. Its summary is your report to the lead, and the only thing it sees of your work: what you did and found, with the evidence (file paths and line numbers, the commands you ran and what they showed), every file you changed (and the commits you made), and anything unfinished. Mark what you couldn't confirm, and say where you looked.\n\n\
         # The lead\n\
         The lead can send you messages while you work: they reach you as workflow context updates from your lead. Take them into account; a message that arrives as you finish comes back to you, and you report again. `message_agent` addressed to `lead` sends the lead a message without ending your task: use it for what it needs before your report (a finding that changes its plan, a question only it can answer), not for progress updates. `wait_for_agents` waits for its answer; wait only when you can't reasonably go on without one. After your report the lead may send you follow-up work: you continue with your conversation as it was.\n",
    );
    instructions.push_str(&tool_instructions(settings));
    let mut builder = AgentConfig::builder(
        WORKER.into(),
        "worker".into(),
        settings.worker.llm_config(settings.show_thinking)?,
    )
    .role_description(
        "A subagent that carries out one task the lead agent hands it (an investigation, a change, a review), with a shell, a terminal and file tools, and reports back.".into(),
    )
    .instructions(instructions)
    .tool_config(tool_config({
        let mut tools = shared_tool_names(settings);
        tools.extend(crew::WORKER_TOOL_NAMES.iter().map(|name| name.to_string()));
        tools
    }))
    .environment_details_config(environment())
    .complete_task_summary_prompt_override(
        "Your report to the lead agent: what you did and found, with evidence (paths, line numbers, commands and results), the files you changed, and anything unfinished or unconfirmed."
            .into(),
    )
    .handover(HandoverConfig::at_input_tokens(
        settings.worker.handover_at.unwrap_or(settings.handover_at),
    ));
    if let Some(skills) = skills::config(settings.skill_folders) {
        builder = builder.skills_config(skills);
    }
    builder.build().map_err(|error| error.to_string())
}

fn main_instructions(_settings: &AgentSettings) -> String {
    "# How this conversation works\n\
     The user talks with you in a terminal. Each of their messages starts a turn: work on it with your tools for as long as it takes, then end the turn by calling `complete_task`. Its summary is your reply: the user reads it as your message, so write your answer there in full. Text you write outside tool calls is shown to the user as you work; keep it to short progress notes, and don't repeat your reply in it.\n\n\
     When a step doesn't need the user, keep going. End the turn to ask only when you can't continue without them, or before something destructive or hard to undo that they didn't ask for: deleting data they may want, force-pushing, publishing, or changing things outside the working directory.\n\n\
     The user can type while you work. What they type reaches you as a note from the operator: take it into account in the work you're doing. When a note asks you something or needs an answer before you finish (a question, \"progress?\"), answer it with `message_user` and carry on with your work. Every response of yours must call a tool: a reply in plain text, without one, is an error.\n\n\
     When you need the user to decide something before you can go on (a choice between approaches with real trade-offs, missing information only they have), ask with `ask_user`: it waits for their answer and you continue. Ask only what you can't reasonably decide yourself; if they can't be asked (fin runs without a terminal), it says so, and you decide.\n"
        .to_string()
}

fn tool_instructions(settings: &AgentSettings) -> String {
    let uses = if settings.index_tools.is_empty() {
        "searching (prefer `rg`), building, testing, git, and running programs"
    } else {
        "building, testing, git, running programs, and searching text with `rg` (to find code or docs to read, start with `index_search`: see Search indexes below)"
    };
    let mut text = format!(
        "\n# Tools\n\
         `shell` runs a command in {shell} in the working directory and returns its exit code and output once it finishes. Use it for what you would do in a terminal: {uses}. Each call starts in the working directory, so a `cd` doesn't carry over to the next call; to run a command somewhere else, give the call that directory as `cwd` instead of a `cd`.\n\n\
         The file tools work with paths relative to the working directory. `read_file` reads a file with line numbers (give `start_line` and `end_line` to read part of a large one), and `list_files` lists a directory's files. Create files with `write_file` (the whole content), and change them with `apply_diff` (SEARCH/REPLACE blocks; it changes nothing if a SEARCH block doesn't match exactly, so read the lines first). Prefer them to heredocs and scripts in the shell, which fail on long text.\n\n\
         The terminal tools run programs in the background: servers, watchers, long builds you want to keep an eye on, and programs that ask questions. `terminal_start` returns a job ID at once, `terminal_status` reads a job's output, `terminal_input` answers a prompt, and `terminal_control` waits for a job, lists jobs or cancels one. Use them only when `shell` doesn't fit, and cancel the jobs you started once you no longer need them.\n\n\
         A command guard refuses commands that would delete, move or overwrite the filesystem root, the home directory, system directories or devices, and paths outside the working directory unless the user has turned on outside access; the file tools' writes follow the same rule. The user can turn outside access on and off at any time, and a note tells you when they do. A refusal is the guard working, not an obstacle: don't rephrase, split or encode the command to get past it. Do the work another way, or say what was refused and why you needed it.\n",
        shell = settings.shell_name,
    );
    if !settings.index_tools.is_empty() {
        text.push_str(
            "\n# Search indexes\n\
             `index_search` searches a codebase or its docs by meaning as well as by words. It returns the top results whole (the entire function, type or doc section, with line numbers as `read_file` shows them), so you don't need to read them again, and the rest as locations with their first lines. Use it first whenever you look for code or docs to read: to find where something is done or how it works (\"where are retries handled?\", \"how is the session saved?\"), and also when you know the name, since a query that names a function or type (\"Session::save\", \"parse_config\") returns its definition whole, where `rg` gives one line and needs another call to read the rest. One search often replaces several rounds of `rg`, `sed` and `read_file`; `detail: brief` lists more results in less space. Keep `rg` for listing every occurrence of a name (all the callers, say), for regexes, and for exact strings in logs, configuration and files no index covers.\n\n\
             Begin every task in an indexed codebase with it, before anything else. Whether you're asked a question about the code, a bug to find or a change to make, your first step is one or a few `index_search` queries that describe the behaviour or concept involved, before you list directories, run `rg` or open files. This is where the index saves the most: it finds the relevant code even when the words differ from the code's (a user's \"handoff\" for code that says \"handover\", which `rg` misses), and its results show where that code lives and how it works, so you don't read whole files to find your way. Go to `rg` and `read_file` once the searches have shown you where to look.\n\n",
        );
        text.push_str(&known_indexes(settings.indexes));
        text.push_str(
            "\n\nIndexes work for any directory, not just the working directory. To search another project, name its index in `index` (its name, or any directory or file inside it) or give a `path_prefix` inside it, instead of running `rg` there. When a non-trivial project you work in has no index, create one with `index_create` (`path: ../other-repo` for another project): it is kept current as files change. `index_status` lists the indexes as they are now, and `index_manage` rebuilds, re-embeds or removes one.\n",
        );
    }
    if !settings.mcp_servers.is_empty() {
        text.push_str(&format!(
            "\n# MCP servers\n\
             Tools named `mcp__<server>__<tool>` come from MCP servers the user set up ({}). Use them where they fit the task better than the shell. They act outside the command guard, so take the same care with what they change.\n",
            settings
                .mcp_servers
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !settings.skill_folders.is_empty() {
        text.push_str(&format!(
            "\n# Skills\n\
             Skills are instructions for particular kinds of work, from these folders: {}. When a skill's description fits your task, load it with `load_skill` before you start, and follow it. A skill's other files, such as scripts and templates, are in its folder (usually named after the skill) in one of these, and the paths a skill gives are relative to that folder.\n",
            settings
                .skill_folders
                .iter()
                .map(|folder| format!("`{}`", folder.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    text
}

/// The indexes the session started with, for the Search indexes section:
/// the working directory's first, at most [`INDEXES_NAMED`] of them.
fn known_indexes(indexes: &[index::Known]) -> String {
    if indexes.is_empty() {
        return "There were no indexes when this session started.".to_string();
    }
    let mut text = String::from("The indexes when this session started:");
    for known in indexes.iter().take(INDEXES_NAMED) {
        text.push_str(&format!(
            "\n- `{}`: {}, `{}`",
            known.name,
            known.kind,
            known.root.display()
        ));
        if let Some(place) = known.place {
            text.push_str(&format!(" ({place})"));
        }
    }
    if indexes.len() > INDEXES_NAMED {
        text.push_str(&format!(
            "\n- and {} more (`index_status` lists them)",
            indexes.len() - INDEXES_NAMED
        ));
    }
    text
}

fn worker_instructions(settings: &AgentSettings) -> String {
    let limit = if settings.max_workers == NO_LIMIT {
        "There is no limit on how many run at once.".to_string()
    } else {
        format!(
            "At most {} run at once. Additional starts wait in a queue.",
            settings.max_workers
        )
    };
    let mut text = format!(
        "\n# Workers\n\
         `start_agent` with `agent: worker` starts a worker: a subagent with the same tools (shell, terminal, files, search) that carries out one task while you go on with your own work. It returns at once with the worker's ID (r1, r2, ...), so start several in one response to run them in parallel. {limit} A worker doesn't see this conversation: give it everything it needs in `task`, namely the goal, the context and constraints, what done looks like, and what to report.\n\n\
         With `isolated: true` a worker works in its own git worktree: a separate checkout of the current commit, on a branch of its own, where it commits its changes. Use one for each worker that changes files, so workers don't edit the same files as each other or you; then review its branch and merge it into the working directory (`git merge` or `git cherry-pick`). A worktree has the committed files only: commit what a worker needs first. Without a worktree, a worker works in the working directory itself, which suits investigations, reviews and small changes to files no one else touches.\n\n\
         Workers report on their own. A worker's report, when it finishes, and its messages to you reach you as notes from run `r1` (and so on) while you work, and start a new turn for you if you have ended yours. So don't wait idly: do other work, and end your turn when you have something to tell the user, saying which workers are still running. When you can't go on without their results, `wait_for_agents` waits until one finishes or sends a message (or the user types something) and returns what arrived. `message_agent` addressed to its handle in `to` sends a worker a message: a running worker reads it at its next step, and a worker that has finished or was stopped continues its task with it, keeping its context, so give follow-up work to the worker that already knows the code. `list_agents` shows the workers and what they're doing; `stop_agent` stops the handle in `run`, and `remove_workspace: true` removes its worktree once you have merged its work.\n\n\
         Use workers for independent parts of a large task (exploring separate areas of a codebase, an audit split by area, changes in separate worktrees), or to keep a long investigation out of your own context. Do small things yourself. Check what a worker reports before you rely on it.\n"
    );
    if settings.worker.is_claude() {
        text.push_str(&format!(
            "\nA worker keeps a prompt cache of its conversation, and `start_agent`'s `cache` sets how long it lasts after the worker's last step: `1h` for a worker you expect to send messages or follow-up work after it reports, so continuing it reuses its cache, and `5m` for a quick one-off such as a research task (a 1h cache costs more to write: twice the input price instead of 1.25 times). The default is `{}`. Continuing a worker after its cache has expired rereads its whole conversation at full price: if it finished more than an hour ago, start a fresh worker instead, unless its context is worth that.\n",
            if settings.worker.cache_1h { "1h" } else { "5m" }
        ));
    } else if settings.worker.is_chatgpt() {
        text.push_str(
            "\nA worker keeps a prompt cache of its conversation for 30 minutes after its last step (ChatGPT's cache, which no setting changes). Continuing a worker after its cache has expired rereads its whole conversation uncached, which uses more of the ChatGPT plan's limits: if it finished more than 30 minutes ago, start a fresh worker instead, unless its context is worth that.\n",
        );
    }
    text
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
            mcp_tools: &[],
            mcp_servers: &[],
            skill_folders: &[],
            index_tools: &[],
            indexes: &[],
        }
    }

    fn model(name: &str, fast: bool) -> ModelSettings {
        ModelSettings {
            model: resolve_model(name).unwrap().id,
            effort: Effort::Xhigh,
            fast,
            cache_1h: false,
            handover_at: None,
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
            handover_at: None,
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
    fn workers_are_offered_only_when_enabled() {
        let opus = model("opus", false);
        let mut with = settings(&opus, &opus);
        let offers = |config: &AgentConfig, tool: &str| {
            config
                .tool_config
                .available_tool_names
                .iter()
                .any(|name| name == tool)
        };
        let config = main_config(&with).unwrap();
        for tool in crew::LEAD_TOOL_NAMES {
            assert!(offers(&config, tool), "{tool}");
        }
        assert!(config.instructions.contains("At most 4 run at once."));
        let worker = worker_config(&with).unwrap();
        for tool in crew::WORKER_TOOL_NAMES {
            assert!(offers(&worker, tool), "{tool}");
            assert!(offers(&config, tool), "{tool}");
        }
        assert!(!offers(&worker, crew::START_AGENT));
        assert!(!offers(&worker, ASK_USER_TOOL_NAME));
        with.max_workers = NO_LIMIT;
        let config = main_config(&with).unwrap();
        assert!(config.instructions.contains("There is no limit"));
        with.max_workers = 0;
        let config = main_config(&with).unwrap();
        assert!(!offers(&config, crew::START_AGENT));
        assert!(!config.instructions.contains("# Workers"));
        assert!(offers(&config, ASK_USER_TOOL_NAME));
    }

    /// The file tools' configuration lets them read and write anywhere:
    /// outside access decides their writes outside the working directory
    /// as the session runs (`outside`), so the configuration, which a
    /// resume needs unchanged, doesn't depend on it. The instructions say
    /// the rule without saying whether it is on.
    #[test]
    fn the_file_tools_write_where_outside_access_lets_them() {
        let opus = model("opus", false);
        let policy = |config: &AgentConfig, tool: &str| {
            config.tool_config.tool_specific_configs[tool]
                .out_of_workspace_path_policy
                .clone()
                .unwrap()
        };
        let with = settings(&opus, &opus);
        for config in [main_config(&with).unwrap(), worker_config(&with).unwrap()] {
            for tool in FILE_TOOL_NAMES {
                assert!(
                    config
                        .tool_config
                        .available_tool_names
                        .iter()
                        .any(|name| name == tool),
                    "{tool}"
                );
            }
            assert!(config.tool_config.read_file_line_ranges);
            for tool in FILE_TOOL_NAMES {
                assert_eq!(policy(&config, tool), PathSafetyPolicy::AutoApprove);
            }
            assert!(crate::outside::files_follow(&config));
            assert!(config.instructions.contains("`apply_diff`"));
            assert!(config.instructions.contains(
                "paths outside the working directory unless the user has turned on outside access"
            ));
        }
    }

    #[test]
    fn mcp_index_and_skill_tools_reach_both_agents_with_their_instructions() {
        let opus = model("opus", false);
        let plain = settings(&opus, &opus);
        for config in [main_config(&plain).unwrap(), worker_config(&plain).unwrap()] {
            assert!(config.skills_config.is_none());
            for section in ["# MCP servers", "# Skills", "# Search indexes"] {
                assert!(!config.instructions.contains(section), "{section}");
            }
        }
        let tools = ["mcp__github__create_issue".to_string()];
        let servers = ["github".to_string()];
        let folders = [std::path::PathBuf::from("/work/.claude/skills")];
        let with = AgentSettings {
            mcp_tools: &tools,
            mcp_servers: &servers,
            skill_folders: &folders,
            index_tools: &["index_search", "index_create"],
            ..settings(&opus, &opus)
        };
        for config in [main_config(&with).unwrap(), worker_config(&with).unwrap()] {
            let names = &config.tool_config.available_tool_names;
            for tool in [
                "shell",
                "mcp__github__create_issue",
                "index_search",
                "index_create",
            ] {
                assert!(names.iter().any(|name| name == tool), "{tool}");
            }
            let skills = config.skills_config.as_ref().unwrap();
            assert_eq!(skills.global_sources[0].path, "/work/.claude/skills");
            assert!(
                config
                    .instructions
                    .contains("from MCP servers the user set up (`github`)")
            );
            assert!(
                config
                    .instructions
                    .contains("from these folders: `/work/.claude/skills`")
            );
            assert!(config.instructions.contains("`index_search` searches"));
        }
    }

    /// With the index tools, both agents are told to read code through
    /// `index_search` before `rg`, and which indexes there are: the working
    /// directory's first, at most `INDEXES_NAMED`.
    #[test]
    fn the_index_section_names_the_indexes_and_puts_search_before_rg() {
        let opus = model("opus", false);
        let plain = main_config(&settings(&opus, &opus)).unwrap();
        assert!(plain.instructions.contains("searching (prefer `rg`)"));

        let none = AgentSettings {
            index_tools: crate::index::TOOL_NAMES,
            ..settings(&opus, &opus)
        };
        let config = main_config(&none).unwrap();
        assert!(!config.instructions.contains("searching (prefer `rg`)"));
        assert!(
            config
                .instructions
                .contains("to find code or docs to read, start with `index_search`")
        );
        // And to explore with it at the start of every task.
        assert!(
            config
                .instructions
                .contains("Begin every task in an indexed codebase with it, before anything else.")
        );
        assert!(
            config
                .instructions
                .contains("There were no indexes when this session started.")
        );

        let known = |name: &str, root: &str, place: Option<&'static str>| index::Known {
            name: name.into(),
            kind: "code",
            root: root.into(),
            place,
        };
        let indexes = [
            known("finn", "/work/finn", Some("the working directory")),
            known("arko", "/work/arko", None),
        ];
        let with = AgentSettings {
            indexes: &indexes,
            ..none
        };
        for config in [main_config(&with).unwrap(), worker_config(&with).unwrap()] {
            assert!(config.instructions.contains(
                "The indexes when this session started:\n- `finn`: code, `/work/finn` (the working directory)\n- `arko`: code, `/work/arko`\n\nIndexes work for any directory"
            ));
        }

        let many: Vec<index::Known> = (0..INDEXES_NAMED + 3)
            .map(|n| known(&format!("i{n:02}"), &format!("/work/i{n:02}"), None))
            .collect();
        let config = main_config(&AgentSettings {
            indexes: &many,
            ..none
        })
        .unwrap();
        let last = format!("`i{:02}`", INDEXES_NAMED - 1);
        assert!(config.instructions.contains(&last), "{last}");
        let first_left_out = format!("`i{INDEXES_NAMED:02}`");
        assert!(!config.instructions.contains(&first_left_out));
        assert!(
            config
                .instructions
                .contains("- and 3 more (`index_status` lists them)")
        );
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

    #[test]
    fn claude_models_hand_over_at_500k_chatgpt_at_250k_and_others_at_200k() {
        let opus = resolve_model("opus").unwrap();
        assert_eq!(default_handover(&opus, false), 500_000);
        assert_eq!(default_handover(&opus, true), 500_000);
        let sol = resolve_model("sol").unwrap();
        // The ChatGPT backend's 272k, less the room: not the API's limit.
        assert!(sol.max_input_tokens > 272_000);
        assert_eq!(default_handover(&sol, true), 250_000);
        assert_eq!(default_handover(&sol, false), 200_000);
        // Never past 80% of what the model accepts.
        let mut small = opus.clone();
        small.max_input_tokens = 200_000;
        assert_eq!(default_handover(&small, false), 160_000);
        let mut small = sol.clone();
        small.max_input_tokens = 128_000;
        assert_eq!(default_handover(&small, true), 106_000);

        let mut main = model("opus", false);
        main.handover_at = Some(default_handover(&opus, false));
        let worker = model("sol", false);
        let settings = settings(&main, &worker);
        let handover = |config: &AgentConfig| config.handover.clone().unwrap().at_input_tokens;
        assert_eq!(handover(&main_config(&settings).unwrap()), Some(500_000));
        // A session saved before it was set per model uses its own size.
        assert_eq!(handover(&worker_config(&settings).unwrap()), Some(200_000));
    }

    #[test]
    fn the_lead_answers_with_message_user_and_chooses_claude_workers_caches() {
        let opus = model("opus", false);
        let sol = model("sol", false);
        let config = main_config(&settings(&opus, &opus)).unwrap();
        let tools = &config.tool_config.available_tool_names;
        assert!(
            tools
                .iter()
                .any(|name| name == crate::message::MESSAGE_USER)
        );
        assert!(config.instructions.contains("`message_user`"));
        assert!(config.instructions.contains("`cache`"), "Claude workers");
        let config = main_config(&settings(&sol, &sol)).unwrap();
        assert!(!config.instructions.contains("`cache`"), "OpenAI workers");
        assert!(
            config
                .instructions
                .contains("for 30 minutes after its last step"),
            "ChatGPT's cache"
        );
        let worker = worker_config(&settings(&opus, &opus)).unwrap();
        assert!(
            !worker
                .tool_config
                .available_tool_names
                .iter()
                .any(|name| name == crate::message::MESSAGE_USER)
        );
    }
}

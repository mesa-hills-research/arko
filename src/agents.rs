//! The agents: the main agent the user talks with, and the workers it can
//! hand tasks to. Their configurations depend only on the session's
//! settings, so a resumed session builds them again unchanged.

use arko_agents::agent_config::PathSafetyPolicy;
use arko_agents::builtins::{
    ASK_USER_TOOL_NAME, EDIT_FILE_TOOL_NAME, LIST_FILES_TOOL_NAME, READ_FILE_TOOL_NAME,
    WRITE_FILE_TOOL_NAME,
};
use arko_agents::terminal::TerminalTool;
use arko_agents::{
    AgentConfig, AgentLLMConfig, AgentToolConfig, BuiltInEnvironmentDetail,
    EnvironmentDetailPlacement, EnvironmentDetailsConfig, HandoverConfig, PerToolConfigOverride,
};
use arko_ai::input_limit::effective_max_input_tokens;
use arko_ai::{AnthropicReasoningEffort, Model, OpenAIEffort, Provider};
use serde::{Deserialize, Serialize};

use crate::crew;
use crate::index;
use crate::providers::{self, OpenAiAuth};
use crate::shell::SHELL_TOOL_NAME;
use crate::skills;

/// The agent the user talks with.
pub const MAIN: &str = "main";
/// The agent `main` hands tasks to.
pub const WORKER: &str = "worker";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
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

/// Which agents get the extra rules for small models in their
/// instructions ([`extra_rules`]). Each agent goes by its own model: a
/// worker on a small model under a lead on a large one gets them, and the
/// lead doesn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtraRules {
    /// Agents on small models ([`providers::is_small`]): new sessions'
    /// default.
    Small,
    /// Every agent, whatever its model.
    All,
    /// No agent. Sessions saved before the rules existed have them off, as
    /// their saved agents were built.
    #[default]
    Off,
}

impl ExtraRules {
    pub const ALL: [ExtraRules; 3] = [ExtraRules::Small, ExtraRules::All, ExtraRules::Off];

    /// The name `/extra-rules` takes and shows.
    pub fn name(self) -> &'static str {
        match self {
            ExtraRules::Small => "small",
            ExtraRules::All => "all",
            ExtraRules::Off => "off",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|rules| rules.name().eq_ignore_ascii_case(text.trim()))
    }

    /// Who gets the rules, as fin tells the user.
    pub fn describe(self) -> &'static str {
        match self {
            ExtraRules::Small => "only agents on small models (Haiku, luna) get them",
            ExtraRules::All => "every agent gets them",
            ExtraRules::Off => "no agent gets them",
        }
    }

    /// Whether an agent on `model` gets them.
    pub fn for_model(self, model: &ModelSettings) -> bool {
        match self {
            ExtraRules::Small => {
                resolve_model(&model.model).is_ok_and(|model| providers::is_small(&model))
            }
            ExtraRules::All => true,
            ExtraRules::Off => false,
        }
    }

    pub fn is_off(&self) -> bool {
        *self == ExtraRules::Off
    }
}

/// Models are named and resolved by [`crate::providers`].
pub use crate::providers::resolve_model;

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
        // A model that doesn't reason takes no effort.
        let reasons = resolve_model(&self.model)
            .map_or(true, |model| !crate::providers::efforts(&model).is_empty());
        let mut details = Vec::new();
        if reasons {
            details.push(format!("{} effort", self.effort));
        }
        if self.fast {
            details.push("fast".to_string());
        }
        if self.cache_1h {
            details.push("1h cache".to_string());
        }
        if details.is_empty() {
            self.model.clone()
        } else {
            format!("{} ({})", self.model, details.join(", "))
        }
    }

    pub fn provider(&self) -> Result<Provider, String> {
        Ok(resolve_model(&self.model)?.provider)
    }

    /// A Claude model in Claude Code.
    pub fn in_claude_code(&self) -> bool {
        self.provider()
            .is_ok_and(|provider| provider == Provider::ClaudeCode)
    }

    /// A model that takes Anthropic's cache lifetimes, so a worker's
    /// `cache` chooses one: Claude by API key or on OpenRouter.
    fn takes_cache_ttl(&self) -> bool {
        resolve_model(&self.model).is_ok_and(|model| providers::takes_cache_ttl(&model))
    }

    fn llm_config(&self, show_thinking: bool) -> Result<AgentLLMConfig, String> {
        let model = resolve_model(&self.model)?;
        let generation_config =
            providers::generation(&model, self.effort, self.fast, self.cache_1h, show_thinking)?;
        Ok(AgentLLMConfig {
            model,
            generation_config,
        })
    }
}

/// What the agent configurations are built from.
pub struct AgentSettings<'a> {
    pub main: &'a ModelSettings,
    pub worker: &'a ModelSettings,
    /// What the configurations take from the session's providers: how it
    /// reaches OpenAI models, and how Claude Code runs.
    pub providers: &'a providers::Setup,
    /// How many workers may run at once; 0 turns them off, and
    /// [`NO_LIMIT`] sets no limit.
    pub max_workers: u32,
    pub handover_at: usize,
    pub show_thinking: bool,
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
    /// The main agent ends its turns with a reply in plain text (see
    /// `Settings::text_replies`).
    pub text_replies: bool,
    /// Which agents get the extra rules for small models, each by its own
    /// model.
    pub extra_rules: ExtraRules,
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
/// unless `--handover-at` says otherwise, in a session that reaches OpenAI
/// models by `openai_auth`. An OpenAI model on the ChatGPT login hands over
/// [`CHATGPT_HANDOVER_ROOM`] below what that login's backend accepts
/// ([`CHATGPT_OAUTH_MAX_INPUT_TOKENS`](arko_ai::input_limit::CHATGPT_OAUTH_MAX_INPUT_TOKENS),
/// 272k, less than the API's limit), so at 250k. Others hand over at 500k
/// on Claude models (by API key, on OpenRouter or in Claude Code), 200k on
/// the rest, and at most 80% of what the model accepts;
/// [`providers::UNKNOWN_LIMIT_HANDOVER`] when that isn't known (an
/// OpenRouter model without its catalog).
pub fn default_handover(model: &Model, openai_auth: Option<OpenAiAuth>) -> usize {
    if model.provider == Provider::OpenAI
        && openai_auth == Some(OpenAiAuth::ChatGpt)
        && let Some(limit) = effective_max_input_tokens(model, true)
    {
        return limit.saturating_sub(CHATGPT_HANDOVER_ROOM);
    }
    let tokens = match model.provider {
        Provider::Anthropic | Provider::ClaudeCode => 500_000,
        Provider::OpenRouter if providers::is_claude_slug(&model.id) => 500_000,
        _ => 200_000,
    };
    match model.max_input_tokens {
        0 => providers::UNKNOWN_LIMIT_HANDOVER,
        limit => tokens.min(limit / 5 * 4),
    }
}

/// What an agent on `model` does at `tokens` of input: in Claude Code,
/// Claude Code compacts the conversation (its own `/compact`) and the
/// agent goes on in the same conversation. On the other providers it hands
/// over: it writes a report and goes on from it in a fresh conversation.
fn handover(model: &ModelSettings, tokens: usize) -> HandoverConfig {
    HandoverConfig {
        compact_in_claude_code: model.in_claude_code(),
        ..HandoverConfig::at_input_tokens(tokens)
    }
}

/// Whether an agent with `config` compacts in Claude Code instead of
/// handing over. A session saved before fin compacted keeps handing over.
pub fn compacts(config: &AgentConfig) -> bool {
    config.runs_in_claude_code()
        && config
            .handover
            .as_ref()
            .is_some_and(|handover| handover.compact_in_claude_code)
}

/// arko's built-in file tools, which both agents have. (Sessions saved
/// before `edit_file` have `apply_diff` instead, which stays registered for
/// them.)
pub const FILE_TOOL_NAMES: [&str; 4] = [
    READ_FILE_TOOL_NAME,
    LIST_FILES_TOOL_NAME,
    WRITE_FILE_TOOL_NAME,
    EDIT_FILE_TOOL_NAME,
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
    let overrides = FILE_TOOL_NAMES
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

/// How the main agent ends its turns, as fin tells the user: with
/// `text_replies`, a plain reply, else a `complete_task` call.
pub fn replies_text(text_replies: bool) -> &'static str {
    if text_replies {
        "the agent ends its turn with a plain reply"
    } else {
        "the agent ends its turn by calling complete_task"
    }
}

/// The main agent's configuration. With `text_replies` it ends its turns
/// with a reply in plain text (`text_reply_ends_task`), and answers what
/// the user types meanwhile in text beside its calls: it has neither
/// `complete_task` nor `message_user`. Without, it ends them with
/// `complete_task`, whose summary is its reply, and answers with
/// `message_user`.
pub fn main_config(settings: &AgentSettings) -> Result<AgentConfig, String> {
    let mut tools = shared_tool_names(settings);
    tools.push(ASK_USER_TOOL_NAME.into());
    if !settings.text_replies {
        tools.push(crate::message::MESSAGE_USER.into());
    }
    if settings.max_workers > 0 {
        tools.extend(crew::LEAD_TOOL_NAMES.iter().map(|name| name.to_string()));
    }
    let mut instructions = main_instructions(settings);
    instructions.push_str(&tool_instructions(settings, false));
    if settings.max_workers > 0 {
        instructions.push_str(&worker_instructions(settings));
    }
    if settings.extra_rules.for_model(settings.main)
        && let Some(rules) = extra_rules(settings)
    {
        instructions.push_str(&rules);
    }
    let mut tool_config = tool_config(tools);
    tool_config.text_reply_ends_task = settings.text_replies;
    let mut builder = AgentConfig::builder(
        MAIN.into(),
        "fin".into(),
        settings.main.llm_config(settings.show_thinking)?,
    )
    .role_description(
        "You are fin, a general-purpose agent working in a terminal on the user's computer. You help with software engineering and anything else the user asks: you investigate, run commands, create and change files, and report back.".into(),
    )
    .instructions(instructions)
    .tool_config(tool_config)
    .environment_details_config(environment())
    .handover(handover(
        settings.main,
        settings.main.handover_at.unwrap_or(settings.handover_at),
    ));
    if !settings.text_replies {
        builder = builder.complete_task_summary_prompt_override(
            "Your reply to the user's latest message, in full: what you did, what you found, and anything you need from them. They read it as your message (Markdown, shown as plain text)."
                .into(),
        );
    }
    if let Some(skills) = skills::config(settings.skill_folders) {
        builder = builder.skills_config(skills);
    }
    if settings.main.in_claude_code() {
        builder = builder.claude_code(settings.providers.claude_code.clone());
    }
    builder.build().map_err(|error| error.to_string())
}

pub fn worker_config(settings: &AgentSettings) -> Result<AgentConfig, String> {
    let mut instructions = String::from(
        "# Your task\n\
         Make reasonable choices yourself, and say which in your report. If something blocks you, report what and why.\n\n\
         # The lead\n\
         The lead's messages reach you as workflow context updates from your lead: take them into account. A message that arrives as you finish comes back to you, and you report again. After your report, the lead may send you follow-up work, which continues this conversation.\n",
    );
    instructions.push_str(&tool_instructions(settings, true));
    if settings.extra_rules.for_model(settings.worker)
        && let Some(rules) = extra_rules(settings)
    {
        instructions.push_str(&rules);
    }
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
        "Your report to the lead, the only thing it sees of your work: what you did and found, with the evidence (file paths and line numbers, the commands you ran and what they showed), every file you changed and the commits you made, and anything unfinished. Mark what you couldn't confirm, and say where you looked."
            .into(),
    )
    .handover(handover(
        settings.worker,
        settings.worker.handover_at.unwrap_or(settings.handover_at),
    ));
    if let Some(skills) = skills::config(settings.skill_folders) {
        builder = builder.skills_config(skills);
    }
    if settings.worker.in_claude_code() {
        builder = builder.claude_code(settings.providers.claude_code.clone());
    }
    builder.build().map_err(|error| error.to_string())
}

fn main_instructions(settings: &AgentSettings) -> String {
    if settings.text_replies {
        return "# How this conversation works\n\
         The user talks with you in a terminal. Each of their messages starts a turn: work on it with your tools for as long as it takes. When you're done, reply without calling a tool. That reply ends your turn, and the user reads it as your message, so write your answer there in full. Text you write beside tool calls is shown to the user as you work: keep it to short progress notes.\n\n\
         When a step doesn't need the user, keep going. End the turn to ask only when you can't continue without them, or before something destructive or hard to undo that they didn't ask for: deleting data they may want, force-pushing, publishing, or changing things outside the working directory.\n\n\
         The user can type while you work. What they type reaches you as a note from the operator: take it into account in the work you're doing. When a note asks you something or needs an answer before you finish (a question, \"progress?\"), answer it in text beside your next tool call and carry on with your work. Only a response without a tool call ends your turn, so don't answer a note that way unless you're done.\n\n\
         When you need the user to decide something before you can go on (a choice between approaches with real trade-offs, missing information only they have), ask with `ask_user`. If they can't be asked (fin runs without a terminal), it says so, and you decide.\n"
            .to_string();
    }
    "# How this conversation works\n\
     The user talks with you in a terminal. Each of their messages starts a turn: work on it with your tools for as long as it takes, then end the turn by calling `complete_task`, whose summary is your reply. Text you write outside tool calls is shown to the user as you work; keep it to short progress notes, and don't repeat your reply in it.\n\n\
     When a step doesn't need the user, keep going. End the turn to ask only when you can't continue without them, or before something destructive or hard to undo that they didn't ask for: deleting data they may want, force-pushing, publishing, or changing things outside the working directory.\n\n\
     The user can type while you work. What they type reaches you as a note from the operator: take it into account in the work you're doing. When a note asks you something or needs an answer before you finish (a question, \"progress?\"), answer it with `message_user` and carry on with your work. Use `message_user` too for anything they must know before you finish. Every response of yours must call a tool: a reply in plain text, without one, is an error.\n\n\
     When you need the user to decide something before you can go on (a choice between approaches with real trade-offs, missing information only they have), ask with `ask_user`. If they can't be asked (fin runs without a terminal), it says so, and you decide.\n"
        .to_string()
}

/// The Tools section and the sections of the tools the session has, for
/// the main agent or (`worker`) a worker.
fn tool_instructions(settings: &AgentSettings, worker: bool) -> String {
    let uses = if settings.index_tools.is_empty() {
        "searching (prefer `rg`), building, testing, git, and running programs"
    } else {
        "building, testing, git, running programs, and searching text with `rg` (to find code or docs to read, start with `index_search`: see Search indexes below)"
    };
    let mut text = format!(
        "\n# Tools\n\
         Paths and file names the user gives are relative to the working directory unless they say otherwise: look for them there, not across the disk.\n\n\
         Use `shell` for what you would do in a terminal: {uses}. Use the terminal tools only when `shell` doesn't fit: for servers, watchers, long builds you want to keep an eye on, and programs that ask questions. Cancel the jobs you start once you no longer need them.\n\n\
         Change files with `edit_file` and `write_file` rather than with heredocs and scripts in the shell, which fail on long text. Use the shell for mechanical changes across many files, such as a rename with `sed`.\n\n\
         A command guard refuses commands that would delete, move or overwrite the filesystem root, the home directory, system directories or devices, and paths outside the working directory unless the user has turned on outside access. The file tools' writes follow the same rule. The user can turn outside access on and off at any time, and a note tells you when they do. A refusal is the guard working, not an obstacle: don't rephrase, split or encode the command to get past it. Do the work another way, or say what was refused and why you needed it.\n",
    );
    if !settings.index_tools.is_empty() {
        text.push_str(
            "\n# Search indexes\n\
             When the code you work on has an index (see the list below), begin every task with `index_search`, before anything else. Whether you're asked a question about the code, a bug to find or a change to make, your first step is one or a few searches that describe the behaviour or concept involved, before you list directories, run `rg` or open files. This is where the index saves the most: it finds the relevant code even when the words differ from the code's (a user's \"handoff\" for code that says \"handover\", which `rg` misses), and its results show where that code lives and how it works, so you don't read whole files to find your way. Go to `rg` and `read_file` once the searches have shown you where to look.\n\n",
        );
        text.push_str(&known_indexes(settings.indexes));
        // A worker's worktree is a checkout elsewhere: the index of the
        // lead's working directory doesn't cover it, and workers on every
        // model searched there without naming it.
        if worker
            && let Some(project) = settings
                .indexes
                .iter()
                .take(INDEXES_NAMED)
                .find(|known| known.place.is_some())
        {
            text.push_str(&format!(
                "\n\nIf you work in a worktree of your own, no index covers it. To search the project, name its index (`index: {}`): the results show the lead's copies of the files, which are at the same paths in your worktree.",
                project.name
            ));
        }
        text.push_str(
            "\n\nTo search another project, use its index instead of running `rg` there. When a non-trivial project you work in has no index, create one with `index_create`.\n",
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
/// the working directory's first, at most [`INDEXES_NAMED`] of them. When
/// none is the working directory, holds it or lies in it, that is said
/// outright: smaller models search anyway when only a label is missing.
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
    if indexes.iter().all(|known| known.place.is_none()) {
        text.push_str("\nNone of them covers the working directory.");
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
         `start_agent` with `agent: worker` starts a worker: a subagent with the same tools (shell, terminal, files, search) that carries out one task while you go on with your own work. {limit}\n\n\
         With `isolated: true` a worker works in its own git worktree: a separate checkout of the current commit, on a branch of its own, where it commits its changes. Use one for each worker that changes files, so workers don't edit the same files as each other or you; then review its branch and merge it into the working directory (`git merge` or `git cherry-pick`). A worktree has the committed files only: commit what a worker needs first. Without a worktree, a worker works in the working directory itself, which suits investigations, reviews and small changes to files no one else touches.\n\n\
         Workers report on their own: a worker's report, and its messages to you, reach you as notes from its run (`r1` and so on) while you work, and start a new turn for you if you have ended yours. So don't wait idly: do other work, and end your turn when you have something to tell the user, saying which workers are still running. Use `wait_for_agents` only when you can't go on without their results. Give follow-up work to the worker that already knows the code, with `message_agent`. Once you have merged an isolated worker's branch, remove its worktree with `stop_agent` and `remove_workspace: true`.\n\n\
         Use workers for independent parts of a large task (exploring separate areas of a codebase, an audit split by area, changes in separate worktrees), or to keep a long investigation out of your own context. Do small things yourself. Check what a worker reports before you rely on it.\n"
    );
    let openai = settings.worker.provider() == Ok(Provider::OpenAI);
    if settings.worker.takes_cache_ttl() {
        text.push_str(&format!(
            "\nA worker keeps a prompt cache of its conversation. Set `start_agent`'s `cache` to `1h` for a worker you expect to send messages or follow-up work after it reports, so continuing it reuses its cache, and to `5m` for a quick one-off such as a research task (a 1h cache costs more to write: twice the input price instead of 1.25 times). The default is `{}`. Continuing a worker after its cache has expired rereads its whole conversation at full price: if it finished more than an hour ago, start a fresh worker instead, unless its context is worth that.\n",
            if settings.worker.cache_1h { "1h" } else { "5m" }
        ));
    } else if openai && settings.providers.openai_auth != Some(OpenAiAuth::ApiKey) {
        text.push_str(
            "\nA worker keeps a prompt cache of its conversation for 30 minutes after its last step (ChatGPT's cache, which no setting changes). Continuing a worker after its cache has expired rereads its whole conversation uncached, which uses more of the ChatGPT plan's limits: if it finished more than 30 minutes ago, start a fresh worker instead, unless its context is worth that.\n",
        );
    } else if settings.worker.in_claude_code() {
        text.push_str(
            "\nA worker runs in Claude Code, which keeps a prompt cache of its conversation for an hour after its last step on a Claude plan (no setting changes it). Continuing a worker after its cache has expired rereads its whole conversation uncached, which uses more of the plan's limits: if it finished more than an hour ago, start a fresh worker instead, unless its context is worth that.\n",
        );
    }
    text
}

/// The extra rules for agents on small models (see [`ExtraRules`]), when
/// any applies: short commands for what the large models do without being
/// told. Each comes from a mistake the small models made in fin's evals
/// (`evals/`) that the large ones didn't.
fn extra_rules(settings: &AgentSettings) -> Option<String> {
    let mut rules: Vec<&str> = Vec::new();
    if !settings.index_tools.is_empty() {
        // luna searched anyway, and the search failed.
        if settings.indexes.iter().all(|known| known.place.is_none()) {
            rules.push(
                "No index covers the working directory, so `index_search` can't search it. Search it with `rg`, or create its index with `index_create` first.",
            );
        }
    }
    if rules.is_empty() {
        return None;
    }
    let mut text = String::from("\n# Extra rules\n");
    for rule in rules {
        text.push_str(&format!("- {rule}\n"));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arko_agents::builtins::APPLY_DIFF_TOOL_NAME;
    use arko_ai::{AnthropicCacheTtl, OpenAIServiceTier};

    fn settings<'a>(main: &'a ModelSettings, worker: &'a ModelSettings) -> AgentSettings<'a> {
        AgentSettings {
            main,
            worker,
            max_workers: 4,
            handover_at: 200_000,
            show_thinking: false,
            mcp_tools: &[],
            mcp_servers: &[],
            skill_folders: &[],
            index_tools: &[],
            indexes: &[],
            providers: Box::leak(Box::new(providers::Setup::new(None))),
            text_replies: false,
            extra_rules: ExtraRules::Off,
        }
    }

    fn model(name: &str, fast: bool) -> ModelSettings {
        ModelSettings {
            model: providers::model_name(&resolve_model(name).unwrap()),
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
    fn aliases_and_ids_name_models_of_every_provider() {
        assert_eq!(resolve_model("opus").unwrap().id, "claude-opus-5-5");
        assert_eq!(resolve_model("Sol").unwrap().id, "gpt-6.1-sol");
        assert_eq!(
            resolve_model("claude-fable-5-1").unwrap().id,
            "claude-fable-5-1"
        );
        assert_eq!(
            resolve_model("gemini-3.5-flash").unwrap().provider,
            Provider::Google
        );
        assert!(resolve_model("no-such-model").is_err());
    }

    /// Agents in Claude Code get fin's own tools, served to it, and none of
    /// Claude Code's: the command guard sees every command. Agents on other
    /// providers get no Claude Code settings.
    #[test]
    fn claude_code_agents_run_fin_s_tools_only() {
        let opus = model("cc:opus", false);
        let fable = model("cc:fable", false);
        let with = settings(&opus, &fable);
        for config in [main_config(&with).unwrap(), worker_config(&with).unwrap()] {
            assert!(config.runs_in_claude_code());
            let claude_code = config.claude_code.as_ref().unwrap();
            assert!(claude_code.replace_system_prompt);
            assert_eq!(claude_code.env["ANTHROPIC_API_KEY"], "");
            let tools = &config.tool_config.available_tool_names;
            assert!(tools.iter().any(|name| name == SHELL_TOOL_NAME));
            assert!(
                !tools
                    .iter()
                    .any(|name| arko_agents::claude_code::is_built_in_tool(name)),
                "{tools:?}"
            );
            assert!(config.validate().is_ok());
        }
        let main = main_config(&with).unwrap();
        assert_eq!(main.llm_config.model.id, "claude-opus-5-5");
        assert!(
            main.instructions.contains("A worker runs in Claude Code"),
            "{}",
            main.instructions
        );
        assert!(!main.instructions.contains("`cache`"));
        let worker = worker_config(&with).unwrap();
        assert_eq!(worker.llm_config.model.id, "claude-fable-5-1");

        let api = model("opus", false);
        let config = main_config(&settings(&api, &opus)).unwrap();
        assert!(config.claude_code.is_none());
        assert!(
            worker_config(&settings(&api, &opus))
                .unwrap()
                .claude_code
                .is_some()
        );
    }

    /// Agents in Claude Code compact at the handover size, and agents on
    /// the other providers hand over there.
    #[test]
    fn claude_code_agents_compact_and_others_hand_over() {
        let cc = model("cc:opus", false);
        let api = model("opus", false);
        let mixed = settings(&cc, &api);
        let main = main_config(&mixed).unwrap();
        assert!(compacts(&main));
        assert_eq!(main.handover.unwrap().at_input_tokens, Some(200_000));
        let worker = worker_config(&mixed).unwrap();
        assert!(!compacts(&worker));
        assert!(!worker.handover.unwrap().compact_in_claude_code);
        let mixed = settings(&api, &cc);
        assert!(!compacts(&main_config(&mixed).unwrap()));
        assert!(compacts(&worker_config(&mixed).unwrap()));
    }

    /// Each worker provider's cache gets its own paragraph: the `cache`
    /// choice where Anthropic's cache lifetimes apply, ChatGPT's 30
    /// minutes, Claude Code's hour, and nothing where fin can't tell.
    #[test]
    fn the_lead_hears_about_its_workers_caches_by_provider() {
        let lead = model("opus", false);
        let instructions = |worker: &ModelSettings, auth: Option<OpenAiAuth>| {
            let setup = providers::Setup::new(auth);
            let with = AgentSettings {
                providers: &setup,
                ..settings(&lead, worker)
            };
            main_config(&with).unwrap().instructions
        };
        let openrouter_claude = model("openrouter:anthropic/claude-opus-5.5", false);
        assert!(instructions(&openrouter_claude, None).contains("`cache`"));
        let sol = model("sol", false);
        assert!(instructions(&sol, None).contains("for 30 minutes after its last step"));
        assert!(
            !instructions(&sol, Some(OpenAiAuth::ApiKey)).contains("30 minutes"),
            "an API key's cache isn't ChatGPT's"
        );
        let gemini = model("gemini", false);
        let text = instructions(&gemini, None);
        assert!(
            !text.contains("`cache`")
                && !text.contains("30 minutes")
                && !text.contains("an hour after")
        );
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
            assert!(config.instructions.contains("`edit_file`"));
            assert!(!config.instructions.contains("apply_diff"));
            assert!(
                !config
                    .tool_config
                    .available_tool_names
                    .iter()
                    .any(|name| name == APPLY_DIFF_TOOL_NAME)
            );
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
            assert!(
                config
                    .instructions
                    .contains("begin every task with `index_search`")
            );
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
        assert!(config.instructions.contains(
            "When the code you work on has an index (see the list below), begin every task with `index_search`, before anything else."
        ));
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
        let list = "The indexes when this session started:\n- `finn`: code, `/work/finn` (the working directory)\n- `arko`: code, `/work/arko`\n\n";
        assert!(
            main_config(&with)
                .unwrap()
                .instructions
                .contains(&format!("{list}To search another project, use its index"))
        );
        // A worker is told how to search from a worktree of its own, which
        // no index covers: by the index of the lead's working directory.
        assert!(worker_config(&with).unwrap().instructions.contains(&format!(
            "{list}If you work in a worktree of your own, no index covers it. To search the project, name its index (`index: finn`): the results show the lead's copies of the files, which are at the same paths in your worktree.\n\nTo search another project"
        )));
        assert!(
            !main_config(&with)
                .unwrap()
                .instructions
                .contains("worktree of your own, no index")
        );

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
        assert!(config.instructions.contains(
            "- and 3 more (`index_status` lists them)\nNone of them covers the working directory.\n\nTo search another project"
        ));
        // With no index of the lead's working directory, there's none to
        // name from a worktree.
        let worker = worker_config(&AgentSettings {
            indexes: &many,
            ..none
        })
        .unwrap();
        assert!(
            !worker
                .instructions
                .contains("worktree of your own, no index")
        );
        // Said only when no index is the working directory, holds it or
        // lies in it.
        for place in ["holds the working directory", "in the working directory"] {
            let indexes = [
                known("arko", "/work/arko", None),
                known("x", "/x", Some(place)),
            ];
            let config = main_config(&AgentSettings {
                indexes: &indexes,
                ..none
            })
            .unwrap();
            assert!(
                !config.instructions.contains("None of them covers"),
                "{place}"
            );
        }
    }

    /// What a tool's description says (how it behaves and how to call it)
    /// isn't said again in the instructions, which say when to use which
    /// tool. A worker's report is described once, by `complete_task`'s
    /// summary.
    #[test]
    fn the_instructions_leave_what_the_tools_say_to_the_tools() {
        let opus = model("opus", false);
        let with = AgentSettings {
            index_tools: crate::index::TOOL_NAMES,
            ..settings(&opus, &opus)
        };
        for text_replies in [true, false] {
            let main = main_config(&AgentSettings {
                text_replies,
                ..with
            })
            .unwrap();
            let worker = worker_config(&with).unwrap();
            for config in [&main, &worker] {
                for told_by_a_tool in [
                    "`cwd`",
                    "job ID",
                    "`start_line`",
                    "changes nothing",
                    "waits for their answer",
                    "returns its definition whole",
                    "`detail: brief`",
                    "kept current",
                    "reads it at its next step",
                    "start several in one response",
                    "doesn't see this conversation",
                ] {
                    assert!(
                        !config.instructions.contains(told_by_a_tool),
                        "{told_by_a_tool}: {}",
                        config.instructions
                    );
                }
            }
            assert!(
                main.instructions
                    .contains("Use `wait_for_agents` only when")
            );
        }
        let worker = worker_config(&with).unwrap();
        let summary = worker.complete_task_summary_prompt_override.unwrap();
        assert!(summary.contains("the only thing it sees of your work"));
        assert!(summary.contains("file paths and line numbers"));
        assert!(!worker.instructions.contains("complete_task"));
        assert!(!worker.instructions.contains("message_agent"));
    }

    /// The extra rules go to agents on small models, each agent by its own
    /// model: a worker on Haiku under a lead on Opus gets them, and the
    /// lead doesn't. `All` gives them to every agent and `Off` to none.
    #[test]
    fn each_agent_gets_the_extra_rules_by_its_own_model() {
        let opus = model("cc:opus", false);
        let haiku = model("cc:claude-haiku-5-5", false);
        let luna = model("luna", false);
        let has = |config: AgentConfig| config.instructions.contains("# Extra rules");
        let with = |main, worker, extra_rules| AgentSettings {
            extra_rules,
            index_tools: crate::index::TOOL_NAMES,
            ..settings(main, worker)
        };
        let mixed = with(&opus, &haiku, ExtraRules::Small);
        assert!(!has(main_config(&mixed).unwrap()));
        assert!(has(worker_config(&mixed).unwrap()));
        let mixed = with(&luna, &opus, ExtraRules::Small);
        assert!(has(main_config(&mixed).unwrap()));
        assert!(!has(worker_config(&mixed).unwrap()));
        let all = with(&opus, &opus, ExtraRules::All);
        assert!(has(main_config(&all).unwrap()));
        assert!(has(worker_config(&all).unwrap()));
        let off = with(&luna, &haiku, ExtraRules::Off);
        assert!(!has(main_config(&off).unwrap()));
        assert!(!has(worker_config(&off).unwrap()));
    }

    /// Each rule is given where it applies: the one about a working
    /// directory no index covers only then, and none without the index
    /// tools, which leaves no section at all.
    #[test]
    fn the_extra_rules_say_only_what_applies() {
        let luna = model("luna", false);
        let without = AgentSettings {
            extra_rules: ExtraRules::Small,
            ..settings(&luna, &luna)
        };
        assert!(
            !main_config(&without)
                .unwrap()
                .instructions
                .contains("# Extra rules")
        );
        let uncovered = AgentSettings {
            index_tools: crate::index::TOOL_NAMES,
            ..without
        };
        let text = main_config(&uncovered).unwrap().instructions;
        assert!(text.contains(
            "# Extra rules\n- No index covers the working directory, so `index_search` can't search it."
        ));
        let indexes = [index::Known {
            name: "work".into(),
            kind: "code",
            root: "/work".into(),
            place: Some("the working directory"),
        }];
        let covered = AgentSettings {
            indexes: &indexes,
            ..uncovered
        };
        // Then no rule applies, and there is no section.
        let text = worker_config(&covered).unwrap().instructions;
        assert!(!text.contains("# Extra rules"), "{text}");
    }

    /// Haiku and luna are small on every route; the others aren't.
    #[test]
    fn small_models_are_haiku_and_luna_on_any_route() {
        for name in [
            "claude-haiku-5-5",
            "cc:claude-haiku-5-5",
            "openrouter:anthropic/claude-haiku-5.5",
            "luna",
            "gpt-6-luna",
            "openrouter:openai/gpt-6-luna:floor",
        ] {
            assert!(providers::is_small(&resolve_model(name).unwrap()), "{name}");
        }
        for name in [
            "opus",
            "cc:opus",
            "fable",
            "sonnet",
            "sol",
            "astra",
            "gemini",
            "openrouter:anthropic/claude-opus-5.5",
        ] {
            assert!(
                !providers::is_small(&resolve_model(name).unwrap()),
                "{name}"
            );
        }
    }

    /// With the default, the large models' configurations (and so what
    /// they are sent) are the same as without the rules.
    #[test]
    fn the_default_leaves_large_models_as_they_were() {
        for name in [
            "opus", "cc:opus", "cc:fable", "sonnet", "sol", "astra", "gemini",
        ] {
            let big = model(name, false);
            let configs = |extra_rules| {
                let with = AgentSettings {
                    extra_rules,
                    index_tools: crate::index::TOOL_NAMES,
                    ..settings(&big, &big)
                };
                (
                    serde_json::to_value(main_config(&with).unwrap()).unwrap(),
                    serde_json::to_value(worker_config(&with).unwrap()).unwrap(),
                )
            };
            assert_eq!(
                configs(ExtraRules::Small),
                configs(ExtraRules::Off),
                "{name}"
            );
        }
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
        assert_eq!(default_handover(&opus, None), 500_000);
        assert_eq!(default_handover(&opus, Some(OpenAiAuth::ChatGpt)), 500_000);
        let sol = resolve_model("sol").unwrap();
        // The ChatGPT backend's 272k, less the room: not the API's limit,
        // which an API key has.
        assert!(sol.max_input_tokens > 272_000);
        assert_eq!(default_handover(&sol, Some(OpenAiAuth::ChatGpt)), 250_000);
        assert_eq!(default_handover(&sol, Some(OpenAiAuth::ApiKey)), 200_000);
        // Never past 80% of what the model accepts.
        let mut small = opus.clone();
        small.max_input_tokens = 200_000;
        assert_eq!(default_handover(&small, None), 160_000);
        let mut small = sol.clone();
        small.max_input_tokens = 128_000;
        assert_eq!(default_handover(&small, Some(OpenAiAuth::ChatGpt)), 106_000);

        let mut main = model("opus", false);
        main.handover_at = Some(default_handover(&opus, None));
        let worker = model("sol", false);
        let settings = settings(&main, &worker);
        let handover = |config: &AgentConfig| config.handover.clone().unwrap().at_input_tokens;
        assert_eq!(handover(&main_config(&settings).unwrap()), Some(500_000));
        // A session saved before it was set per model uses its own size.
        assert_eq!(handover(&worker_config(&settings).unwrap()), Some(200_000));
    }

    /// With text replies, the lead ends its turns with a reply in text: it
    /// has neither `complete_task` nor `message_user`, nor the summary's
    /// description, and its instructions say how its turn ends and how it
    /// answers mid-turn. The workers keep `complete_task`.
    #[test]
    fn with_text_replies_the_lead_ends_its_turns_with_a_reply() {
        let opus = model("opus", false);
        let replying = AgentSettings {
            text_replies: true,
            ..settings(&opus, &opus)
        };
        let config = main_config(&replying).unwrap();
        assert!(config.tool_config.text_reply_ends_task);
        assert!(config.tool_config.ends_task_with_text());
        let offered = config.tool_config.offered_tool_names();
        for absent in [
            arko_agents::builtins::COMPLETE_TASK_TOOL_NAME,
            crate::message::MESSAGE_USER,
        ] {
            assert!(!offered.contains(absent), "{absent}");
        }
        assert!(offered.contains(ASK_USER_TOOL_NAME));
        assert!(config.complete_task_summary_prompt_override.is_none());
        let instructions = &config.instructions;
        assert!(instructions.contains("When you're done, reply without calling a tool."));
        assert!(instructions.contains("answer it in text beside your next tool call"));
        assert!(instructions.contains("Only a response without a tool call ends your turn"));
        for gone in [
            "message_user",
            "complete_task",
            "Every response of yours must call a tool",
        ] {
            assert!(!instructions.contains(gone), "{gone}");
        }
        let worker = worker_config(&replying).unwrap();
        assert!(!worker.tool_config.ends_task_with_text());
        assert!(
            worker
                .tool_config
                .offered_tool_names()
                .contains(arko_agents::builtins::COMPLETE_TASK_TOOL_NAME)
        );
        assert!(worker.complete_task_summary_prompt_override.is_some());
        // Without them, the lead is as it was.
        let calling = main_config(&settings(&opus, &opus)).unwrap();
        assert!(!calling.tool_config.ends_task_with_text());
        assert!(calling.complete_task_summary_prompt_override.is_some());
        assert!(
            calling
                .instructions
                .contains("Every response of yours must call a tool")
        );
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

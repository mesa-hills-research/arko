//! The providers Arko runs its agents on, and how models are named.
//!
//! A model ID from arko's registry (`claude-opus-5-5`, `gpt-6.1-sol`,
//! `gemini-3.1-pro-preview`) or an alias (`opus`, `sol`, `gemini`) names a
//! model at its own provider: Claude models with an Anthropic API key,
//! OpenAI models with a ChatGPT login or an OpenAI API key, Gemini models
//! with a Google Gemini API key. Two prefixes name the other routes:
//! `openrouter:<slug>` any model OpenRouter serves
//! (`openrouter:anthropic/claude-opus-5.5`), on an OpenRouter key, and
//! `cc:<model>` a Claude model run in the user's Claude Code CLI, on their
//! Claude plan (`cc:opus`, `cc:fable`).
//!
//! Plain names stay as they were: a registry ID names one model at one
//! provider (IDs are unique across the registry). A prefix appears only
//! where the same model can be reached another way, and it says which
//! account pays. A session saves the name in this form
//! (`ModelSettings::model`), so it reads the same in `session.json`, in
//! `arko --sessions` and on the command line.
//!
//! OpenAI models are the exception: arko's client holds one OpenAI client,
//! so a session reaches them all one way, its [`OpenAiAuth`], saved with
//! the session.

use std::path::{Path, PathBuf};

use arko_ai::model::ModelType;
use arko_ai::{
    BillingKind, CodexAuthFile, GeminiThinkingLevel, GenerationConfig, LLMClient, Model,
    OpenAIServiceTier, OpenRouterCatalog, OpenRouterReasoning, PlanUsage, Provider,
    ReasoningConfig, ReasoningEffort, ReasoningSummary, ServiceTier, SubscriptionPlan, UsageInfo,
};
use arko_workflows::budget::{CostLedger, CostSource};
use serde::{Deserialize, Serialize};

use crate::agents::{Effort, ModelSettings, default_handover};
use crate::claude_code;
use crate::credentials::{self, Need};
use crate::logins;
use crate::session::Settings;

/// The prefix of a model on OpenRouter: `openrouter:<slug>`.
pub const OPENROUTER: &str = "openrouter:";
/// The prefix of a Claude model run in Claude Code: `cc:<model>`.
pub const CLAUDE_CODE: &str = "cc:";
/// The long form of [`CLAUDE_CODE`], also accepted.
const CLAUDE_CODE_LONG: &str = "claude-code:";

/// Short names for the models people use most.
pub const ALIASES: [(&str, &str); 11] = [
    ("opus", "claude-opus-5-5"),
    ("fable", "claude-fable-5-1"),
    ("sonnet", "claude-sonnet-5-5"),
    ("haiku", "claude-haiku-5-5"),
    ("mythos", "claude-mythos-5-1"),
    ("sol", "gpt-6.1-sol"),
    ("astra", "gpt-6-astra"),
    ("terra", "gpt-6.1-terra"),
    ("luna", "gpt-6-luna"),
    ("gemini", "gemini-3.1-pro-preview"),
    ("flash", "gemini-3.8-flash"),
];

/// Registry models and the exact slug each has on OpenRouter, one row for
/// each model OpenRouter also serves, written by hand from OpenRouter's
/// model list (checked on 2026-10-29). A slug names its model only by
/// equality, after one of [`OPENROUTER_ROUTING_SUFFIXES`] comes off (see
/// [`openrouter_equivalent`]), so a "-v2" variant, a fine-tune or another
/// publisher's model is none of these. Nothing fills it from OpenRouter's
/// list, since that would need the guessing the table is there to avoid.
///
/// An alias moved or added in [`ALIASES`] needs a row here, or its model in
/// [`NOT_ON_OPENROUTER`], and a test fails until it has one. A live test
/// (ignored by default) fails for a slug OpenRouter no longer lists.
pub const OPENROUTER_EQUIVALENTS: &[(&str, &str)] = &[
    // Claude
    ("claude-opus-5-5", "anthropic/claude-opus-5.5"),
    ("claude-opus-5", "anthropic/claude-opus-5"),
    ("claude-opus-4-8", "anthropic/claude-opus-4.8"),
    ("claude-opus-4-7", "anthropic/claude-opus-4.7"),
    ("claude-opus-4-6", "anthropic/claude-opus-4.6"),
    ("claude-opus-4-5-20251101", "anthropic/claude-opus-4.5"),
    ("claude-opus-4-1-20250805", "anthropic/claude-opus-4.1"),
    ("claude-fable-5-1", "anthropic/claude-fable-5.1"),
    ("claude-fable-5", "anthropic/claude-fable-5"),
    ("claude-sonnet-5-5", "anthropic/claude-sonnet-5.5"),
    ("claude-sonnet-5", "anthropic/claude-sonnet-5"),
    ("claude-sonnet-4-6", "anthropic/claude-sonnet-4.6"),
    ("claude-sonnet-4-5-20250929", "anthropic/claude-sonnet-4.5"),
    ("claude-haiku-5-5", "anthropic/claude-haiku-5.5"),
    ("claude-haiku-4-5-20251001", "anthropic/claude-haiku-4.5"),
    // OpenAI
    ("gpt-6.1-sol", "openai/gpt-6.1-sol"),
    ("gpt-6.1-terra", "openai/gpt-6.1-terra"),
    ("gpt-6-sol", "openai/gpt-6-sol"),
    ("gpt-6-astra", "openai/gpt-6-astra"),
    ("gpt-6-luna", "openai/gpt-6-luna"),
    ("gpt-5.6-sol", "openai/gpt-5.6-sol"),
    ("gpt-5.6-terra", "openai/gpt-5.6-terra"),
    ("gpt-5.6-luna", "openai/gpt-5.6-luna"),
    ("gpt-5.5-2026-04-23", "openai/gpt-5.5"),
    ("gpt-5.5-pro-2026-04-23", "openai/gpt-5.5-pro"),
    ("gpt-5.4-2026-03-05", "openai/gpt-5.4"),
    ("gpt-5.4-pro-2026-03-05", "openai/gpt-5.4-pro"),
    ("gpt-5.4-mini-2026-03-17", "openai/gpt-5.4-mini"),
    ("gpt-5.4-nano-2026-03-17", "openai/gpt-5.4-nano"),
    ("gpt-5.3-codex", "openai/gpt-5.3-codex"),
    ("gpt-5.2", "openai/gpt-5.2"),
    ("gpt-5.2-pro", "openai/gpt-5.2-pro"),
    ("gpt-5.1-2025-11-13", "openai/gpt-5.1"),
    ("gpt-5-2025-08-07", "openai/gpt-5"),
    ("gpt-5-pro-2025-10-06", "openai/gpt-5-pro"),
    ("gpt-5-mini-2025-08-07", "openai/gpt-5-mini"),
    ("gpt-5-nano-2025-08-07", "openai/gpt-5-nano"),
    ("gpt-4.1-2025-04-14", "openai/gpt-4.1"),
    ("gpt-4.1-mini-2025-04-14", "openai/gpt-4.1-mini"),
    ("gpt-4.1-nano-2025-04-14", "openai/gpt-4.1-nano"),
    ("o3-2025-04-16", "openai/o3"),
    ("o3-pro-2025-06-10", "openai/o3-pro"),
    // Gemini
    ("gemini-3.1-pro-preview", "google/gemini-3.1-pro-preview"),
    (
        "gemini-3.1-pro-preview-customtools",
        "google/gemini-3.1-pro-preview-customtools",
    ),
    ("gemini-3.8-flash", "google/gemini-3.8-flash"),
    ("gemini-3.7-flash", "google/gemini-3.7-flash"),
    ("gemini-3.6-flash", "google/gemini-3.6-flash"),
    ("gemini-3.5-flash", "google/gemini-3.5-flash"),
    ("gemini-3.5-flash-lite", "google/gemini-3.5-flash-lite"),
    ("gemini-3.1-flash-lite", "google/gemini-3.1-flash-lite"),
    (
        "gemini-3.1-flash-lite-preview",
        "google/gemini-3.1-flash-lite-preview",
    ),
    ("gemini-3-flash-preview", "google/gemini-3-flash-preview"),
    ("gemini-2.5-pro", "google/gemini-2.5-pro"),
    ("gemini-2.5-flash", "google/gemini-2.5-flash"),
    ("gemini-2.5-flash-lite", "google/gemini-2.5-flash-lite"),
];

/// The models [`ALIASES`] names that OpenRouter doesn't serve, so that every
/// alias has a row in [`OPENROUTER_EQUIVALENTS`] or a place here.
#[cfg_attr(not(test), allow(dead_code))]
pub const NOT_ON_OPENROUTER: [&str; 1] = ["claude-mythos-5-1"];

/// The registry ID of the model `slug` (without the `openrouter:` prefix)
/// names on OpenRouter, by [`OPENROUTER_EQUIVALENTS`]. The slug must equal a
/// row's exactly, after at most one of [`OPENROUTER_ROUTING_SUFFIXES`] comes
/// off, since those pick a provider or a free tier for the same model.
pub fn openrouter_equivalent(slug: &str) -> Option<&'static str> {
    let slug = OPENROUTER_ROUTING_SUFFIXES
        .iter()
        .find_map(|suffix| slug.strip_suffix(suffix))
        .unwrap_or(slug);
    OPENROUTER_EQUIVALENTS
        .iter()
        .find(|(_, equivalent)| *equivalent == slug)
        .map(|(id, _)| *id)
}

/// The suffixes of an OpenRouter slug that pick a provider or a free tier
/// for the same model, `:floor` (the cheapest provider), `:nitro` (the
/// fastest) and `:free`. A slug with one of them, and only one, is looked up
/// in [`OPENROUTER_EQUIVALENTS`] without it. Other suffixes stay part of the
/// slug.
pub const OPENROUTER_ROUTING_SUFFIXES: [&str; 3] = [":floor", ":nitro", ":free"];

/// The handover size of a model whose input limit Arko can't tell (an
/// OpenRouter model when its catalog can't be read): 80% of 128k, the
/// smallest context window common among the models OpenRouter serves.
pub const UNKNOWN_LIMIT_HANDOVER: usize = 100_000;

/// The model `name` names: a registry ID or alias, `openrouter:<slug>`, or
/// `cc:<model>`.
pub fn resolve_model(name: &str) -> Result<Model, String> {
    let name = name.trim();
    if let Some(slug) = strip_prefix(name, OPENROUTER) {
        if slug.is_empty() || slug.chars().any(char::is_whitespace) {
            return Err(format!(
                "`{name}` names no OpenRouter model. Give its slug, such as openrouter:anthropic/claude-opus-5.5"
            ));
        }
        return Ok(Model::openrouter(slug));
    }
    if let Some(rest) =
        strip_prefix(name, CLAUDE_CODE).or_else(|| strip_prefix(name, CLAUDE_CODE_LONG))
    {
        let model = registered(rest).map_err(|_| {
            format!(
                "`{name}` names no Claude model. Claude Code runs Claude models, such as cc:opus (claude-opus-5-5) or cc:fable (claude-fable-5-1){}",
                suggestion(rest).map(|close| format!(". Did you mean cc:{close}?")).unwrap_or_default()
            )
        })?;
        if model.provider != Provider::Anthropic {
            return Err(format!(
                "`{name}`: Claude Code runs Claude models, and {} isn't one ({} model)",
                model.id,
                provider_name(&model)
            ));
        }
        return Ok(Model::claude_code(model));
    }
    registered(name)
}

/// A registered model by ID or alias.
fn registered(name: &str) -> Result<Model, String> {
    let id = ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map_or(name, |(_, id)| id);
    Model::find(id).ok_or_else(|| unknown(name))
}

/// Where the error for an unknown model goes on from what is wrong to the
/// aliases and prefixes that would be right.
const UNKNOWN_HELP: &str = " Use a model ID, an alias (";

/// `error` from [`resolve_model`] without the list of aliases, for a line
/// or a note that has no room for them. It keeps what is wrong with the name
/// and what it may have meant.
pub fn briefly(error: &str) -> &str {
    error.split(UNKNOWN_HELP).next().unwrap_or(error)
}

/// The error for a name that names no model, with what it may have meant.
fn unknown(name: &str) -> String {
    let mut text = format!("`{name}` isn't a model Arko knows.");
    if name.contains('/') {
        text.push_str(&format!(
            " Models on OpenRouter take its prefix, as in openrouter:{name}."
        ));
    } else if let Some(close) = suggestion(name) {
        text.push_str(&format!(" Did you mean {close}?"));
    }
    text.push_str(&format!(
        "{UNKNOWN_HELP}{}), openrouter:<slug> for a model on OpenRouter, or cc:<model> for a Claude model in Claude Code (cc:opus).",
        ALIASES
            .iter()
            .map(|(alias, id)| format!("{alias} = {id}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    text
}

/// The alias or registry ID closest to `name`, when it is close enough to be
/// a typo of it.
fn suggestion(name: &str) -> Option<String> {
    let name = name.to_ascii_lowercase();
    ALIASES
        .iter()
        .map(|(alias, _)| alias.to_string())
        .chain(Model::all().into_iter().map(|model| model.id))
        .map(|candidate| (distance(&name, &candidate), candidate))
        .filter(|(distance, candidate)| *distance <= (candidate.len() / 3).max(2))
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// The edit distance between two strings.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substituted = previous + usize::from(ca != *cb);
            previous = row[j + 1];
            row[j + 1] = substituted.min(row[j] + 1).min(previous + 1);
        }
    }
    row[b.len()]
}

pub(crate) fn strip_prefix<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    let head = name.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| name[prefix.len()..].trim())
}

/// How a session saves `model`: its registry ID, or with the prefix of its
/// route.
pub fn model_name(model: &Model) -> String {
    match model.provider {
        Provider::OpenRouter => format!("{OPENROUTER}{}", model.id),
        Provider::ClaudeCode => format!("{CLAUDE_CODE}{}", model.id),
        _ => model.id.clone(),
    }
}

/// `name` (a model as [`model_name`] writes it) in as few characters as
/// still name it. That is its alias (`sol`, `cc:opus`), or else a long
/// OpenRouter slug cut short.
pub fn short_model_name(name: &str) -> String {
    let (prefix, id) = match name.strip_prefix(CLAUDE_CODE) {
        Some(id) => (CLAUDE_CODE, id),
        None => ("", name),
    };
    if let Some((alias, _)) = ALIASES.iter().find(|(_, registry)| *registry == id) {
        return format!("{prefix}{alias}");
    }
    if name.chars().count() > 28 {
        return format!("{}…", name.chars().take(27).collect::<String>());
    }
    name.to_string()
}

/// The provider's name, as Arko's messages say it.
pub fn provider_name(model: &Model) -> &'static str {
    match model.provider {
        Provider::Anthropic => "Claude",
        Provider::OpenAI => "OpenAI",
        Provider::Google => "Gemini",
        Provider::OpenRouter => "OpenRouter",
        Provider::ClaudeCode => "Claude Code",
    }
}

/// A Claude model on OpenRouter: `anthropic/…`, or a `~anthropic/…-latest`
/// alias.
pub fn is_claude_slug(slug: &str) -> bool {
    slug.trim_start_matches('~').starts_with("anthropic/")
}

/// Whether `model` has a longer prompt cache to choose (the session's long
/// cache, and a worker's `cache`). Claude models on an API key, on
/// OpenRouter or in Claude Code keep it an hour instead of 5 minutes, and
/// OpenAI GPT-5.6 and later on an API key 12 hours instead of 30 minutes.
/// Claude Code chooses its own when none is asked for, an hour on a Claude
/// plan. The ChatGPT login keeps its own 30-minute cache.
pub fn takes_long_cache(model: &Model, openai_auth: Option<OpenAiAuth>) -> bool {
    match model.provider {
        Provider::Anthropic | Provider::ClaudeCode => true,
        Provider::OpenRouter => is_claude_slug(&model.id),
        Provider::OpenAI => {
            openai_auth == Some(OpenAiAuth::ApiKey) && model.supports_openai_prompt_cache_options()
        }
        Provider::Google => false,
    }
}

/// The shorter and the longer prompt cache of a model that takes a choice
/// ([`takes_long_cache`]), as Arko's words name them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheChoice {
    /// "5m" or "30m".
    pub short: &'static str,
    /// "1h" or "12h".
    pub long: &'static str,
    /// "5 minutes" or "30 minutes".
    pub short_words: &'static str,
    /// "an hour" or "12 hours".
    pub long_words: &'static str,
    /// What a long cache's write costs, "twice" or "2.5 times" the input
    /// price (a short one's is 1.25 times on both).
    pub long_write: &'static str,
}

impl CacheChoice {
    /// `model`'s caches, OpenAI's for OpenAI models and Claude's for the
    /// others.
    pub fn of(model: &Model) -> Self {
        if model.provider == Provider::OpenAI {
            CacheChoice {
                short: "30m",
                long: "12h",
                short_words: "30 minutes",
                long_words: "12 hours",
                long_write: "2.5 times",
            }
        } else {
            CacheChoice {
                short: "5m",
                long: "1h",
                short_words: "5 minutes",
                long_words: "an hour",
                long_write: "twice",
            }
        }
    }
}

/// Asks for the shorter prompt cache outright where leaving it out gives a
/// longer one, which is in Claude Code (it otherwise chooses its own). The
/// other models have it when none is asked for.
pub fn ask_short_cache(model: &Model, generation: &mut GenerationConfig) {
    if model.provider == Provider::ClaudeCode {
        generation.long_prompt_cache = Some(false);
    }
}

/// The small models, by registry ID: fast, cheap models that do better with
/// plain, explicit rules than the large ones need (`agents::extra_rules`).
const SMALL_MODELS: [&str; 4] = [
    "claude-haiku-5-5",
    "claude-haiku-4-5-20251001",
    "gpt-6-luna",
    "gpt-5.6-luna",
];

/// Whether `model` is one of [`SMALL_MODELS`], on any route: its registry ID
/// through the API or `cc:`, or its slug in [`OPENROUTER_EQUIVALENTS`]
/// through OpenRouter, with or without one of
/// [`OPENROUTER_ROUTING_SUFFIXES`]. The ID or slug must match exactly.
pub fn is_small(model: &Model) -> bool {
    let id = match model.provider {
        Provider::OpenRouter => openrouter_equivalent(&model.id),
        _ => Some(model.id.as_str()),
    };
    id.is_some_and(|id| SMALL_MODELS.contains(&id))
}

/// What the session says, once, when OpenAI refuses the reasoning summaries
/// Arko asks for (the organization isn't verified for them): the framework
/// sends the request again without them, and no request asks again.
pub const THINKING_REFUSED: &str = "OpenAI gives reasoning summaries only to verified organizations, and this one isn't verified, so the agents' thinking isn't shown and Arko stops asking for it. Verifying the organization in OpenAI's settings turns it on for new sessions.";

/// The generation settings of an agent on `model`: its reasoning at
/// `effort`, fast mode where the provider has it, and the long prompt cache
/// with `long_cache` (which the caller asks for where [`takes_long_cache`]).
/// With `thinking_text`, the model is asked for the text of its thinking
/// where it needs asking, for the views to show: Claude by API key for its
/// summarized thinking (Claude Code passes it on by itself), OpenAI and
/// OpenRouter for reasoning summaries, Gemini for its thoughts. An OpenAI
/// organization that isn't verified for summaries refuses them, and the
/// framework then asks without them ([`THINKING_REFUSED`]).
pub fn generation(
    model: &Model,
    effort: Effort,
    fast: bool,
    long_cache: bool,
    thinking_text: bool,
) -> Result<GenerationConfig, String> {
    let mut generation = GenerationConfig::default().with_prompt_caching(true);
    let reasons = model.model_type == ModelType::Reasoning;
    match model.provider {
        Provider::Anthropic | Provider::ClaudeCode => {
            if fast && !model.supports_anthropic_fast_mode() {
                return Err(format!(
                    "{} has no fast mode. Anthropic offers it on Claude Opus 5.5, Opus 5 and Opus 4.8",
                    model_name(model)
                ));
            }
            // In Claude Code, the effort is the CLI's --effort.
            generation = generation.with_reasoning(
                ReasoningConfig::anthropic_adaptive_with_effort(effort.into()),
            );
            if fast {
                generation = generation.with_anthropic_fast_mode(true);
            }
            // The newer models leave their thinking's text out unless asked.
            if thinking_text && model.provider == Provider::Anthropic {
                generation = generation.with_anthropic_summarized_thinking(true);
            }
        }
        Provider::OpenAI => {
            if reasons {
                generation = generation
                    .with_reasoning(ReasoningConfig::openai(effort.into(), thinking_text));
            }
            if fast {
                generation = generation.with_openai_service_tier(OpenAIServiceTier::Fast);
            }
        }
        Provider::Google => {
            if fast {
                return Err(format!(
                    "{} has no fast mode (Gemini models have none)",
                    model.id
                ));
            }
            if reasons {
                generation = generation.with_reasoning(ReasoningConfig::gemini(
                    gemini_level(model, effort),
                    thinking_text,
                ));
            }
        }
        Provider::OpenRouter => {
            if fast {
                return Err(format!(
                    "{} has no fast mode on OpenRouter (a :nitro slug, such as {}:nitro, prefers its fastest providers)",
                    model_name(model),
                    model_name(model)
                ));
            }
            // OpenRouter maps the effort to the model's own levels, and
            // ignores it for models that don't reason.
            let mut reasoning = OpenRouterReasoning::effort(reasoning_effort(effort));
            if thinking_text {
                reasoning = reasoning.with_summary(ReasoningSummary::Auto);
            }
            generation = generation.with_reasoning(ReasoningConfig::openrouter(reasoning));
        }
    }
    if long_cache {
        generation = generation.with_long_prompt_cache(true);
    }
    Ok(generation)
}

/// The model settings an agent's configuration `config` runs, as `/model`
/// names them: its model, the effort its reasoning was set up with (the
/// first `generation` gives the same reasoning for, else `fallback`), its
/// fast mode, long cache and handover size.
pub fn settings_of(config: &arko_agents::AgentConfig, fallback: Effort) -> ModelSettings {
    let model = &config.llm_config.model;
    let generation = &config.llm_config.generation_config;
    let have = serde_json::to_value(&generation.reasoning).ok();
    let effort = efforts(model)
        .iter()
        .copied()
        .find(|effort| {
            [true, false].iter().any(|thinking_text| {
                generation_of(model, *effort, *thinking_text).is_some_and(|candidate| {
                    serde_json::to_value(&candidate.reasoning).ok() == have
                })
            })
        })
        .unwrap_or_else(|| fit_effort(model, fallback));
    ModelSettings {
        model: model_name(model),
        effort,
        fast: generation.anthropic_fast_mode == Some(true)
            || generation.openai_service_tier == Some(OpenAIServiceTier::Fast),
        cache_1h: generation.long_prompt_cache == Some(true),
        handover_at: config
            .handover
            .as_ref()
            .and_then(|handover| handover.at_input_tokens),
    }
}

/// [`generation`] at `effort`, without fast mode or a long cache.
fn generation_of(model: &Model, effort: Effort, thinking_text: bool) -> Option<GenerationConfig> {
    generation(model, effort, false, false, thinking_text).ok()
}

/// Gemini thinks at low, medium or high: higher efforts are high, and
/// Gemini 3 Pro, which has no medium, thinks high at medium.
fn gemini_level(model: &Model, effort: Effort) -> GeminiThinkingLevel {
    match effort {
        Effort::Low => GeminiThinkingLevel::Low,
        Effort::Medium if !model.is_gemini_3_pro() => GeminiThinkingLevel::Medium,
        _ => GeminiThinkingLevel::High,
    }
}

fn reasoning_effort(effort: Effort) -> ReasoningEffort {
    match effort {
        Effort::Low => ReasoningEffort::Low,
        Effort::Medium => ReasoningEffort::Medium,
        Effort::High => ReasoningEffort::High,
        Effort::Xhigh => ReasoningEffort::XHigh,
        Effort::Max => ReasoningEffort::Max,
    }
}

/// How a session reaches OpenAI models. It's one way for all of them,
/// since Arko's model client holds one OpenAI client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "tui", derive(clap::ValueEnum))]
pub enum OpenAiAuth {
    /// The ChatGPT login (a Codex auth.json), whose usage counts against
    /// the ChatGPT plan.
    #[serde(rename = "chatgpt")]
    #[cfg_attr(feature = "tui", value(name = "chatgpt"))]
    ChatGpt,
    /// An OpenAI API key, billed per token.
    #[serde(rename = "api-key")]
    #[cfg_attr(feature = "tui", value(name = "api-key"))]
    ApiKey,
}

impl std::str::FromStr for OpenAiAuth {
    type Err = ();

    /// Its name as the flags take it, in any case.
    fn from_str(text: &str) -> Result<Self, ()> {
        match text.to_ascii_lowercase().as_str() {
            "chatgpt" => Ok(OpenAiAuth::ChatGpt),
            "api-key" => Ok(OpenAiAuth::ApiKey),
            _ => Err(()),
        }
    }
}

impl OpenAiAuth {
    /// How the session reaches OpenAI models, as the banner says it.
    pub fn describe(self) -> &'static str {
        match self {
            OpenAiAuth::ChatGpt => "ChatGPT login",
            OpenAiAuth::ApiKey => "OpenAI API key",
        }
    }
}

/// Whether there is a ChatGPT login: a Codex auth.json (`--codex-auth`,
/// ARKO_CODEX_AUTH or Codex's own) or the OPENAI_OAUTH_* variables.
pub fn has_chatgpt_login(codex_auth: Option<&Path>) -> bool {
    credentials::codex_auth(codex_auth).is_some()
        || credentials::var("OPENAI_OAUTH_ACCESS_TOKEN").is_some()
        || CodexAuthFile::default_path().is_some_and(|path| path.is_file())
}

/// How a new session reaches OpenAI models: `flag`, else ARKO_OPENAI_AUTH,
/// else the ChatGPT login when there is one, else an OpenAI API key when
/// there is one, else the ChatGPT login (which the setup screen asks for).
/// The second value says whether both were there, so the choice is worth
/// saying.
pub fn choose_openai_auth(
    flag: Option<OpenAiAuth>,
    codex_auth: Option<&Path>,
) -> Result<(OpenAiAuth, bool), String> {
    if let Some(auth) = flag {
        return Ok((auth, false));
    }
    if let Some(value) = credentials::var(credentials::OPENAI_AUTH) {
        return value
            .trim()
            .parse::<OpenAiAuth>()
            .map(|auth| (auth, false))
            .map_err(|_| format!("ARKO_OPENAI_AUTH is `{value}`. Set it to chatgpt or api-key"));
    }
    let login = has_chatgpt_login(codex_auth);
    let key = credentials::var(credentials::OPENAI_KEY).is_some();
    Ok(match (login, key) {
        (false, true) => (OpenAiAuth::ApiKey, false),
        (login, key) => (OpenAiAuth::ChatGpt, login && key),
    })
}

/// The models a new session takes without `--model`, and what to tell the
/// user about where they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultModels {
    pub main: String,
    /// The workers' model, when it differs from the main one.
    pub worker: Option<String>,
    /// Why the session doesn't run the last session's models as they were:
    /// a credential one needs is gone.
    pub reason: Option<String>,
}

/// The models of a new session without `--model`: those of the last
/// session started (its main model, and its workers' when they differed)
/// while their credentials are still there, else the first model the
/// credentials allow ([`model_by_credentials`]). When the last model's
/// credential is gone and there is no other, it is still the default, so
/// the setup screen asks for exactly that credential. `None` without
/// either.
pub fn default_models(codex_auth: Option<&Path>, root: &Path) -> Option<DefaultModels> {
    choose_default(
        LastModels::load(root),
        |name| missing_credential(name, codex_auth),
        model_by_credentials(codex_auth),
    )
}

/// [`default_models`], from the last session's models, what a model needs
/// that isn't there, and the model the credentials allow.
fn choose_default(
    last: Option<LastModels>,
    missing: impl Fn(&str) -> Option<String>,
    by_credentials: Option<(&'static str, &'static str)>,
) -> Option<DefaultModels> {
    let Some(last) = last else {
        let (main, _) = by_credentials?;
        return Some(DefaultModels {
            main: main.to_string(),
            worker: None,
            reason: None,
        });
    };
    match missing(&last.main) {
        None => {
            let worker = last.worker.filter(|worker| worker != &last.main);
            let (worker, reason) = match worker {
                Some(worker) => match missing(&worker) {
                    None => (Some(worker), None),
                    Some(needs) => (
                        None,
                        Some(format!(
                            "Your last session's workers ran {worker}, which needs {needs}, so they run {} now.",
                            last.main
                        )),
                    ),
                },
                None => (None, None),
            };
            Some(DefaultModels {
                main: last.main,
                worker,
                reason,
            })
        }
        Some(needs) => match by_credentials {
            Some((main, _)) => Some(DefaultModels {
                reason: Some(format!(
                    "Your last session ran {}, which needs {needs}.",
                    last.main
                )),
                main: main.to_string(),
                worker: None,
            }),
            // The setup screen asks for exactly what it needs.
            None => Some(DefaultModels {
                main: last.main,
                worker: None,
                reason: None,
            }),
        },
    }
}

/// The first model the credentials there are allow, and why: Claude Opus
/// 5.5 in Claude Code when `claude` is installed and signed in (the Claude
/// plan comes before a key billed per token), else Opus with an Anthropic
/// API key, else GPT-6.1 Sol with a ChatGPT login, else Opus on OpenRouter,
/// Sol on an OpenAI API key, or Gemini 3.1 Pro on a Gemini key, else Opus
/// in a Claude Code that isn't signed in yet (the setup screen helps sign
/// it in); `None` without any.
pub fn model_by_credentials(codex_auth: Option<&Path>) -> Option<(&'static str, &'static str)> {
    first_by_credentials(Credentials {
        claude_code: claude_code::find_cli().map(|_| claude_code::check_recent().is_ok()),
        anthropic: credentials::var(credentials::ANTHROPIC_KEY).is_some(),
        chatgpt: has_chatgpt_login(codex_auth),
        openrouter: credentials::var(credentials::OPENROUTER_KEY).is_some(),
        openai: credentials::var(credentials::OPENAI_KEY).is_some(),
        gemini: gemini_key().is_some(),
    })
}

/// Which credentials there are, for [`first_by_credentials`].
#[derive(Debug, Clone, Copy, Default)]
struct Credentials {
    /// Whether Claude Code is signed in, when it is installed.
    claude_code: Option<bool>,
    anthropic: bool,
    chatgpt: bool,
    openrouter: bool,
    openai: bool,
    gemini: bool,
}

/// [`model_by_credentials`]'s order.
fn first_by_credentials(have: Credentials) -> Option<(&'static str, &'static str)> {
    let opus_on_plan = "cc:claude-opus-5-5";
    if have.claude_code == Some(true) {
        Some((
            opus_on_plan,
            "Claude Code is signed in, on your Claude plan",
        ))
    } else if have.anthropic {
        Some(("claude-opus-5-5", "ANTHROPIC_API_KEY is set"))
    } else if have.chatgpt {
        Some(("gpt-6.1-sol", "there is a ChatGPT login"))
    } else if have.openrouter {
        Some((
            "openrouter:anthropic/claude-opus-5.5",
            "OPENROUTER_API_KEY is set",
        ))
    } else if have.openai {
        Some(("gpt-6.1-sol", "OPENAI_API_KEY is set"))
    } else if have.gemini {
        Some(("gemini-3.1-pro-preview", "GEMINI_API_KEY is set"))
    } else if have.claude_code.is_some() {
        Some((opus_on_plan, "Claude Code is installed"))
    } else {
        None
    }
}

/// What a provider's models need, as Arko's notes say it.
pub fn credential_name(provider: &Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "ANTHROPIC_API_KEY",
        Provider::OpenAI => "a ChatGPT login or OPENAI_API_KEY",
        Provider::Google => "GEMINI_API_KEY",
        Provider::OpenRouter => "OPENROUTER_API_KEY",
        Provider::ClaudeCode => "Claude Code signed in",
    }
}

/// A provider's name in `remote::Models::reachable`.
pub fn provider_key(provider: &Provider) -> &'static str {
    provider.as_telemetry_str()
}

/// What the model `name` needs that isn't there, as Arko's notes say it;
/// `None` when its credential is there.
pub fn missing_credential(name: &str, codex_auth: Option<&Path>) -> Option<String> {
    let Ok(model) = resolve_model(name) else {
        return Some("a model Arko knows (it no longer knows this one)".to_string());
    };
    let missing = match model.provider {
        Provider::Anthropic => credentials::var(credentials::ANTHROPIC_KEY).is_none(),
        Provider::OpenAI => {
            !has_chatgpt_login(codex_auth) && credentials::var(credentials::OPENAI_KEY).is_none()
        }
        Provider::Google => gemini_key().is_none(),
        Provider::OpenRouter => credentials::var(credentials::OPENROUTER_KEY).is_none(),
        Provider::ClaudeCode => {
            return match claude_code::find_cli().map(|_| claude_code::check_recent()) {
                Some(Ok(_)) => None,
                Some(Err(claude_code::Problem::Outdated { .. })) => {
                    Some("Claude Code updated".to_string())
                }
                Some(Err(_)) => Some("Claude Code signed in".to_string()),
                None => Some("Claude Code installed".to_string()),
            };
        }
    };
    missing.then(|| {
        match model.provider {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::OpenAI => "a ChatGPT login or OPENAI_API_KEY",
            Provider::Google => "GEMINI_API_KEY",
            _ => "OPENROUTER_API_KEY",
        }
        .to_string()
    })
}

/// How a session reaches OpenAI models when none of its models is one,
/// read through `var`: the ChatGPT login when there is one (unless
/// ARKO_OPENAI_AUTH says api-key and there is a key), else the API key;
/// `None` without either.
fn available_openai_auth(
    var: &impl Fn(&str) -> Option<String>,
    auth_files: &[PathBuf],
) -> Option<OpenAiAuth> {
    let login = (var("OPENAI_OAUTH_ACCESS_TOKEN").is_some()
        && var("OPENAI_OAUTH_ACCOUNT_ID").is_some())
        || auth_files.iter().any(|path| path.is_file())
        || (auth_files.is_empty()
            && CodexAuthFile::default_path().is_some_and(|path| path.is_file()));
    let key = var(credentials::OPENAI_KEY).is_some();
    let wants_key = var(credentials::OPENAI_AUTH)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("api-key"));
    match (login, key) {
        (_, true) if wants_key => Some(OpenAiAuth::ApiKey),
        (true, _) => Some(OpenAiAuth::ChatGpt),
        (false, true) => Some(OpenAiAuth::ApiKey),
        (false, false) => None,
    }
}

/// How this process reaches OpenAI models when the session's models
/// don't say (its client was built so): see [`client`].
pub fn spare_openai_auth(codex_auth: Option<&Path>) -> Option<OpenAiAuth> {
    available_openai_auth(&credentials::var, &credentials::codex_auths(codex_auth))
}

/// The reasoning efforts `model` takes, from Arko's five, lowest first:
/// those the provider accepts for it (arko checks OpenAI's per model as it
/// builds a request; these follow its rules). Empty for a model that
/// doesn't reason.
pub fn efforts(model: &Model) -> Vec<Effort> {
    use Effort::{High, Low, Max, Medium, Xhigh};
    let all = vec![Low, Medium, High, Xhigh, Max];
    match model.provider {
        Provider::Anthropic | Provider::ClaudeCode => {
            let mut efforts = vec![Low, Medium, High];
            if model.supports_anthropic_xhigh_effort() {
                efforts.push(Xhigh);
            }
            if model.supports_anthropic_max_effort() {
                efforts.push(Max);
            }
            efforts
        }
        Provider::OpenAI if model.model_type != ModelType::Reasoning => Vec::new(),
        Provider::OpenAI => {
            let id = model.id.as_str();
            let pro = |version: &str| id.starts_with(&format!("gpt-{version}-pro"));
            if model.is_openai_gpt_6_astra()
                || model.is_openai_gpt_6_sol_or_luna()
                || model.is_openai_gpt_5_6()
            {
                all
            } else if pro("5.5") || pro("5.4") || pro("5.2") {
                vec![Medium, High, Xhigh]
            } else if pro("5") {
                vec![High]
            } else if id.starts_with("gpt-5.5")
                || id.starts_with("gpt-5.4")
                || id.starts_with("gpt-5.2")
                || id.starts_with("gpt-5.3")
            {
                vec![Low, Medium, High, Xhigh]
            } else {
                vec![Low, Medium, High]
            }
        }
        Provider::Google if model.model_type != ModelType::Reasoning => Vec::new(),
        Provider::Google if model.is_gemini_3_pro() => vec![Low, High],
        Provider::Google => vec![Low, Medium, High],
        // OpenRouter maps each effort to the model's own levels.
        Provider::OpenRouter => all,
    }
}

/// The effort `model` runs at when `wanted` was asked for without a check:
/// `wanted` when it takes it, else the highest it takes below it, else its
/// lowest. (A model that doesn't reason keeps `wanted`, which it ignores.)
pub fn fit_effort(model: &Model, wanted: Effort) -> Effort {
    let efforts = efforts(model);
    if efforts.is_empty() || efforts.contains(&wanted) {
        return wanted;
    }
    efforts
        .iter()
        .rev()
        .find(|effort| **effort < wanted)
        .or(efforts.first())
        .copied()
        .unwrap_or(wanted)
}

/// An effort's name (`low` … `max`), or what the names are.
pub fn parse_effort(text: &str) -> Result<Effort, String> {
    text.trim().parse::<Effort>().map_err(|_| {
        format!("`{text}` isn't an effort. The efforts are low, medium, high, xhigh and max.")
    })
}

/// What the user is told when `model` can't be used: it needs `needs`
/// (from [`missing_credential`] or [`credential_name`]), and how to get it.
pub fn needs_text(model: &str, needs: &str) -> String {
    if needs == "Claude Code signed in" {
        format!("{model} needs Claude Code signed in. Sign in with `claude auth login`.")
    } else if needs == "Claude Code updated" {
        format!("{model} needs Claude Code updated. Update it with `claude update`.")
    } else if needs.starts_with("Claude Code") {
        format!("{model} needs {needs}.")
    } else {
        format!("{model} needs {needs}. /keys adds API keys.")
    }
}

/// The efforts `model` takes, as a sentence's list: `low, medium or high`.
pub fn efforts_text(model: &Model) -> String {
    let names: Vec<String> = efforts(model).iter().map(Effort::to_string).collect();
    match names.as_slice() {
        [] => "no effort (it doesn't reason)".to_string(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// A group of the models `/model` offers, with one way to reach them and
/// what it needs when that isn't there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGroup {
    /// The way, as the picker heads it ("Claude Code, on your Claude plan").
    pub name: String,
    /// What the way needs that isn't there, which keeps its models from
    /// being chosen.
    pub needs: Option<String>,
    /// Each model's name as `/model` takes it, with its alias.
    pub models: Vec<(String, &'static str)>,
}

/// What the picker says beside Claude Mythos 5.1, on any route.
pub const MYTHOS_ACCESS: &str = "Needs access through Anthropic's Cyber Verification Program";

/// Claude Code's words when the API turns down the model it asked for,
/// because it doesn't exist or the account may not use it (Claude Code
/// 2.1.299: "There's an issue with the selected model (claude-mythos-5-1).
/// It may not exist or you may not have access to it.").
const CLAUDE_CODE_NO_ACCESS: &str = "you may not have access to it";

/// What the model picker says beside the model `name` (as `/model` takes
/// it), when there is something to know before choosing it. Claude Mythos
/// 5.1 is open only to organizations Anthropic has verified.
pub fn model_note(name: &str) -> Option<&'static str> {
    let model = resolve_model(name).ok()?;
    model.is_mythos_5_1().then_some(MYTHOS_ACCESS)
}

/// A failed request's error `message`, with why the model may be refused
/// added where the message shows it: Claude Code turned down Claude Mythos
/// 5.1, which needs access through Anthropic's Cyber Verification Program.
pub fn explain_error(message: &str) -> std::borrow::Cow<'_, str> {
    if message.contains("claude-mythos") && message.contains(CLAUDE_CODE_NO_ACCESS) {
        format!("{message} Claude Mythos 5.1 needs access through Anthropic's Cyber Verification Program.").into()
    } else {
        message.into()
    }
}

/// The models `/model` offers, by the way they are reached: Claude Code,
/// the Anthropic API, OpenAI, Gemini and OpenRouter, the ways that can be
/// used first. `needs` says what a model (a way's first) needs that isn't
/// there, and `openai_auth` how OpenAI models are reached. `current` (the
/// agents' models now) are listed in their group when the list leaves
/// them out. OpenRouter serves any model, so its group lists the ones
/// picked (`recent`, up to [`crate::openrouter::RECENT`], newest first), and
/// one to start with while there are none.
pub fn model_menu(
    needs: impl Fn(&str) -> Option<String>,
    openai_auth: Option<OpenAiAuth>,
    current: &[&str],
    recent: &[String],
) -> Vec<ModelGroup> {
    let openai = match openai_auth {
        Some(OpenAiAuth::ApiKey) => "OpenAI, with your API key",
        _ => "OpenAI, on your ChatGPT plan",
    };
    // A group's name, a model that tells whether it is usable, and its models
    // with their aliases.
    type Group = (
        String,
        &'static str,
        &'static [(&'static str, &'static str)],
    );
    let groups: [Group; 5] = [
        (
            "Claude Code, on your Claude plan".to_string(),
            "cc:claude-opus-5-5",
            &[
                ("cc:claude-opus-5-5", "cc:opus"),
                ("cc:claude-fable-5-1", "cc:fable"),
                ("cc:claude-sonnet-5-5", "cc:sonnet"),
                ("cc:claude-haiku-5-5", "cc:haiku"),
                ("cc:claude-mythos-5-1", "cc:mythos"),
            ],
        ),
        (
            "Anthropic API, billed per token".to_string(),
            "claude-opus-5-5",
            &[
                ("claude-opus-5-5", "opus"),
                ("claude-fable-5-1", "fable"),
                ("claude-sonnet-5-5", "sonnet"),
                ("claude-haiku-5-5", "haiku"),
                ("claude-mythos-5-1", "mythos"),
            ],
        ),
        (
            openai.to_string(),
            "gpt-6.1-sol",
            &[
                ("gpt-6.1-sol", "sol"),
                ("gpt-6-astra", "astra"),
                ("gpt-6.1-terra", "terra"),
                ("gpt-6-luna", "luna"),
            ],
        ),
        (
            "Google Gemini, billed per token".to_string(),
            "gemini-3.1-pro-preview",
            &[
                ("gemini-3.1-pro-preview", "gemini"),
                ("gemini-3.8-flash", "flash"),
            ],
        ),
        (
            "OpenRouter, billed per token".to_string(),
            "openrouter:anthropic/claude-opus-5.5",
            &[("openrouter:anthropic/claude-opus-5.5", "")],
        ),
    ];
    let mut menu: Vec<ModelGroup> = groups
        .into_iter()
        .map(|(name, sample, models)| ModelGroup {
            name,
            needs: needs(sample),
            models: models
                .iter()
                .map(|(model, alias)| (model.to_string(), *alias))
                .collect(),
        })
        .collect();
    // OpenRouter's models are the ones picked.
    if !recent.is_empty() {
        menu[4].models = recent
            .iter()
            .take(crate::openrouter::RECENT)
            .map(|name| (name.clone(), ""))
            .collect();
    }
    // The agents' own models, where the list leaves them out.
    for name in current {
        if menu
            .iter()
            .any(|group| group.models.iter().any(|(model, _)| model == name))
        {
            continue;
        }
        let Ok(model) = resolve_model(name) else {
            continue;
        };
        let group = match model.provider {
            Provider::ClaudeCode => 0,
            Provider::Anthropic => 1,
            Provider::OpenAI => 2,
            Provider::Google => 3,
            Provider::OpenRouter => 4,
        };
        menu[group].models.push((name.to_string(), ""));
    }
    // The ways that can be used first, in their order.
    menu.sort_by_key(|group| group.needs.is_some());
    menu
}

/// The models of the last session started, kept in Arko's folder for the
/// next session's default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastModels {
    pub main: String,
    /// The workers' model, when it differed from the main one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    /// The effort each model last ran at, by the model's name as a session
    /// saves it (`claude-opus-5-5`, `openrouter:acme/model`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub efforts: std::collections::BTreeMap<String, String>,
}

/// The file in Arko's folder that holds [`LastModels`].
const LAST_MODELS_FILE: &str = "last-model.json";

impl LastModels {
    /// The last session's models, if one was saved and can be read.
    pub fn load(root: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(root.join(LAST_MODELS_FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Keeps a new session's models for the next session's default. Only
    /// sessions with a terminal count: a `-p` run is often a script or a
    /// one-off, and its model shouldn't change what the next interactive
    /// session starts on.
    pub fn remember(root: &Path, settings: &Settings) -> std::io::Result<()> {
        let mut efforts = Self::load(root)
            .map(|last| last.efforts)
            .unwrap_or_default();
        efforts.insert(
            settings.main.model.clone(),
            settings.main.effort.to_string(),
        );
        if settings.worker.model != settings.main.model {
            efforts.insert(
                settings.worker.model.clone(),
                settings.worker.effort.to_string(),
            );
        }
        Self {
            main: settings.main.model.clone(),
            worker: (settings.worker.model != settings.main.model)
                .then(|| settings.worker.model.clone()),
            efforts,
        }
        .write(root)
    }

    /// Keeps the effort `model` was switched to, for the next time it is
    /// chosen. Nothing is kept before a session has kept its models.
    pub fn remember_effort(root: &Path, model: &Model, effort: Effort) -> std::io::Result<()> {
        let Some(mut last) = Self::load(root) else {
            return Ok(());
        };
        last.efforts.insert(model_name(model), effort.to_string());
        last.write(root)
    }

    fn write(&self, root: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(root)?;
        let path = root.join(LAST_MODELS_FILE);
        let temporary = root.join(format!("{LAST_MODELS_FILE}.tmp"));
        std::fs::write(&temporary, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(temporary, path)
    }

    /// The effort `model` last ran at, when it still takes it.
    pub fn effort_of(root: &Path, model: &str) -> Option<Effort> {
        let model = resolve_model(model).ok()?;
        let kept = Self::load(root)?.efforts.get(&model_name(&model))?.clone();
        let effort = parse_effort(&kept).ok()?;
        efforts(&model).contains(&effort).then_some(effort)
    }
}

/// The Gemini API key: GOOGLE_API_KEY, else GEMINI_API_KEY, as arko reads
/// them.
pub fn gemini_key() -> Option<String> {
    credentials::var(credentials::GOOGLE_KEY).or_else(|| credentials::var(credentials::GEMINI_KEY))
}

/// Checks a new session's choices that depend on the OpenAI auth: Fast
/// mode with an API key only on the models OpenAI offers it for. (Over the
/// ChatGPT login arko checks it at the first request.)
pub fn check_openai_choices(settings: &Settings) -> Result<(), String> {
    if settings.openai_auth != Some(OpenAiAuth::ApiKey) {
        return Ok(());
    }
    for model in [&settings.main, &settings.worker] {
        let resolved = resolve_model(&model.model)?;
        if model.fast
            && resolved.provider == Provider::OpenAI
            && !resolved.pricing.offers_tier(&ServiceTier::Fast)
        {
            return Err(format!(
                "{} has no Fast mode with an API key, because OpenAI doesn't offer it for this model",
                resolved.id
            ));
        }
    }
    Ok(())
}

/// Why there is no model client.
pub enum Failure {
    /// Credentials are missing, or the auth file fails.
    Need(Need),
    Other(String),
}

/// The model client for the session's models: each provider with the
/// credentials it needs, OpenAI as the session's [`OpenAiAuth`] says (never
/// both ways), and Claude Code checked: installed and signed in.
pub fn client(
    codex_auth: Option<&Path>,
    root: &Path,
    settings: &Settings,
) -> Result<LLMClient, Failure> {
    // Debug builds can talk to a local test model server (the full-screen
    // view's end-to-end checks run the real binary on one).
    #[cfg(debug_assertions)]
    if let Some(url) = std::env::var("ARKO_TEST_MODEL_URL")
        .ok()
        .filter(|url| !url.is_empty())
    {
        return Ok(LLMClient::new().with_openai_client(
            arko_ai::providers::openai::OpenAIClient::new("test-key").with_base_url(&url),
        ));
    }
    client_with(
        settings,
        root,
        credentials::var,
        credentials::codex_auths(codex_auth),
        claude_code::check_recent,
    )
}

/// [`client`], with the settings read through `var`, the Codex auth.json
/// files the user named (else Codex's own), and how Claude Code is checked.
/// OpenRouter's client carries the settings in Arko's folder `root`.
fn client_with(
    settings: &Settings,
    root: &Path,
    var: impl Fn(&str) -> Option<String>,
    auth_files: Vec<PathBuf>,
    check_claude_code: impl Fn() -> Result<claude_code::Ready, claude_code::Problem>,
) -> Result<LLMClient, Failure> {
    let models = [&settings.main, &settings.worker]
        .into_iter()
        .map(|model| Ok((model, resolve_model(&model.model)?)))
        .collect::<Result<Vec<(&ModelSettings, Model)>, String>>()
        .map_err(Failure::Other)?;
    let uses = |provider: Provider| models.iter().any(|(_, model)| model.provider == provider);
    // Sessions saved before the choice existed reached OpenAI models over
    // the ChatGPT login. A session without OpenAI models still gets OpenAI
    // when there is a credential for it, one way only, so that `/model
    // worker` can move workers there after the conversation has started.
    let spare = !uses(Provider::OpenAI);
    let auth = match uses(Provider::OpenAI) {
        true => Some(settings.openai_auth.unwrap_or(OpenAiAuth::ChatGpt)),
        false => available_openai_auth(&var, &auth_files),
    };
    // OpenRouter's privacy settings may be in its file. Without them read, a
    // session that uses OpenRouter doesn't start, and one that doesn't gets no
    // OpenRouter client, which a later model or profile could use unrestricted.
    let routing =
        var(credentials::OPENROUTER_KEY).map(|_| crate::openrouter::Settings::read_to_use(root));
    if uses(Provider::OpenRouter)
        && let Some(Err(error)) = &routing
    {
        return Err(Failure::Other(error.clone()));
    }
    let routing_read = !matches!(routing, Some(Err(_)));
    if let Some(Err(error)) = &routing {
        eprintln!("arko: {error} OpenRouter stays off in this session.");
    }
    let from_env = |auth: Option<OpenAiAuth>| {
        LLMClient::from_env_with(|name| {
            let wanted = if name.starts_with("OPENAI_OAUTH_") {
                auth == Some(OpenAiAuth::ChatGpt) && auth_files.is_empty()
            } else if name == credentials::OPENAI_KEY {
                auth == Some(OpenAiAuth::ApiKey)
            } else if name == credentials::OPENROUTER_KEY {
                routing_read
            } else {
                true
            };
            wanted.then(|| var(name)).flatten()
        })
    };
    let mut client = match (from_env(auth), spare) {
        (Ok(client), _) => client,
        // A spare OpenAI login that fails (part of the OPENAI_OAUTH_*
        // variables) is left out: no model of the session needs it.
        (Err(_), true) => from_env(None).map_err(|error| Failure::Other(error.to_string()))?,
        (Err(error), false) => return Err(Failure::Other(error.to_string())),
    };
    if client.has_provider(&Provider::OpenRouter)
        && let Some(key) = var(credentials::OPENROUTER_KEY)
        && let Some(Ok(routing)) = &routing
    {
        client.set_openrouter_client(crate::openrouter::client_with(&key, routing, None));
    }
    if spare
        && auth == Some(OpenAiAuth::ChatGpt)
        && !client.has_provider(&Provider::OpenAI)
        && let Ok((accounts, _)) =
            logins::chatgpt_accounts(&logins::paths_in_use_of(&auth_files), None)
    {
        client = client.with_chatgpt_accounts(accounts);
    }
    if !spare && auth == Some(OpenAiAuth::ChatGpt) && !client.has_provider(&Provider::OpenAI) {
        let (model, _) = models
            .iter()
            .find(|(_, model)| model.provider == Provider::OpenAI)
            .expect("an OpenAI model");
        let need = |problem: String| {
            Failure::Need(Need::ChatGptLogin {
                model: model.model.clone(),
                problem,
            })
        };
        let (accounts, problems) =
            logins::chatgpt_accounts(&logins::paths_in_use_of(&auth_files), None).map_err(need)?;
        // A login left out is said, while the others serve.
        for problem in problems {
            eprintln!("arko: {problem}");
        }
        client = client.with_chatgpt_accounts(accounts);
    }
    for (settings, model) in &models {
        if model.provider == Provider::ClaudeCode || client.has_provider(&model.provider) {
            continue;
        }
        let model_name = settings.model.clone();
        return Err(Failure::Need(match model.provider {
            Provider::Anthropic => Need::AnthropicKey { model: model_name },
            Provider::OpenAI => Need::OpenAiKey { model: model_name },
            Provider::Google => Need::GeminiKey { model: model_name },
            Provider::OpenRouter => Need::OpenRouterKey { model: model_name },
            Provider::ClaudeCode => unreachable!("Claude Code takes no client"),
        }));
    }
    if let Some((settings, _)) = models
        .iter()
        .find(|(_, model)| model.provider == Provider::ClaudeCode)
    {
        match check_claude_code() {
            Ok(ready) => {
                if let Some(warning) = ready.warning() {
                    eprintln!("arko: {warning}");
                }
            }
            Err(problem) => {
                return Err(Failure::Need(Need::ClaudeCode {
                    model: settings.model.clone(),
                    problem,
                }));
            }
        }
    }
    Ok(client)
}

/// Checks a new session's OpenRouter models against OpenRouter's list of
/// models that take tool calls and that the OpenRouter settings in Arko's
/// folder `root` allow (read without the API key, and fetched only when the
/// list in memory is old): the slug is on it, and, unless `--handover-at` was
/// given (`explicit_handover`), the handover is sized from its context
/// window. `local` is a server standing in for OpenRouter (tests). Returns
/// what to tell the user. Without the list (offline), the session starts
/// unchecked, handing over at [`UNKNOWN_LIMIT_HANDOVER`].
pub async fn check_openrouter(
    settings: &mut Settings,
    explicit_handover: bool,
    root: &Path,
    local: Option<&str>,
) -> Result<Vec<String>, String> {
    let uses = [&settings.main, &settings.worker]
        .iter()
        .any(|model| strip_prefix(&model.model, OPENROUTER).is_some());
    if !uses {
        return Ok(Vec::new());
    }
    // (A file that can't be read may hold the privacy settings, so the
    // session doesn't start on a guess.)
    let routing = crate::openrouter::Settings::read_to_use(root)?;
    let catalog = crate::openrouter::checked(root, &routing, local).await;
    apply_catalog(
        settings,
        catalog.as_deref(),
        explicit_handover,
        routing.filters_summary().as_deref(),
    )
}

/// [`check_openrouter`] with the catalog read (or why it couldn't be).
/// `filters` says what the OpenRouter settings rule out of the list (zero
/// data retention, a region), when they do.
fn apply_catalog(
    settings: &mut Settings,
    catalog: Result<&OpenRouterCatalog, &String>,
    explicit_handover: bool,
    filters: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    for model in [&mut settings.main, &mut settings.worker] {
        let Some(slug) = strip_prefix(&model.model, OPENROUTER).map(str::to_string) else {
            continue;
        };
        let name = model.model.clone();
        let note = |notes: &mut Vec<String>, text: String| {
            if !notes.contains(&text) {
                notes.push(text);
            }
        };
        let handover = |model: &ModelSettings| {
            crate::handover::shown(model.handover_at.unwrap_or(UNKNOWN_LIMIT_HANDOVER))
        };
        let catalog = match catalog {
            Ok(catalog) => catalog,
            Err(error) => {
                note(
                    &mut notes,
                    format!(
                        "Could not read OpenRouter's model catalog ({error}), so Arko can't check {name} or tell its context size. It hands over at {} tokens (--handover-at sets it).",
                        handover(model)
                    ),
                );
                continue;
            }
        };
        if slug.starts_with('@') {
            note(
                &mut notes,
                format!(
                    "Arko can't tell the context size of the preset {name}, so it hands over at {} tokens (--handover-at sets it).",
                    handover(model)
                ),
            );
            continue;
        }
        let Some(listed) = catalog.get(&slug) else {
            return Err(match filters {
                Some(filters) => format!(
                    "{name} isn't among the OpenRouter models that take tool calls and that your settings allow ({filters}). It may be a mistyped slug, or the settings rule it out. /openrouter changes them, and https://openrouter.ai/models lists the slugs."
                ),
                None => format!(
                    "{name} isn't in OpenRouter's model catalog of models that take tool calls. https://openrouter.ai/models lists the slugs (such as anthropic/claude-opus-5.5)"
                ),
            });
        };
        if listed
            .openrouter_info()
            .is_some_and(|info| !info.supports_parameter("tools"))
        {
            return Err(format!(
                "{name} doesn't take tool calls on OpenRouter, and Arko's agents work through tools"
            ));
        }
        if !explicit_handover {
            model.handover_at = Some(default_handover(&listed, None));
        } else if let Some(most) = crate::agents::max_handover(&listed, None)
            && model.handover_at.is_some_and(|at| at > most)
        {
            // --handover-at, held under what the model leaves room for.
            model.handover_at = Some(most);
            note(
                &mut notes,
                format!(
                    "{name} hands over at {} tokens, the largest size Arko allows for its model.",
                    crate::handover::shown(most)
                ),
            );
        }
    }
    // What sessions read before the handover was set per model.
    settings.handover_at = settings.main.handover_at.unwrap_or(settings.handover_at);
    Ok(notes)
}

/// A usage window by its length, in whole days, else in whole hours, else in
/// minutes (`7 d`, `5 h`, `90 min`). `unknown` names one that doesn't say.
fn window_name(minutes: Option<u64>, unknown: &str) -> String {
    match minutes {
        Some(minutes) if minutes > 0 && minutes % (24 * 60) == 0 => {
            format!("{} d", minutes / (24 * 60))
        }
        Some(minutes) if minutes > 0 && minutes % 60 == 0 => format!("{} h", minutes / 60),
        Some(minutes) => format!("{minutes} min"),
        None => unknown.to_string(),
    }
}

/// A plan's usage windows, as the status line and `/cost` say them:
/// `Claude plan 5 h 12%, 7 d 3%`. `None` when it reports no window.
pub fn plan_usage_text(usage: &PlanUsage) -> Option<String> {
    let windows = [(&usage.primary, "5 h"), (&usage.secondary, "7 d")]
        .into_iter()
        .filter_map(|(window, name)| {
            let window = window.as_ref()?;
            let name = window_name(window.window_minutes, name);
            Some(format!("{name} {:.0}%", window.used_percent))
        })
        .collect::<Vec<_>>();
    if windows.is_empty() {
        return None;
    }
    let credits = usage.credits.as_ref().and_then(credits_text);
    let extra = usage.extra_usage.as_ref().and_then(extra_usage_text);
    let mut text = format!("{} {}", plan_name(usage.plan), windows.join(", "));
    for part in credits.into_iter().chain(extra) {
        text.push_str(", ");
        text.push_str(&part);
    }
    Some(text)
}

/// Whether a Claude account's extra usage is on, or pays for requests now.
fn extra_usage_on(extra: &arko_ai::ExtraUsage) -> bool {
    extra.enabled == Some(true) || extra.in_use
}

/// A Claude account's extra usage, as a plan's usage line says it: "extra
/// usage $271.40 of $500.00" with its amounts, "extra usage $271.40 used"
/// without a limit, and "extra usage in use" or "extra usage on" when the
/// amounts aren't known. `None` when it isn't on.
fn extra_usage_text(extra: &arko_ai::ExtraUsage) -> Option<String> {
    extra_usage_words(extra, "extra usage ")
}

/// A Claude account's extra usage, as the full-screen view's status line
/// says it: "extra usage: $271.40 of $500.00", "extra usage: $271.40 used",
/// "extra usage in use" or "extra usage on". `None` when it isn't on.
// Only the full-screen view says it, so a build without it leaves it unused.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub fn extra_usage_figure(extra: &arko_ai::ExtraUsage) -> Option<String> {
    extra_usage_words(extra, "extra usage: ")
}

/// A Claude account's extra usage in words, with `label` before its amounts.
fn extra_usage_words(extra: &arko_ai::ExtraUsage, label: &str) -> Option<String> {
    if !extra_usage_on(extra) {
        return None;
    }
    // The amounts are in minor units of the currency (cents for USD).
    let places = extra.decimal_places.unwrap_or(2);
    let (before, after) = match extra.currency.as_deref() {
        None | Some("USD") => ("$", String::new()),
        Some(code) => ("", format!(" {code}")),
    };
    let amount = |minor: f64| {
        format!(
            "{before}{:.*}",
            places as usize,
            minor / 10_f64.powi(places as i32)
        )
    };
    Some(match (extra.used_credits, extra.monthly_limit) {
        (Some(used), Some(limit)) => {
            format!("{label}{} of {}{after}", amount(used), amount(limit))
        }
        (Some(used), None) => format!("{label}{}{after} used", amount(used)),
        _ if extra.in_use => "extra usage in use".to_string(),
        _ => "extra usage on".to_string(),
    })
}

/// A ChatGPT account's credits, as the status line says them: "credits
/// left 12.50", "has credits" when the backend gives no balance, or
/// "unlimited credits". `None` when it has none.
fn credits_text(credits: &arko_ai::PlanCredits) -> Option<String> {
    if credits.unlimited {
        return Some("unlimited credits".to_string());
    }
    if !credits.has_credits {
        return None;
    }
    Some(match credits.balance.as_deref().map(str::trim) {
        Some(balance) if !balance.is_empty() => format!("credits left {balance}"),
        _ => "has credits".to_string(),
    })
}

/// A ChatGPT account's credits, as the full-screen view's status line says
/// them: "credits: 12.50" (the balance to two decimals, or as the backend
/// wrote it when it isn't a number), "has credits" when the backend gives no
/// balance, or "unlimited credits". `None` when it has none.
// Only the full-screen view says it, so a build without it leaves it unused.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub fn credits_figure(credits: &arko_ai::PlanCredits) -> Option<String> {
    if credits.unlimited {
        return Some("unlimited credits".to_string());
    }
    if !credits.has_credits {
        return None;
    }
    Some(match credits.balance.as_deref().map(str::trim) {
        Some(balance) if !balance.is_empty() => match balance.parse::<f64>() {
            Ok(number) if number.is_finite() => format!("credits: {number:.2}"),
            _ => format!("credits: {balance}"),
        },
        _ => "has credits".to_string(),
    })
}

/// `usage`'s windows (5 hours, 7 days), as the status gives them.
pub fn plan_windows(usage: &PlanUsage) -> Vec<crate::remote::PlanWindow> {
    [(&usage.primary, "5 h"), (&usage.secondary, "7 d")]
        .into_iter()
        .filter_map(|(window, name)| {
            let window = window.as_ref()?;
            Some(crate::remote::PlanWindow {
                label: window_name(window.window_minutes, name),
                used_percent: window.used_percent,
                resets_at: window.resets_at.and_then(|at| i64::try_from(at).ok()),
            })
        })
        .collect()
}

/// `usage` as the status gives a plan's usage, with its `text` and how many
/// ChatGPT logins are `set_aside`. Credits and extra usage are there only when
/// the text shows them.
pub fn plan_status(usage: &PlanUsage, text: String, set_aside: usize) -> crate::remote::PlanStatus {
    crate::remote::PlanStatus {
        plan: plan_name(usage.plan).to_string(),
        windows: plan_windows(usage),
        credits: usage.credits.clone().filter(arko_ai::PlanCredits::usable),
        set_aside,
        extra_usage: usage.extra_usage.clone().filter(extra_usage_on),
        text,
    }
}

/// Every plan's latest usage, in one line: a session with agents on both
/// plans shows both.
pub fn plan_usage_line(plans: &std::collections::BTreeMap<String, String>) -> Option<String> {
    (!plans.is_empty()).then(|| plans.values().cloned().collect::<Vec<_>>().join(" · "))
}

/// The key [`plan_usage_text`]'s report of `plan` is kept under, by plan
/// (`Stats::plans`), and the plan as `/cost` names it.
pub fn plan_name(plan: Option<SubscriptionPlan>) -> &'static str {
    match plan {
        Some(SubscriptionPlan::ClaudeCode) => "Claude plan",
        // Reports saved before plans were named came from ChatGPT.
        Some(SubscriptionPlan::ChatGpt) | None => "ChatGPT plan",
    }
}

/// The plan that pays for `model`'s calls when the session reaches OpenAI
/// models by `openai_auth`, as `/cost` names it: the Claude plan for `cc:`
/// models, the ChatGPT plan for OpenAI models on the ChatGPT login. `None`
/// for a model billed per token (an API key, Gemini, OpenRouter).
pub fn plan_of(model: &Model, openai_auth: Option<OpenAiAuth>) -> Option<&'static str> {
    match model.provider {
        Provider::ClaudeCode => Some(plan_name(Some(SubscriptionPlan::ClaudeCode))),
        Provider::OpenAI if openai_auth != Some(OpenAiAuth::ApiKey) => {
            Some(plan_name(Some(SubscriptionPlan::ChatGpt)))
        }
        _ => None,
    }
}

/// What a call would have cost at API list prices: its estimate when a
/// plan covered it, else what it was billed.
pub(crate) fn at_api_prices(usage: &UsageInfo, billed: f64) -> f64 {
    match usage.billing {
        BillingKind::Subscription => usage.total_cost,
        // Calls billed in different ways: the estimate covers them all,
        // those a plan paid for too.
        BillingKind::Mixed => usage.total_cost.max(billed),
        _ => billed,
    }
}

/// The plan that paid for calls of `model` whose usage was billed as
/// `billing`: by the model's provider, whatever the session reaches OpenAI
/// models by now (a run saved on the ChatGPT login stays the ChatGPT plan's
/// after the session moved to an API key). `None` for calls billed per
/// token.
// shortcut: a run billed partly on a plan and partly per token reads as all
// the plan's, since its tokens can't be told apart. Arko can't make such a
// run on a plan's provider today (Claude Code bills every call to the plan,
// and a session reaches OpenAI by one login or one key), so a split by
// tokens would need the run's calls one by one.
fn plan_that_paid(model: &Model, billing: BillingKind) -> Option<&'static str> {
    if !matches!(billing, BillingKind::Subscription | BillingKind::Mixed) {
        return None;
    }
    match model.provider {
        Provider::ClaudeCode => Some(plan_name(Some(SubscriptionPlan::ClaudeCode))),
        Provider::OpenAI => Some(plan_name(Some(SubscriptionPlan::ChatGpt))),
        _ => None,
    }
}

/// What the session's usage would have cost at API list prices: what the
/// calls billed per token cost, and the list-price estimate of those a
/// plan covered (Claude Code, the ChatGPT login). `None` when nothing ran
/// on a plan: what was billed says it all then.
#[cfg(test)]
pub fn api_price(ledger: &CostLedger) -> Option<f64> {
    let on_a_plan = ledger.entries.iter().any(|entry| {
        matches!(&entry.source, CostSource::Agent { usage, .. } if usage.billing == BillingKind::Subscription)
    });
    on_a_plan.then(|| {
        ledger
            .entries
            .iter()
            .map(|entry| match &entry.source {
                CostSource::Agent { usage, .. } => at_api_prices(usage, entry.amount),
                CostSource::Manual { .. } => entry.amount,
            })
            .sum()
    })
}

/// One line of `/cost`; a `detail` line is indented and dim.
#[derive(Debug, Clone, PartialEq)]
pub struct CostLine {
    pub text: String,
    pub detail: bool,
}

/// Some agents' part of the costs (the main agent's, or the workers'), as
/// `/cost` adds it up from the ledger.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostShare {
    /// What was billed, in dollars.
    pub billed: f64,
    /// What it would cost at API list prices.
    pub api: f64,
    pub input: usize,
    pub output: usize,
    /// The plan that covered its calls, when one did.
    pub plan: Option<&'static str>,
}

impl CostShare {
    fn tokens(&self) -> String {
        format!(
            "{}k input and {}k output tokens",
            self.input / 1000,
            self.output / 1000
        )
    }

    /// Adds a call that billed `billed`.
    fn add(&mut self, usage: &UsageInfo, billed: f64) {
        self.billed += billed;
        self.api += at_api_prices(usage, billed);
        self.input += usage.input_tokens;
        self.output += usage.output_tokens;
    }

    /// Whether it holds any call that reported tokens.
    pub fn used(&self) -> bool {
        self.input + self.output > 0
    }

    /// `parts` added up, with their plan when there is one part.
    pub fn sum(parts: &[CostShare]) -> CostShare {
        let mut sum = CostShare {
            plan: match parts {
                [one] => one.plan,
                _ => None,
            },
            ..CostShare::default()
        };
        for part in parts {
            sum.billed += part.billed;
            sum.api += part.api;
            sum.input += part.input;
            sum.output += part.output;
        }
        sum
    }
}

/// What one of the main agent's runs used, its calls added up, on the model
/// it ran: what `/cost` splits the main agent's part by.
#[derive(Debug, Clone)]
pub struct RunCost {
    pub model: Model,
    pub usage: UsageInfo,
}

/// What each of the main agent's runs in `workflow` used, oldest first.
pub fn main_run_costs(workflow: &arko_workflows::Workflow) -> Vec<RunCost> {
    let results = workflow.context().agent_results.read();
    let mut runs: Vec<(chrono::DateTime<chrono::Utc>, RunCost)> = results
        .iter()
        .filter(|result| result.agent_name == crate::agents::MAIN)
        .map(|result| {
            let info = result.info();
            (
                info.start_time,
                RunCost {
                    model: info.agent_config.llm_config.model.clone(),
                    usage: info.usage.clone(),
                },
            )
        })
        .collect();
    runs.sort_by_key(|(started, _)| *started);
    runs.into_iter().map(|(_, run)| run).collect()
}

/// What the run `handle` used (`usage`, its calls added up), in one line that
/// ends the news its lead gets. It is worded as `/cost` words the workers'
/// share, with a plan named when it covered the calls (`provider` is the
/// run's model's). `None` when no call reported tokens.
// shortcut: a run whose calls mixed billing kinds reads as billed per token,
// which no run of Arko's can be today.
pub(crate) fn run_usage_line(
    handle: &str,
    usage: &UsageInfo,
    provider: &Provider,
) -> Option<String> {
    if usage.input_tokens + usage.output_tokens == 0 {
        return None;
    }
    let mut used = CostShare::default();
    used.add(usage, usage.billed());
    if usage.billing == BillingKind::Subscription {
        used.plan = Some(plan_name(Some(match provider {
            Provider::ClaudeCode => SubscriptionPlan::ClaudeCode,
            _ => SubscriptionPlan::ChatGpt,
        })));
    }
    // (A short run's few tokens are told whole, not as "0k".)
    let count = |tokens: usize| match tokens {
        0..1000 => tokens.to_string(),
        _ => format!("{}k", tokens / 1000),
    };
    let cached = match usage.cached_tokens.unwrap_or(0) {
        0 => String::new(),
        tokens => format!(" ({} cached)", count(tokens)),
    };
    let tokens = format!(
        "{} input{cached} and {} output tokens",
        count(used.input),
        count(used.output)
    );
    Some(match used.plan {
        Some(plan) => format!(
            "{handle} ran on the {plan}, so nothing was billed per token. It used {tokens}, ≈ ${:.2} at API prices.",
            used.api
        ),
        None => format!("{handle} used {tokens}, ${:.2}.", used.billed),
    })
}

/// `/cost`: what the session was billed, and, for what ran on a plan, what
/// it would have cost at API prices and the plan's usage windows. The main
/// agent and the workers are told apart when they were paid for
/// differently (one on a plan, the other per token).
#[cfg(test)]
pub fn cost_report(
    ledger: &CostLedger,
    settings: &Settings,
    main_runs: &[RunCost],
    plans: &std::collections::BTreeMap<String, String>,
) -> Vec<CostLine> {
    let (main, workers) = cost_shares(ledger, settings, main_runs, None);
    cost_lines(&main, &workers, plans)
}

/// The main agent's part of the costs in `ledger`, one for each way its
/// calls were paid for (a plan, or per token) in the order its runs first
/// used it, and the workers' part. The ledger says what the main agent
/// used in all, its calls under way included. Its runs `main_runs` say
/// which plan paid for what, each by its own model and billing, as
/// conversations can run on different models. What the ledger has beyond
/// what the runs recorded so far (a turn under way) is the current
/// conversation's, on `current` (its model), paid for as the session pays
/// for that model now. Without runs its calls go by `current`, else the
/// session's main model.
pub fn cost_shares(
    ledger: &CostLedger,
    settings: &Settings,
    main_runs: &[RunCost],
    current: Option<&Model>,
) -> (Vec<CostShare>, CostShare) {
    let plan_of_settings = |model: &ModelSettings| {
        resolve_model(&model.model)
            .ok()
            .and_then(|model| plan_of(&model, settings.openai_auth))
    };
    let current_plan = match current {
        Some(model) => plan_of(model, settings.openai_auth),
        None => plan_of_settings(&settings.main),
    };
    let mut main = CostShare::default();
    let mut workers = CostShare::default();
    let mut main_on_plan = false;
    for entry in &ledger.entries {
        let CostSource::Agent { agent_name, usage } = &entry.source else {
            continue;
        };
        if agent_name == crate::agents::MAIN {
            main.add(usage, entry.amount);
            main_on_plan |= usage.billing == BillingKind::Subscription;
        } else {
            workers.add(usage, entry.amount);
            if usage.billing == BillingKind::Subscription {
                workers.plan = plan_of_settings(&settings.worker);
            }
        }
    }
    let mut parts: Vec<CostShare> = Vec::new();
    let part_of = |parts: &mut Vec<CostShare>, plan: Option<&'static str>| -> usize {
        match parts.iter().position(|part| part.plan == plan) {
            Some(at) => at,
            None => {
                parts.push(CostShare {
                    plan,
                    ..CostShare::default()
                });
                parts.len() - 1
            }
        }
    };
    for run in main_runs
        .iter()
        .filter(|run| run.usage.input_tokens + run.usage.output_tokens > 0)
    {
        let at = part_of(&mut parts, plan_that_paid(&run.model, run.usage.billing));
        parts[at].add(&run.usage, run.usage.billed());
    }
    if parts.is_empty() {
        main.plan = if main_on_plan { current_plan } else { None };
        return (vec![main], workers);
    }
    // The runs split the ledger's total. What it has beyond them is the
    // current conversation's, and a total the runs go past (they never do,
    // as both count the same calls) holds them to it.
    let recorded = CostShare::sum(&parts);
    let rest = CostShare {
        billed: (main.billed - recorded.billed).max(0.0),
        api: (main.api - recorded.api).max(0.0),
        input: main.input.saturating_sub(recorded.input),
        output: main.output.saturating_sub(recorded.output),
        plan: None,
    };
    if rest.used() || rest.billed > 0.0 || rest.api > 0.0 {
        let at = part_of(&mut parts, current_plan);
        let part = &mut parts[at];
        part.billed += rest.billed;
        part.api += rest.api;
        part.input += rest.input;
        part.output += rest.output;
    }
    let scale = |total: f64, sum: f64| {
        if sum > total && sum > 0.0 {
            total / sum
        } else {
            1.0
        }
    };
    let (billed, api) = (
        scale(main.billed, recorded.billed),
        scale(main.api, recorded.api),
    );
    let tokens = |total: usize, sum: usize, part: usize| match sum > total {
        true => (part as f64 * total as f64 / sum as f64).round() as usize,
        false => part,
    };
    for part in &mut parts {
        part.billed *= billed;
        part.api *= api;
        part.input = tokens(main.input, recorded.input, part.input);
        part.output = tokens(main.output, recorded.output, part.output);
    }
    (parts, workers)
}

/// `/cost`'s lines for the main agent's parts `main` (see [`cost_shares`])
/// and the workers' part `workers`, with the plans' usage windows
/// (`plans`, by plan).
pub fn cost_lines(
    main: &[CostShare],
    workers: &CostShare,
    plans: &std::collections::BTreeMap<String, String>,
) -> Vec<CostLine> {
    let line = |text: String| CostLine {
        text,
        detail: false,
    };
    let detail = |text: String| CostLine { text, detail: true };
    let total = CostShare::sum(main);
    let mut lines = vec![line(format!(
        "${:.2} billed in this session · {}k input and {}k output tokens",
        total.billed + workers.billed,
        (total.input + workers.input) / 1000,
        (total.output + workers.output) / 1000
    ))];
    let parts: Vec<(&str, &CostShare)> = main
        .iter()
        .map(|part| ("main agent", part))
        .chain([("workers", workers)])
        .collect();
    let used: Vec<&(&str, &CostShare)> = parts.iter().filter(|(_, part)| part.used()).collect();
    let paid_alike = used.windows(2).all(|pair| pair[0].1.plan == pair[1].1.plan);
    if paid_alike {
        let plan = parts.iter().find_map(|(_, part)| part.plan);
        if let Some(plan) = plan {
            lines.push(line(format!(
                "≈ ${:.2} at API prices, which the {plan} covers",
                total.api + workers.api
            )));
        }
        if workers.used() {
            lines.push(detail(match plan {
                Some(_) => format!(
                    "the workers' share ≈ ${:.2} at API prices · {}",
                    workers.api,
                    workers.tokens()
                ),
                None => format!(
                    "the workers' share is ${:.2} · {}",
                    workers.billed,
                    workers.tokens()
                ),
            }));
        }
    } else {
        // (Each part the main agent's runs were paid for in, on a plan or
        // per token, has a line of its own.)
        for (name, part) in &used {
            lines.push(detail(match part.plan {
                Some(plan) => format!(
                    "{name} ≈ ${:.2} at API prices, on the {plan} · {}",
                    part.api,
                    part.tokens()
                ),
                None => format!(
                    "{name} ${:.2}, billed per token · {}",
                    part.billed,
                    part.tokens()
                ),
            }));
        }
    }
    // The plans' usage windows, as last reported.
    for (_, part) in &parts {
        if let Some(windows) = part.plan.and_then(|plan| plans.get(plan))
            && !lines.iter().any(|line| &line.text == windows)
        {
            lines.push(line(windows.clone()));
        }
    }
    lines
}

/// What the agents' configurations take from the session's providers
/// besides their models.
#[derive(Debug, Clone)]
pub struct Setup {
    /// How the session reaches OpenAI models.
    pub openai_auth: Option<OpenAiAuth>,
    /// How Claude Code runs, for the agents on `cc:` models.
    pub claude_code: arko_agents::ClaudeCodeSettings,
}

impl Setup {
    pub fn new(openai_auth: Option<OpenAiAuth>) -> Self {
        Self {
            openai_auth,
            claude_code: claude_code::settings(),
        }
    }
}

/// What writing the conversation's `tokens` to the prompt cache, and reading
/// them from it, cost on `model` now, per million tokens: at the prices in
/// effect today, the long-context rates above their threshold, the fast
/// mode's price when `fast`, and the long cache's write price (Claude's
/// hour, OpenAI's 12 hours) when `long_cache`.
/// Writes cost input where the provider charges nothing extra for them.
pub fn cache_prices(model: &Model, tokens: usize, fast: bool, long_cache: bool) -> (f64, f64) {
    let pricing = model.pricing.at(std::time::SystemTime::now());
    let long = pricing
        .large_input_threshold
        .is_some_and(|threshold| tokens > threshold);
    let rate = |base: Option<f64>, large: Option<f64>| if long { large.or(base) } else { base };
    let input = rate(
        Some(pricing.input_cost_per_million),
        pricing.large_input_cost_per_million,
    )
    .unwrap_or(pricing.input_cost_per_million);
    let write = long_cache
        .then(|| {
            rate(
                pricing.cache_write_1h_cost_per_million,
                pricing.large_cache_write_1h_cost_per_million,
            )
        })
        .flatten()
        .or(rate(
            pricing.cache_write_cost_per_million,
            pricing.large_cache_write_cost_per_million,
        ))
        .unwrap_or(input);
    let read = rate(
        pricing.cached_input_cost_per_million,
        pricing.large_cached_input_cost_per_million,
    )
    .unwrap_or(input);
    let tier = fast.then_some(arko_ai::ServiceTier::Fast);
    let times = pricing.tier_multiplier(tier.as_ref(), long);
    (write * times, read * times)
}
#[cfg(test)]
mod tests {
    use super::*;
    use arko_ai::AnthropicCacheTtl;

    /// The cache's prices are those in effect now: the fast mode's at its
    /// price, and the long-context rates above their threshold.
    #[test]
    fn the_cache_is_priced_as_the_request_would_be() {
        let sol = resolve_model("sol").unwrap();
        let pricing = sol.pricing.at(std::time::SystemTime::now()).clone();
        let read = pricing
            .cached_input_cost_per_million
            .unwrap_or(pricing.input_cost_per_million);
        let (_, short) = cache_prices(&sol, 100_000, false, false);
        assert!((short - read).abs() < 1e-9);
        let (_, fast) = cache_prices(&sol, 100_000, true, false);
        assert!(
            (fast - 2.0 * read).abs() < 1e-9,
            "the fast mode costs twice"
        );
        let threshold = pricing.large_input_threshold.unwrap();
        let long_read = pricing
            .large_cached_input_cost_per_million
            .or(pricing.cached_input_cost_per_million)
            .unwrap();
        let (_, long) = cache_prices(&sol, threshold + 1, false, false);
        assert!((long - long_read).abs() < 1e-9);
        // A write costs 1.25 times the input price, and 2.5 times for
        // OpenAI's long cache, which Claude's hour charges at twice.
        let write = |model: &Model, long: bool| cache_prices(model, 100_000, false, long).0;
        assert!((write(&sol, false) - 1.25 * pricing.input_cost_per_million).abs() < 1e-9);
        assert!((write(&sol, true) - 2.5 * pricing.input_cost_per_million).abs() < 1e-9);
        let opus = resolve_model("opus").unwrap();
        let input = opus.pricing.input_cost_per_million;
        assert!((write(&opus, true) - 2.0 * input).abs() < 1e-9);
    }

    /// The long cache is Claude's hour on every Claude route, and OpenAI's
    /// 12 hours on GPT-5.6 and later with an API key. The ChatGPT login,
    /// earlier OpenAI models and the other providers have none to choose.
    #[test]
    fn the_long_cache_is_each_routes_own() {
        use OpenAiAuth::{ApiKey, ChatGpt};
        let takes = |name: &str, auth| takes_long_cache(&resolve_model(name).unwrap(), auth);
        for name in ["opus", "cc:opus", "openrouter:anthropic/claude-opus-5.5"] {
            for auth in [None, Some(ApiKey), Some(ChatGpt)] {
                assert!(takes(name, auth), "{name} {auth:?}");
            }
            assert_eq!(CacheChoice::of(&resolve_model(name).unwrap()).long, "1h");
        }
        for name in ["sol", "luna", "gpt-5.6-sol", "gpt-6-astra"] {
            assert!(takes(name, Some(ApiKey)), "{name}");
            assert!(!takes(name, Some(ChatGpt)), "{name}");
            assert!(!takes(name, None), "{name}");
        }
        for name in [
            "gpt-5.5-2026-04-23",
            "gemini",
            "openrouter:openai/gpt-6-luna",
            "openrouter:deepseek/deepseek-v4",
        ] {
            assert!(!takes(name, Some(ApiKey)), "{name}");
        }
        let sol = resolve_model("sol").unwrap();
        let choice = CacheChoice::of(&sol);
        assert_eq!((choice.short, choice.long), ("30m", "12h"));
        // What the setting asks for, and the lifetime it reports, by auth.
        let long = generation(&sol, Effort::High, false, true, false).unwrap();
        let lifetime = |chatgpt| arko_ai::prompt_cache::prompt_cache_lifetime(&sol, &long, chatgpt);
        assert_eq!(
            lifetime(false),
            arko_ai::PromptCacheLifetime::Known(std::time::Duration::from_secs(12 * 3600))
        );
        assert_eq!(
            lifetime(true),
            arko_ai::PromptCacheLifetime::Known(std::time::Duration::from_secs(30 * 60))
        );
        let opus = resolve_model("opus").unwrap();
        let long = generation(&opus, Effort::High, false, true, false).unwrap();
        assert_eq!(
            arko_ai::prompt_cache::prompt_cache_lifetime(&opus, &long, false),
            arko_ai::PromptCacheLifetime::Known(std::time::Duration::from_secs(3600))
        );
    }
    use crate::agents::ModelSettings;

    /// An Arko folder that isn't there: no OpenRouter settings.
    fn no_root() -> &'static Path {
        Path::new("/no/such/arko")
    }

    fn catalog(json: &str) -> OpenRouterCatalog {
        OpenRouterCatalog::from_json(json).unwrap()
    }

    fn model(name: &str) -> ModelSettings {
        ModelSettings {
            model: model_name(&resolve_model(name).unwrap()),
            effort: Effort::Xhigh,
            fast: false,
            cache_1h: false,
            handover_at: Some(default_handover(&resolve_model(name).unwrap(), None)),
        }
    }

    #[test]
    fn names_are_ids_aliases_or_a_prefixed_route() {
        for (name, provider, saved) in [
            ("opus", Provider::Anthropic, "claude-opus-5-5"),
            ("Fable", Provider::Anthropic, "claude-fable-5-1"),
            ("sol", Provider::OpenAI, "gpt-6.1-sol"),
            ("Terra", Provider::OpenAI, "gpt-6.1-terra"),
            ("gpt-6.1-terra", Provider::OpenAI, "gpt-6.1-terra"),
            ("gpt-6-luna", Provider::OpenAI, "gpt-6-luna"),
            ("gemini", Provider::Google, "gemini-3.1-pro-preview"),
            ("flash", Provider::Google, "gemini-3.8-flash"),
            ("gemini-3.5-flash", Provider::Google, "gemini-3.5-flash"),
            (
                "openrouter:anthropic/claude-opus-5.5",
                Provider::OpenRouter,
                "openrouter:anthropic/claude-opus-5.5",
            ),
            (
                "OpenRouter:openai/gpt-6-luna:floor",
                Provider::OpenRouter,
                "openrouter:openai/gpt-6-luna:floor",
            ),
            ("cc:opus", Provider::ClaudeCode, "cc:claude-opus-5-5"),
            ("cc:fable", Provider::ClaudeCode, "cc:claude-fable-5-1"),
            ("haiku", Provider::Anthropic, "claude-haiku-5-5"),
            ("mythos", Provider::Anthropic, "claude-mythos-5-1"),
            ("cc:haiku", Provider::ClaudeCode, "cc:claude-haiku-5-5"),
            ("cc:mythos", Provider::ClaudeCode, "cc:claude-mythos-5-1"),
            (
                "claude-code:claude-opus-5-5",
                Provider::ClaudeCode,
                "cc:claude-opus-5-5",
            ),
        ] {
            let model = resolve_model(name).unwrap();
            assert_eq!(model.provider, provider, "{name}");
            let saved_name = model_name(&model);
            assert_eq!(saved_name, saved, "{name}");
            // The saved name names the same model.
            let again = resolve_model(&saved_name).unwrap();
            assert_eq!((again.provider, again.id), (model.provider, model.id));
        }
        // Claude Code models keep the registry's limits and prices.
        let cc = resolve_model("cc:opus").unwrap();
        let opus = resolve_model("opus").unwrap();
        assert_eq!(cc.max_input_tokens, opus.max_input_tokens);
    }

    #[test]
    fn a_name_that_names_no_model_says_what_was_meant() {
        let error = resolve_model("opsu").unwrap_err();
        assert!(error.contains("Did you mean opus?"), "{error}");
        assert!(error.contains("cc:<model>"), "{error}");
        assert!(error.contains("openrouter:<slug>"), "{error}");
        let error = resolve_model("anthropic/claude-opus-5.5").unwrap_err();
        assert!(
            error.contains("openrouter:anthropic/claude-opus-5.5"),
            "{error}"
        );
        let error = resolve_model("claude-opus-5.5").unwrap_err();
        assert!(error.contains("Did you mean claude-opus-5-5?"), "{error}");
        let error = resolve_model("cc:sol").unwrap_err();
        assert!(
            error.contains("gpt-6.1-sol isn't one (OpenAI model)"),
            "{error}"
        );
        let error = resolve_model("cc:opsu").unwrap_err();
        assert!(error.contains("Did you mean cc:opus?"), "{error}");
        assert!(resolve_model("openrouter:").is_err());
        assert!(resolve_model("no-such-model-at-all").is_err());
    }

    /// The text of the models' thinking is asked for where a provider
    /// needs asking, for the views to show, unless the session hides it:
    /// Claude by API key for its summarized thinking (Claude Code passes it
    /// on by itself), OpenAI and OpenRouter for reasoning summaries, Gemini
    /// for its thoughts.
    #[test]
    fn the_thinking_text_is_asked_for_unless_hidden() {
        let config = |name: &str, thinking: bool| {
            generation(
                &resolve_model(name).unwrap(),
                Effort::High,
                false,
                false,
                thinking,
            )
            .unwrap()
        };
        assert_eq!(
            config("opus", true).anthropic_summarized_thinking,
            Some(true)
        );
        assert_eq!(config("opus", false).anthropic_summarized_thinking, None);
        assert_eq!(config("cc:opus", true).anthropic_summarized_thinking, None);
        for thinking in [true, false] {
            assert!(matches!(
                config("sol", thinking).reasoning,
                Some(ReasoningConfig::OpenAI { include_summaries, .. }) if include_summaries == thinking
            ));
            assert!(matches!(
                config("gemini", thinking).reasoning,
                Some(ReasoningConfig::Gemini { include_summaries, .. }) if include_summaries == thinking
            ));
            let summary = match config("openrouter:anthropic/claude-opus-5.5", thinking).reasoning {
                Some(ReasoningConfig::OpenRouter(reasoning)) => reasoning.summary,
                _ => panic!("OpenRouter's reasoning"),
            };
            assert_eq!(summary.is_some(), thinking);
        }
    }

    #[test]
    fn each_provider_gets_its_own_reasoning_fast_mode_and_cache() {
        let config = |name: &str, effort: Effort, fast: bool, cache_1h: bool| {
            generation(&resolve_model(name).unwrap(), effort, fast, cache_1h, false)
        };
        // Claude, by API key or in Claude Code: adaptive thinking at the
        // effort, fast mode on Opus, the 1-hour cache by API key only.
        for name in ["opus", "cc:opus"] {
            let opus = config(name, Effort::Xhigh, true, true).unwrap();
            assert!(matches!(
                opus.reasoning,
                Some(ReasoningConfig::AnthropicAdaptive {
                    effort: Some(arko_ai::AnthropicReasoningEffort::XHigh)
                })
            ));
            assert_eq!(opus.anthropic_fast_mode, Some(true));
            assert!(
                config(
                    name.replace("opus", "fable").as_str(),
                    Effort::High,
                    true,
                    false
                )
                .is_err()
            );
        }
        assert_eq!(
            config("opus", Effort::High, false, true)
                .unwrap()
                .claude_cache_ttl(),
            Some(AnthropicCacheTtl::OneHour)
        );
        // Claude Code passes the lifetime on, and chooses its own without one.
        assert_eq!(
            config("cc:opus", Effort::High, false, true)
                .unwrap()
                .claude_cache_ttl(),
            Some(AnthropicCacheTtl::OneHour)
        );
        assert_eq!(
            config("cc:opus", Effort::High, false, false)
                .unwrap()
                .claude_cache_ttl(),
            None
        );
        // OpenAI: its reasoning and Fast tier.
        let sol = config("sol", Effort::Max, true, true).unwrap();
        assert!(matches!(
            sol.reasoning,
            Some(ReasoningConfig::OpenAI { .. })
        ));
        assert_eq!(sol.openai_service_tier, Some(OpenAIServiceTier::Fast));
        // The long cache is OpenAI's 12 hours, which the framework asks for
        // with an API key.
        assert_eq!(sol.long_prompt_cache, Some(true));
        // Gemini: thinking levels, no fast mode.
        let level = |name: &str, effort: Effort| match config(name, effort, false, false)
            .unwrap()
            .reasoning
        {
            Some(ReasoningConfig::Gemini { thinking_level, .. }) => Some(thinking_level),
            _ => None,
        };
        assert_eq!(
            level("gemini", Effort::Xhigh),
            Some(GeminiThinkingLevel::High)
        );
        assert_eq!(
            level("gemini", Effort::Medium),
            Some(GeminiThinkingLevel::Medium)
        );
        assert_eq!(level("flash", Effort::Low), Some(GeminiThinkingLevel::Low));
        assert_eq!(
            level("gemini-3-pro-preview", Effort::Medium),
            Some(GeminiThinkingLevel::High),
            "Gemini 3 Pro has no medium"
        );
        assert_eq!(
            level("gemini-2.0-flash", Effort::High),
            None,
            "it doesn't think"
        );
        let error = config("gemini", Effort::High, true, false).unwrap_err();
        assert!(error.contains("no fast mode"), "{error}");
        // OpenRouter: its own reasoning object; the 1-hour cache on Claude.
        let claude = config(
            "openrouter:anthropic/claude-opus-5.5",
            Effort::Xhigh,
            false,
            true,
        )
        .unwrap();
        assert!(matches!(
            &claude.reasoning,
            Some(ReasoningConfig::OpenRouter(reasoning)) if reasoning.effort == Some(ReasoningEffort::XHigh)
        ));
        assert_eq!(claude.claude_cache_ttl(), Some(AnthropicCacheTtl::OneHour));
        let other = config("openrouter:deepseek/deepseek-v4", Effort::High, false, true).unwrap();
        assert_eq!(other.anthropic_cache_ttl, None);
        let error =
            config("openrouter:deepseek/deepseek-v4", Effort::High, true, false).unwrap_err();
        assert!(error.contains(":nitro"), "{error}");
    }

    #[test]
    fn handovers_follow_the_model_s_limit_and_a_fallback_when_unknown() {
        assert_eq!(
            default_handover(&resolve_model("opus").unwrap(), None),
            950_000
        );
        assert_eq!(
            default_handover(&resolve_model("cc:fable").unwrap(), None),
            950_000
        );
        assert_eq!(
            default_handover(&resolve_model("sol").unwrap(), None),
            200_000
        );
        assert_eq!(
            default_handover(&resolve_model("gemini").unwrap(), None),
            200_000
        );
        // An OpenRouter model without its catalog has no known limit.
        assert_eq!(
            default_handover(&resolve_model("openrouter:x/y").unwrap(), None),
            UNKNOWN_LIMIT_HANDOVER
        );
        let mut small = resolve_model("openrouter:anthropic/claude-opus-5.5").unwrap();
        small.max_input_tokens = 200_000;
        assert_eq!(default_handover(&small, None), 160_000);
        small.max_input_tokens = 1_000_000;
        assert_eq!(
            default_handover(&small, None),
            500_000,
            "Claude on OpenRouter"
        );
    }

    fn settings(main: ModelSettings, worker: ModelSettings) -> Settings {
        Settings {
            handover_at: main.handover_at.unwrap_or(200_000),
            worker,
            max_workers: 4,
            ..Settings::for_test(std::path::Path::new("/work"), &main)
        }
    }

    const CATALOG: &str = r#"{"data": [
        {"id": "anthropic/claude-opus-5.5", "context_length": 1000000,
         "supported_parameters": ["tools", "reasoning"],
         "pricing": {"prompt": "0.000005", "completion": "0.000025"}},
        {"id": "meta/small-model", "context_length": 128000,
         "supported_parameters": ["tools"],
         "pricing": {"prompt": "0.0000001", "completion": "0.0000002"}},
        {"id": "acme/no-tools", "context_length": 32000,
         "supported_parameters": ["temperature"],
         "pricing": {"prompt": "0", "completion": "0"}}
    ]}"#;

    #[test]
    fn openrouter_models_are_checked_and_sized_from_the_catalog() {
        let catalog = catalog(CATALOG);
        let mut session = settings(
            model("openrouter:anthropic/claude-opus-5.5"),
            model("openrouter:meta/small-model"),
        );
        let notes = apply_catalog(&mut session, Ok(&catalog), false, None).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            session.main.handover_at,
            Some(500_000),
            "Claude on OpenRouter"
        );
        assert_eq!(session.worker.handover_at, Some(102_400), "80% of 128k");
        assert_eq!(session.handover_at, 500_000);

        // --handover-at stands, up to 95% of what the model accepts.
        let mut session = settings(model("openrouter:meta/small-model"), model("opus"));
        session.main.handover_at = Some(100_000);
        apply_catalog(&mut session, Ok(&catalog), true, None).unwrap();
        assert_eq!(session.main.handover_at, Some(100_000));
        session.main.handover_at = Some(300_000);
        let notes = apply_catalog(&mut session, Ok(&catalog), true, None).unwrap();
        assert_eq!(session.main.handover_at, Some(121_600), "95% of 128k");
        assert_eq!(
            notes,
            [
                "openrouter:meta/small-model hands over at 121,600 tokens, the largest size Arko allows for its model."
            ]
        );

        // A slug the catalog doesn't list, or a model without tools, is
        // refused before the session starts.
        let mut session = settings(model("openrouter:acme/typo"), model("opus"));
        let error = apply_catalog(&mut session, Ok(&catalog), false, None).unwrap_err();
        assert!(
            error.contains("isn't in OpenRouter's model catalog"),
            "{error}"
        );
        let mut session = settings(model("openrouter:acme/no-tools"), model("opus"));
        let error = apply_catalog(&mut session, Ok(&catalog), false, None).unwrap_err();
        assert!(error.contains("doesn't take tool calls"), "{error}");

        // Offline: the session starts, at the fallback, and says so.
        let mut session = settings(model("openrouter:meta/small-model"), model("opus"));
        let offline = "connection refused".to_string();
        let notes = apply_catalog(&mut session, Err(&offline), false, None).unwrap();
        assert_eq!(session.main.handover_at, Some(UNKNOWN_LIMIT_HANDOVER));
        assert!(
            notes[0].contains("Could not read OpenRouter's model catalog"),
            "{notes:?}"
        );
        assert!(notes[0].contains("100k tokens"), "{notes:?}");

        // Sessions without OpenRouter models aren't touched.
        let mut session = settings(model("opus"), model("cc:fable"));
        assert!(
            apply_catalog(&mut session, Ok(&catalog), false, None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(session.main.handover_at, Some(950_000));
    }

    /// A session reaches OpenRouter only with the settings the file holds
    /// read. Without them, one that uses OpenRouter doesn't start, and one
    /// that doesn't gets no OpenRouter client, which a worker's model or a
    /// profile could use without the restrictions.
    #[test]
    fn an_unreadable_openrouter_file_keeps_openrouter_off() {
        let root = tempfile::tempdir().unwrap();
        let vars = |name: &str| match name {
            "ANTHROPIC_API_KEY" => Some("sk-ant-test".to_string()),
            "OPENROUTER_API_KEY" => Some("sk-or-test".to_string()),
            _ => None,
        };
        let ready = || {
            Ok(claude_code::Ready {
                path: "/bin/claude".into(),
                method: None,
                plan: None,
            })
        };
        let build = |session: &Settings| client_with(session, root.path(), vars, vec![], ready);
        let opus = settings(model("opus"), model("opus"));
        let on_openrouter = settings(model("openrouter:anthropic/claude-opus-5.5"), model("opus"));
        // Readable (or missing): OpenRouter is there.
        assert!(
            build(&opus)
                .ok()
                .is_some_and(|client| client.has_provider(&Provider::OpenRouter))
        );
        std::fs::write(root.path().join(crate::openrouter::FILE), "{ oops").unwrap();
        // A session on Opus has no OpenRouter, and one on OpenRouter says why.
        let client = build(&opus).ok().expect("a session on Opus starts");
        assert!(!client.has_provider(&Provider::OpenRouter));
        assert!(client.has_provider(&Provider::Anthropic));
        match build(&on_openrouter) {
            Err(Failure::Other(error)) => {
                assert!(error.contains("openrouter.json can't be read"), "{error}")
            }
            _ => panic!("a session on OpenRouter needs the file read"),
        }
        // A file emptied or blanked, as a truncated write leaves it, is no
        // better: it isn't "no settings".
        for text in ["", " \n "] {
            std::fs::write(root.path().join(crate::openrouter::FILE), text).unwrap();
            let client = build(&opus).ok().expect("a session on Opus starts");
            assert!(!client.has_provider(&Provider::OpenRouter), "{text:?}");
            assert!(
                matches!(build(&on_openrouter), Err(Failure::Other(_))),
                "{text:?}"
            );
        }
        // Fixed: the client carries what the file says.
        std::fs::write(
            root.path().join(crate::openrouter::FILE),
            r#"{"provider": {"zdr": true}}"#,
        )
        .unwrap();
        assert!(
            build(&opus)
                .ok()
                .is_some_and(|client| client.has_provider(&Provider::OpenRouter))
        );
        assert!(build(&on_openrouter).is_ok());
    }

    /// The catalog is read from OpenRouter's public endpoint: a local
    /// server stands in for it.
    #[tokio::test]
    async fn the_catalog_is_read_from_openrouter_and_offline_is_no_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let n = socket.read(&mut request).await.unwrap();
            let head = String::from_utf8_lossy(&request[..n]).to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{CATALOG}",
                CATALOG.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            head
        });
        let temp = tempfile::tempdir().unwrap();
        let mut session = settings(model("openrouter:meta/small-model"), model("opus"));
        let notes = check_openrouter(&mut session, false, temp.path(), Some(&url))
            .await
            .unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(session.main.handover_at, Some(102_400));
        let head = server.await.unwrap();
        assert!(
            head.starts_with("GET /api/v1/models?supported_parameters=tools "),
            "{head}"
        );
        assert!(
            !head.to_ascii_lowercase().contains("authorization"),
            "no key is sent: {head}"
        );

        // Nothing listens here.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/v1", closed.local_addr().unwrap());
        drop(closed);
        let mut session = settings(model("openrouter:meta/small-model"), model("opus"));
        let notes = check_openrouter(&mut session, false, temp.path(), Some(&url))
            .await
            .unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("(it couldn't be reached)"), "{notes:?}");
    }

    #[test]
    fn plan_usage_names_its_plan() {
        let window = |percent: f64, minutes: u64| arko_ai::PlanUsageWindow {
            used_percent: percent,
            window_minutes: Some(minutes),
            ..Default::default()
        };
        let claude = PlanUsage {
            plan: Some(SubscriptionPlan::ClaudeCode),
            primary: Some(window(12.4, 300)),
            secondary: Some(window(3.0, 10_080)),
            ..Default::default()
        };
        assert_eq!(
            plan_usage_text(&claude).as_deref(),
            Some("Claude plan 5 h 12%, 7 d 3%")
        );
        let chatgpt = PlanUsage {
            plan: Some(SubscriptionPlan::ChatGpt),
            secondary: Some(window(40.0, 10_080)),
            ..Default::default()
        };
        assert_eq!(
            plan_usage_text(&chatgpt).as_deref(),
            Some("ChatGPT plan 7 d 40%")
        );
        assert_eq!(plan_usage_text(&PlanUsage::default()), None);
        // The same windows as data, for the status.
        let windows = plan_windows(&claude);
        assert_eq!(
            windows
                .iter()
                .map(|window| (window.label.as_str(), window.used_percent))
                .collect::<Vec<_>>(),
            [("5 h", 12.4), ("7 d", 3.0)]
        );
        assert!(plan_windows(&PlanUsage::default()).is_empty());
    }

    /// A Claude account's extra usage shows beside the plan windows when it is
    /// on, with its amounts in the currency's decimal places, and says nothing
    /// when it is off or not known.
    #[test]
    fn extra_usage_shows_beside_the_claude_windows_when_it_is_on() {
        let line = |extra: Option<arko_ai::ExtraUsage>| {
            let window = |percent: f64, minutes: u64| arko_ai::PlanUsageWindow {
                used_percent: percent,
                window_minutes: Some(minutes),
                ..Default::default()
            };
            plan_usage_text(&PlanUsage {
                plan: Some(SubscriptionPlan::ClaudeCode),
                primary: Some(window(12.0, 300)),
                secondary: Some(window(71.0, 10_080)),
                extra_usage: extra,
                ..Default::default()
            })
            .unwrap()
        };
        let extra = |enabled, in_use| arko_ai::ExtraUsage {
            enabled,
            in_use,
            ..Default::default()
        };
        let amounts = |used, limit, currency: &str, places| arko_ai::ExtraUsage {
            used_credits: Some(used),
            monthly_limit: limit,
            currency: Some(currency.into()),
            decimal_places: Some(places),
            ..extra(Some(true), true)
        };
        let windows = "Claude plan 5 h 12%, 7 d 71%";
        assert_eq!(line(None), windows);
        assert_eq!(line(Some(extra(Some(false), false))), windows);
        assert_eq!(line(Some(extra(None, false))), windows);
        assert_eq!(
            line(Some(extra(Some(true), false))),
            format!("{windows}, extra usage on")
        );
        assert_eq!(
            line(Some(extra(Some(true), true))),
            format!("{windows}, extra usage in use")
        );
        assert_eq!(
            line(Some(amounts(27_140.0, Some(50_000.0), "USD", 2))),
            format!("{windows}, extra usage $271.40 of $500.00")
        );
        assert_eq!(
            line(Some(amounts(27_140.0, None, "USD", 2))),
            format!("{windows}, extra usage $271.40 used")
        );
        // Another currency, with no decimal places, is named after the amounts.
        assert_eq!(
            line(Some(amounts(3_500.0, Some(10_000.0), "JPY", 0))),
            format!("{windows}, extra usage 3500 of 10000 JPY")
        );
        // The status line labels the amounts.
        assert_eq!(
            extra_usage_figure(&amounts(27_140.0, Some(50_000.0), "USD", 2)).as_deref(),
            Some("extra usage: $271.40 of $500.00")
        );
        assert_eq!(
            extra_usage_figure(&amounts(27_140.0, None, "USD", 2)).as_deref(),
            Some("extra usage: $271.40 used")
        );
        assert_eq!(
            extra_usage_figure(&extra(Some(true), false)).as_deref(),
            Some("extra usage on")
        );
        assert_eq!(extra_usage_figure(&extra(Some(false), false)), None);
    }

    /// The status line's credits have two decimals at most, keep a balance
    /// that isn't a number as the backend wrote it, and stay words when
    /// they are unlimited or have no balance.
    #[test]
    fn the_status_line_rounds_the_credits_to_two_decimals() {
        let credits = |balance: Option<&str>| arko_ai::PlanCredits {
            has_credits: true,
            unlimited: false,
            balance: balance.map(str::to_string),
        };
        let figure = |credits| credits_figure(&credits);
        assert_eq!(
            figure(credits(Some("12.503417"))).as_deref(),
            Some("credits: 12.50")
        );
        assert_eq!(
            figure(credits(Some(" 0.996 "))).as_deref(),
            Some("credits: 1.00")
        );
        assert_eq!(
            figure(credits(Some("12"))).as_deref(),
            Some("credits: 12.00")
        );
        assert_eq!(
            figure(credits(Some("about 12"))).as_deref(),
            Some("credits: about 12")
        );
        assert_eq!(
            figure(credits(Some("NaN"))).as_deref(),
            Some("credits: NaN")
        );
        assert_eq!(figure(credits(Some(""))).as_deref(), Some("has credits"));
        assert_eq!(figure(credits(None)).as_deref(), Some("has credits"));
        let unlimited = arko_ai::PlanCredits {
            unlimited: true,
            ..credits(Some("5"))
        };
        assert_eq!(figure(unlimited).as_deref(), Some("unlimited credits"));
        let none = arko_ai::PlanCredits {
            has_credits: false,
            ..credits(Some("0"))
        };
        assert_eq!(figure(none), None);
        // The line of `/cost` and the page keeps the balance as it came.
        assert_eq!(
            credits_text(&credits(Some("12.503417"))).as_deref(),
            Some("credits left 12.503417")
        );
    }

    #[test]
    fn a_plan_window_is_named_by_its_length() {
        for (minutes, name) in [
            (300, "5 h"),
            (10_080, "7 d"),
            (43_200, "30 d"),
            (90, "90 min"),
        ] {
            assert_eq!(window_name(Some(minutes), "?"), name, "{minutes}");
        }
        assert_eq!(window_name(None, "5 h"), "5 h");
        // The status line and the status give a long window the same name.
        let chatgpt = PlanUsage {
            plan: Some(SubscriptionPlan::ChatGpt),
            secondary: Some(arko_ai::PlanUsageWindow {
                used_percent: 43.0,
                window_minutes: Some(43_200),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            plan_usage_text(&chatgpt).as_deref(),
            Some("ChatGPT plan 30 d 43%")
        );
        assert_eq!(plan_windows(&chatgpt)[0].label, "30 d");
    }

    /// What `client_with` needs when nothing is set, Claude Code is
    /// installed, and the ChatGPT login is a file that isn't there.
    fn need(session: &Settings, vars: &[(&str, &str)]) -> Option<Need> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        let lookup = |name: &str| {
            vars.iter()
                .find(|(set, _)| set == name)
                .map(|(_, value)| value.clone())
        };
        let ready = || {
            Ok(claude_code::Ready {
                path: "/bin/claude".into(),
                method: Some("claude.ai".into()),
                plan: Some("max".into()),
            })
        };
        match client_with(
            session,
            no_root(),
            lookup,
            vec!["/no/such/auth.json".into()],
            ready,
        ) {
            Ok(_) => None,
            Err(Failure::Need(need)) => Some(need),
            Err(Failure::Other(error)) => panic!("{error}"),
        }
    }

    /// A session asks for exactly the credential its models lack, and
    /// reaches OpenAI models only the way it chose.
    #[tokio::test]
    async fn each_model_asks_for_its_own_credential() {
        let gemini = settings(model("gemini"), model("gemini"));
        assert_eq!(
            need(&gemini, &[("ANTHROPIC_API_KEY", "sk-ant-1")]),
            Some(Need::GeminiKey {
                model: "gemini-3.1-pro-preview".into()
            })
        );
        assert_eq!(need(&gemini, &[("GEMINI_API_KEY", "AIza1")]), None);
        assert_eq!(need(&gemini, &[("GOOGLE_API_KEY", "AIza1")]), None);

        let openrouter = settings(model("opus"), model("openrouter:x/y"));
        assert_eq!(
            need(&openrouter, &[("ANTHROPIC_API_KEY", "sk-ant-1")]),
            Some(Need::OpenRouterKey {
                model: "openrouter:x/y".into()
            })
        );
        assert_eq!(
            need(&openrouter, &[]),
            Some(Need::AnthropicKey {
                model: "claude-opus-5-5".into()
            }),
            "the main model's first"
        );

        // The API key, never the ChatGPT login, and the other way round.
        let mut sol = settings(model("sol"), model("sol"));
        sol.openai_auth = Some(OpenAiAuth::ApiKey);
        let login = [
            ("OPENAI_OAUTH_ACCESS_TOKEN", "access"),
            ("OPENAI_OAUTH_ACCOUNT_ID", "account"),
        ];
        assert_eq!(
            need(&sol, &login),
            Some(Need::OpenAiKey {
                model: "gpt-6.1-sol".into()
            })
        );
        let both = [login[0], login[1], ("OPENAI_API_KEY", "sk-proj-1")];
        let lookup = |name: &str| {
            both.iter()
                .find(|(set, _)| *set == name)
                .map(|(_, value)| value.to_string())
        };
        let client = client_with(&sol, no_root(), lookup, vec![], || {
            Err(claude_code::Problem::Missing)
        })
        .ok()
        .unwrap();
        assert!(!client.is_openai_chatgpt_oauth().await, "the API key");
        sol.openai_auth = Some(OpenAiAuth::ChatGpt);
        let client = client_with(&sol, no_root(), lookup, vec![], || {
            Err(claude_code::Problem::Missing)
        })
        .ok()
        .unwrap();
        assert!(client.is_openai_chatgpt_oauth().await, "the login");
        assert!(matches!(
            need(&sol, &[("OPENAI_API_KEY", "sk-proj-1")]),
            Some(Need::ChatGptLogin { .. })
        ));
        // Sessions from before the choice used the login.
        sol.openai_auth = None;
        assert!(matches!(
            need(&sol, &[("OPENAI_API_KEY", "sk-proj-1")]),
            Some(Need::ChatGptLogin { .. })
        ));

        // Claude Code takes no key, but must be installed and signed in.
        let cc = settings(model("cc:opus"), model("cc:fable"));
        assert_eq!(need(&cc, &[]), None);
        let missing = client_with(
            &cc,
            no_root(),
            |_| None,
            vec![],
            || Err(claude_code::Problem::Missing),
        );
        assert!(matches!(
            missing,
            Err(Failure::Need(Need::ClaudeCode { problem: claude_code::Problem::Missing, ref model })) if model == "cc:claude-opus-5-5"
        ));
    }

    /// A session on the ChatGPT login signs in with every login of the
    /// list, in its order, leaving out one that doesn't work.
    #[tokio::test]
    async fn the_session_signs_in_with_every_chatgpt_login_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let login = |folder: &str, account: &str| {
            let path = temp.path().join(folder).join("auth.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                format!(r#"{{"tokens": {{"access_token": "access", "account_id": "{account}"}}}}"#),
            )
            .unwrap();
            path
        };
        let (first, second) = (login("first", "account-1"), login("second", "account-2"));
        let mut sol = settings(model("sol"), model("sol"));
        sol.openai_auth = Some(OpenAiAuth::ChatGpt);
        let labels = |paths: Vec<PathBuf>| {
            let client = client_with(
                &sol,
                no_root(),
                |_| None,
                paths,
                || Err(claude_code::Problem::Missing),
            )
            .ok()
            .unwrap();
            client
                .chatgpt_accounts()
                .unwrap()
                .accounts()
                .iter()
                .map(|account| account.label().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(vec![first.clone(), second.clone()]),
            ["first", "second"]
        );
        assert_eq!(
            labels(vec![second.clone(), first.clone()]),
            ["second", "first"]
        );
        // One that is missing is left out while another works.
        let missing = temp.path().join("none").join("auth.json");
        assert_eq!(labels(vec![missing, first]), ["first"]);
    }

    #[test]
    fn an_openai_api_key_session_refuses_fast_mode_where_openai_has_none() {
        let mut fast = model("gpt-5.4-nano-2026-03-17");
        fast.fast = true;
        let mut session = settings(fast, model("opus"));
        assert!(
            check_openai_choices(&session).is_ok(),
            "ChatGPT: arko checks"
        );
        session.openai_auth = Some(OpenAiAuth::ApiKey);
        let error = check_openai_choices(&session).unwrap_err();
        assert!(error.contains("no Fast mode with an API key"), "{error}");
        let mut sol = model("sol");
        sol.fast = true;
        session.main = sol;
        assert!(check_openai_choices(&session).is_ok());
        let mut terra = model("terra");
        terra.fast = true;
        session.main = terra;
        assert!(check_openai_choices(&session).is_ok());
    }

    fn last(main: &str, worker: Option<&str>) -> Option<LastModels> {
        Some(LastModels {
            main: main.into(),
            worker: worker.map(String::from),
            efforts: Default::default(),
        })
    }

    /// The last session's models come first while their credentials are
    /// there; else the credentials decide, and the reason says why.
    #[test]
    fn the_last_session_s_models_are_the_default_while_they_can_run() {
        let have_all = |_: &str| None;
        let by_key = Some(("claude-opus-5-5", "ANTHROPIC_API_KEY is set"));
        // No last session: the credentials decide.
        let default = choose_default(None, have_all, by_key).unwrap();
        assert_eq!(default.main, "claude-opus-5-5");
        assert_eq!(default.reason, None);
        assert_eq!(choose_default(None, have_all, None), None);

        // The last models, workers included.
        let default = choose_default(
            last("cc:claude-opus-5-5", Some("gemini-3.8-flash")),
            have_all,
            by_key,
        )
        .unwrap();
        assert_eq!(default.main, "cc:claude-opus-5-5");
        assert_eq!(default.worker.as_deref(), Some("gemini-3.8-flash"));
        assert_eq!(default.reason, None);

        // The workers' credential is gone: they run on the main model.
        let no_gemini = |name: &str| {
            name.starts_with("gemini")
                .then(|| "GEMINI_API_KEY".to_string())
        };
        let default = choose_default(
            last("cc:claude-opus-5-5", Some("gemini-3.8-flash")),
            no_gemini,
            by_key,
        )
        .unwrap();
        assert_eq!(default.worker, None);
        assert_eq!(
            default.reason.as_deref(),
            Some(
                "Your last session's workers ran gemini-3.8-flash, which needs GEMINI_API_KEY, so they run cc:claude-opus-5-5 now."
            )
        );

        // The main model's credential is gone: the credentials decide, and
        // the note says why.
        let default = choose_default(last("gemini-3.8-flash", None), no_gemini, by_key).unwrap();
        assert_eq!(default.main, "claude-opus-5-5");
        assert_eq!(
            default.reason.as_deref(),
            Some("Your last session ran gemini-3.8-flash, which needs GEMINI_API_KEY.")
        );
        // ...and without any other credential it stays, so the setup screen
        // asks for exactly what it needs.
        let default = choose_default(last("gemini-3.8-flash", None), no_gemini, None).unwrap();
        assert_eq!(default.main, "gemini-3.8-flash");
        assert_eq!(default.reason, None);
    }

    /// Claude Code, signed in, comes before an Anthropic API key; one not
    /// signed in comes last, after every key.
    #[test]
    fn the_claude_plan_comes_first_and_a_signed_out_claude_code_last() {
        let first = |have: Credentials| first_by_credentials(have).map(|(model, _)| model);
        let key = Credentials {
            anthropic: true,
            ..Credentials::default()
        };
        assert_eq!(first(key), Some("claude-opus-5-5"));
        assert_eq!(
            first(Credentials {
                claude_code: Some(true),
                ..key
            }),
            Some("cc:claude-opus-5-5")
        );
        assert_eq!(
            first(Credentials {
                claude_code: Some(false),
                ..key
            }),
            Some("claude-opus-5-5")
        );
        assert_eq!(
            first(Credentials {
                claude_code: Some(false),
                ..Credentials::default()
            }),
            Some("cc:claude-opus-5-5")
        );
        assert_eq!(
            first(Credentials {
                chatgpt: true,
                openai: true,
                ..Credentials::default()
            }),
            Some("gpt-6.1-sol")
        );
        assert_eq!(first(Credentials::default()), None);
    }

    #[test]
    fn the_last_session_s_models_are_kept_in_arko_s_folder() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(LastModels::load(root.path()), None);
        let mut session = settings(model("cc:opus"), model("cc:opus"));
        LastModels::remember(root.path(), &session).unwrap();
        let models = |root: &Path| {
            let last = LastModels::load(root);
            last.map(|last| (last.main, last.worker))
        };
        assert_eq!(
            models(root.path()),
            Some(("cc:claude-opus-5-5".to_string(), None))
        );
        session.worker = model("flash");
        LastModels::remember(root.path(), &session).unwrap();
        assert_eq!(
            models(root.path()),
            Some((
                "cc:claude-opus-5-5".to_string(),
                Some("gemini-3.8-flash".to_string())
            ))
        );
        std::fs::write(root.path().join(LAST_MODELS_FILE), "not json").unwrap();
        assert_eq!(
            LastModels::load(root.path()),
            None,
            "a damaged file is no default"
        );
    }

    /// The effort each model last ran at is kept with the last models,
    /// for the main agent's model, the workers' when it differs, and a model
    /// switched to later. Only an effort the model still takes comes back.
    #[test]
    fn the_effort_each_model_last_ran_at_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        assert_eq!(LastModels::effort_of(root, "opus"), None);
        let mut session = settings(model("opus"), model("flash"));
        session.main.effort = Effort::High;
        session.worker.effort = Effort::Low;
        LastModels::remember(root, &session).unwrap();
        // Any name the model goes by finds it.
        assert_eq!(LastModels::effort_of(root, "opus"), Some(Effort::High));
        assert_eq!(
            LastModels::effort_of(root, "claude-opus-5-5"),
            Some(Effort::High)
        );
        assert_eq!(LastModels::effort_of(root, "flash"), Some(Effort::Low));
        assert_eq!(LastModels::effort_of(root, "sol"), None, "never run");
        // A switch keeps the effort chosen, and the others stay.
        let sol = resolve_model("sol").unwrap();
        LastModels::remember_effort(root, &sol, Effort::Max).unwrap();
        assert_eq!(LastModels::effort_of(root, "sol"), Some(Effort::Max));
        assert_eq!(LastModels::effort_of(root, "opus"), Some(Effort::High));
        // The next session's models keep what was kept for the others.
        let mut next = settings(model("luna"), model("luna"));
        next.main.effort = Effort::Medium;
        LastModels::remember(root, &next).unwrap();
        assert_eq!(LastModels::effort_of(root, "luna"), Some(Effort::Medium));
        assert_eq!(LastModels::effort_of(root, "sol"), Some(Effort::Max));
        // An effort a model doesn't take (or one made up) isn't offered.
        let file = std::fs::read_to_string(root.join(LAST_MODELS_FILE)).unwrap();
        let file = file
            .replace("\"gpt-6.1-sol\": \"max\"", "\"gpt-6.1-sol\": \"extreme\"")
            .replace(
                "\"gemini-3.8-flash\": \"low\"",
                "\"gemini-3.8-flash\": \"max\"",
            );
        std::fs::write(root.join(LAST_MODELS_FILE), file).unwrap();
        assert_eq!(LastModels::effort_of(root, "sol"), None);
        assert_eq!(
            LastModels::effort_of(root, "flash"),
            providers_takes_max("flash")
        );
        // Before any session kept its models, a switch keeps nothing.
        let empty = tempfile::tempdir().unwrap();
        LastModels::remember_effort(empty.path(), &sol, Effort::High).unwrap();
        assert_eq!(LastModels::load(empty.path()), None);
        // An older file without efforts still loads.
        std::fs::write(root.join(LAST_MODELS_FILE), r#"{"main": "opus"}"#).unwrap();
        assert_eq!(LastModels::effort_of(root, "opus"), None);
        assert_eq!(LastModels::load(root).unwrap().main, "opus");
    }

    /// `max` as `model` takes it, or none.
    fn providers_takes_max(model: &str) -> Option<Effort> {
        let model = resolve_model(model).unwrap();
        efforts(&model)
            .contains(&Effort::Max)
            .then_some(Effort::Max)
    }

    /// A ledger entry of `agent`'s: `api` dollars at API prices, of which
    /// `billed` were billed (0 on a plan).
    fn call(
        agent: &str,
        input: usize,
        output: usize,
        api: f64,
        billed: f64,
    ) -> arko_workflows::budget::CostEntry {
        let usage = UsageInfo {
            input_tokens: input,
            output_tokens: output,
            total_cost: api,
            billing: if billed == 0.0 && api > 0.0 {
                BillingKind::Subscription
            } else {
                BillingKind::Metered
            },
            billed_cost: Some(billed),
            ..UsageInfo::default()
        };
        arko_workflows::budget::CostEntry {
            amount: billed,
            source: CostSource::Agent {
                agent_name: agent.into(),
                usage: Box::new(usage),
            },
            timestamp: chrono::Utc::now(),
            metadata: None,
        }
    }

    fn report(
        session: &Settings,
        entries: Vec<arko_workflows::budget::CostEntry>,
    ) -> Vec<(bool, String)> {
        report_with_runs(session, entries, &[])
    }

    /// [`report`] with the main agent's runs `runs`.
    fn report_with_runs(
        session: &Settings,
        entries: Vec<arko_workflows::budget::CostEntry>,
        runs: &[RunCost],
    ) -> Vec<(bool, String)> {
        let plans = std::collections::BTreeMap::from([
            (
                "Claude plan".to_string(),
                "Claude plan 5 h 12%, 7 d 3%".to_string(),
            ),
            (
                "ChatGPT plan".to_string(),
                "ChatGPT plan 7 d 40%".to_string(),
            ),
        ]);
        cost_report(&CostLedger { entries }, session, runs, &plans)
            .into_iter()
            .map(|line| (line.detail, line.text))
            .collect()
    }

    /// A run's line says its tokens (the cached part of the input too) and,
    /// per token, what it billed, or on a plan, that it ran there and what
    /// it would cost at API prices. A run that reported no tokens has none.
    #[test]
    fn a_runs_usage_reads_in_a_line_as_the_cost_does() {
        let usage = |input, cached, output, cost: f64, billing| UsageInfo {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            total_cost: cost,
            billing,
            billed_cost: Some(if billing == BillingKind::Subscription {
                0.0
            } else {
                cost
            }),
            ..UsageInfo::default()
        };
        let metered = usage(55_000, Some(20_000), 2_500, 0.3, BillingKind::Metered);
        assert_eq!(
            run_usage_line("r2", &metered, &Provider::Anthropic).as_deref(),
            Some("r2 used 55k input (20k cached) and 2k output tokens, $0.30.")
        );
        let plan = usage(
            120_000,
            Some(100_000),
            8_000,
            3.0,
            BillingKind::Subscription,
        );
        for (provider, name) in [
            (Provider::ClaudeCode, "Claude"),
            (Provider::OpenAI, "ChatGPT"),
        ] {
            assert_eq!(
                run_usage_line("r1", &plan, &provider),
                Some(format!(
                    "r1 ran on the {name} plan, so nothing was billed per token. It used 120k input (100k cached) and 8k output tokens, ≈ $3.00 at API prices."
                )),
            );
        }
        // A short run's tokens are told whole.
        let short = usage(10, None, 5, 0.0, BillingKind::Metered);
        assert_eq!(
            run_usage_line("r3", &short, &Provider::OpenAI).as_deref(),
            Some("r3 used 10 input and 5 output tokens, $0.00.")
        );
        assert_eq!(
            run_usage_line("r4", &UsageInfo::default(), &Provider::OpenAI),
            None
        );
    }

    /// `/cost` on a plan shows what was billed, the API price and the
    /// plan's windows; per token, what was billed; mixed, each part.
    #[test]
    fn the_cost_shows_api_prices_and_plan_usage_where_a_plan_pays() {
        use crate::agents::{MAIN, WORKER};
        // Claude Code, main agent and workers.
        let cc = settings(model("cc:opus"), model("cc:opus"));
        let entries = vec![
            call(MAIN, 120_000, 8_000, 3.0, 0.0),
            call(WORKER, 30_000, 2_000, 0.4, 0.0),
        ];
        assert_eq!(
            report(&cc, entries.clone()),
            [
                (
                    false,
                    "$0.00 billed in this session · 150k input and 10k output tokens".to_string()
                ),
                (
                    false,
                    "≈ $3.40 at API prices, which the Claude plan covers".to_string()
                ),
                (
                    true,
                    "the workers' share ≈ $0.40 at API prices · 30k input and 2k output tokens"
                        .to_string()
                ),
                (false, "Claude plan 5 h 12%, 7 d 3%".to_string()),
            ]
        );
        assert!((api_price(&CostLedger { entries }).unwrap() - 3.4).abs() < 1e-9);

        // The ChatGPT login: its plan.
        let sol = settings(model("sol"), model("sol"));
        let lines = report(&sol, vec![call(MAIN, 50_000, 1_000, 0.9, 0.0)]);
        assert_eq!(
            lines[1].1,
            "≈ $0.90 at API prices, which the ChatGPT plan covers"
        );
        assert_eq!(lines[2].1, "ChatGPT plan 7 d 40%");

        // Per token: the cost, as before, and no API price line.
        let opus = settings(model("opus"), model("opus"));
        let entries = vec![
            call(MAIN, 120_000, 8_000, 1.0, 1.0),
            call(WORKER, 30_000, 2_000, 0.2, 0.2),
        ];
        assert_eq!(
            report(&opus, entries.clone()),
            [
                (
                    false,
                    "$1.20 billed in this session · 150k input and 10k output tokens".to_string()
                ),
                (
                    true,
                    "the workers' share is $0.20 · 30k input and 2k output tokens".to_string()
                ),
            ]
        );
        assert_eq!(
            api_price(&CostLedger { entries }),
            None,
            "nothing on a plan"
        );

        // Mixed: the main agent on the Claude plan, the workers per token.
        let mixed = settings(model("cc:opus"), model("gemini"));
        assert_eq!(
            report(
                &mixed,
                vec![
                    call(MAIN, 120_000, 8_000, 3.0, 0.0),
                    call(WORKER, 30_000, 2_000, 0.5, 0.5),
                ]
            ),
            [
                (false, "$0.50 billed in this session · 150k input and 10k output tokens".to_string()),
                (true, "main agent ≈ $3.00 at API prices, on the Claude plan · 120k input and 8k output tokens".to_string()),
                (true, "workers $0.50, billed per token · 30k input and 2k output tokens".to_string()),
                (false, "Claude plan 5 h 12%, 7 d 3%".to_string()),
            ]
        );
        // Nothing yet.
        assert_eq!(
            report(&cc, Vec::new()),
            [(
                false,
                "$0.00 billed in this session · 0k input and 0k output tokens".to_string()
            )]
        );
    }

    /// A main agent whose conversations ran on models paid for in different
    /// ways has a part for each, by each run's own model: the Claude plan's
    /// part stays the Claude plan's after a switch to the ChatGPT login,
    /// and a part billed per token says so.
    #[test]
    fn the_main_agents_part_is_split_by_the_plan_of_each_runs_model() {
        use crate::agents::{MAIN, WORKER};
        let run = |name: &str, entry: &arko_workflows::budget::CostEntry| RunCost {
            model: resolve_model(name).unwrap(),
            usage: match &entry.source {
                CostSource::Agent { usage, .. } => (**usage).clone(),
                CostSource::Manual { .. } => unreachable!(),
            },
        };
        let on_claude = call(MAIN, 120_000, 8_000, 3.0, 0.0);
        let on_chatgpt = call(MAIN, 50_000, 1_000, 0.9, 0.0);
        let worker = call(WORKER, 30_000, 2_000, 0.4, 0.0);
        // The session's main model is sol now, on the ChatGPT login.
        let switched = settings(model("sol"), model("cc:opus"));
        let runs = [run("cc:opus", &on_claude), run("sol", &on_chatgpt)];
        assert_eq!(
            report_with_runs(
                &switched,
                vec![on_claude.clone(), on_chatgpt.clone(), worker.clone()],
                &runs
            ),
            [
                (false, "$0.00 billed in this session · 200k input and 11k output tokens".to_string()),
                (true, "main agent ≈ $3.00 at API prices, on the Claude plan · 120k input and 8k output tokens".to_string()),
                (true, "main agent ≈ $0.90 at API prices, on the ChatGPT plan · 50k input and 1k output tokens".to_string()),
                (true, "workers ≈ $0.40 at API prices, on the Claude plan · 30k input and 2k output tokens".to_string()),
                (false, "Claude plan 5 h 12%, 7 d 3%".to_string()),
                (false, "ChatGPT plan 7 d 40%".to_string()),
            ]
        );
        // Without the runs, the session's main model named the plan.
        assert!(
            report(&switched, vec![on_claude.clone(), on_chatgpt.clone()])
                .iter()
                .any(|(_, line)| line == "≈ $3.90 at API prices, which the ChatGPT plan covers")
        );
        // Runs of one plan, one of them before a switch, read as one part.
        let ledger = |entries: &[&arko_workflows::budget::CostEntry]| CostLedger {
            entries: entries.iter().map(|entry| (*entry).clone()).collect(),
        };
        let both = [run("cc:opus", &on_claude), run("cc:fable", &on_claude)];
        let (parts, _) = cost_shares(&ledger(&[&on_claude, &on_claude]), &switched, &both, None);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].plan, Some("Claude plan"));
        assert_eq!(parts[0].input, 240_000);
        // From the Claude plan to a model billed per token.
        let per_token = call(MAIN, 10_000, 1_000, 0.2, 0.2);
        let runs = [run("cc:opus", &on_claude), run("opus", &per_token)];
        let opus = settings(model("opus"), model("opus"));
        let (parts, _) = cost_shares(&ledger(&[&on_claude, &per_token]), &opus, &runs, None);
        assert_eq!(
            parts.iter().map(|part| part.plan).collect::<Vec<_>>(),
            [Some("Claude plan"), None]
        );
        let total = CostShare::sum(&parts);
        assert!((total.billed - 0.2).abs() < 1e-9 && (total.api - 3.2).abs() < 1e-9);
        assert_eq!(total.plan, None);
    }

    /// The ledger says what the main agent used, calls under way included,
    /// and its runs only split it. What the runs haven't recorded yet (a
    /// turn under way) is the current conversation's, on its model's plan.
    #[test]
    fn calls_under_way_count_for_the_current_conversation() {
        use crate::agents::MAIN;
        let run = |name: &str, entry: &arko_workflows::budget::CostEntry| RunCost {
            model: resolve_model(name).unwrap(),
            usage: match &entry.source {
                CostSource::Agent { usage, .. } => (**usage).clone(),
                CostSource::Manual { .. } => unreachable!(),
            },
        };
        let on_claude = call(MAIN, 120_000, 8_000, 3.0, 0.0);
        let on_chatgpt = call(MAIN, 50_000, 1_000, 0.9, 0.0);
        let under_way = call(MAIN, 20_000, 500, 0.3, 0.0);
        let switched = settings(model("sol"), model("sol"));
        let ledger = CostLedger {
            entries: vec![on_claude.clone(), on_chatgpt.clone(), under_way],
        };
        let runs = [run("cc:opus", &on_claude), run("sol", &on_chatgpt)];
        let sol = resolve_model("sol").unwrap();
        let (parts, _) = cost_shares(&ledger, &switched, &runs, Some(&sol));
        let shown: Vec<(Option<&str>, usize)> =
            parts.iter().map(|part| (part.plan, part.input)).collect();
        assert_eq!(
            shown,
            [
                (Some("Claude plan"), 120_000),
                (Some("ChatGPT plan"), 70_000)
            ]
        );
        assert!(
            (CostShare::sum(&parts).api - 4.2).abs() < 1e-9,
            "the ledger's total"
        );
        // Back in the Claude Code conversation, its turn under way is its.
        let opus = resolve_model("cc:opus").unwrap();
        let (parts, _) = cost_shares(&ledger, &switched, &runs, Some(&opus));
        assert_eq!(parts[0].input, 140_000);
        assert_eq!(parts[1].input, 50_000);
    }

    /// A saved run's plan is its own model's and billing's, whatever the
    /// session reaches OpenAI models by now, and a run billed in different
    /// ways keeps the API price of what a plan paid for.
    #[test]
    fn a_saved_runs_plan_doesnt_follow_the_sessions_login_now() {
        use crate::agents::MAIN;
        let entry = call(MAIN, 50_000, 1_000, 0.9, 0.0);
        let usage = match &entry.source {
            CostSource::Agent { usage, .. } => (**usage).clone(),
            CostSource::Manual { .. } => unreachable!(),
        };
        let mut on_key = settings(model("sol"), model("sol"));
        on_key.openai_auth = Some(OpenAiAuth::ApiKey);
        let runs = [RunCost {
            model: resolve_model("sol").unwrap(),
            usage: usage.clone(),
        }];
        let ledger = CostLedger {
            entries: vec![entry.clone()],
        };
        let (parts, _) = cost_shares(&ledger, &on_key, &runs, None);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].plan, Some("ChatGPT plan"));
        assert!((parts[0].api - 0.9).abs() < 1e-9);
        // Some of a run's calls on the plan and some per token.
        let mixed = UsageInfo {
            billing: BillingKind::Mixed,
            total_cost: 3.0,
            billed_cost: Some(0.5),
            ..usage
        };
        assert!((at_api_prices(&mixed, mixed.billed()) - 3.0).abs() < 1e-9);
        let runs = [RunCost {
            model: resolve_model("cc:opus").unwrap(),
            usage: mixed,
        }];
        let (parts, _) = cost_shares(&CostLedger::default(), &on_key, &runs, None);
        assert_eq!(parts[0].plan, Some("Claude plan"));
    }

    /// The picker offers only the efforts each model takes, and a model
    /// switched to without one gets the nearest to the effort before.
    #[test]
    fn each_model_takes_its_own_efforts() {
        use Effort::{High, Low, Max, Medium, Xhigh};
        let efforts = |name: &str| efforts(&resolve_model(name).unwrap());
        assert_eq!(efforts("opus"), [Low, Medium, High, Xhigh, Max]);
        assert_eq!(efforts("cc:fable"), [Low, Medium, High, Xhigh, Max]);
        assert_eq!(efforts("sol"), [Low, Medium, High, Xhigh, Max]);
        assert_eq!(efforts("terra"), [Low, Medium, High, Xhigh, Max]);
        assert_eq!(
            efforts("gpt-5.4-nano-2026-03-17"),
            [Low, Medium, High, Xhigh]
        );
        assert_eq!(efforts("gemini"), [Low, Medium, High]);
        assert_eq!(efforts("gemini-3-pro-preview"), [Low, High]);
        assert!(efforts("gemini-2.0-flash").is_empty(), "it doesn't reason");
        assert_eq!(efforts("openrouter:x/y"), [Low, Medium, High, Xhigh, Max]);
        let fit = |name: &str, effort| fit_effort(&resolve_model(name).unwrap(), effort);
        assert_eq!(fit("gemini", Xhigh), High);
        assert_eq!(fit("gemini-3-pro-preview", Medium), Low);
        assert_eq!(fit("opus", Medium), Medium);
        assert_eq!(
            efforts_text(&resolve_model("gemini").unwrap()),
            "low, medium or high"
        );
        assert_eq!(parse_effort("XHigh"), Ok(Xhigh));
        assert!(
            parse_effort("extreme")
                .unwrap_err()
                .contains("low, medium, high, xhigh and max")
        );
    }

    /// The menu lists the ways that can be used first, says what the
    /// others need, and lists the agents' own models where it leaves them
    /// out.
    #[test]
    fn the_model_menu_puts_what_can_be_used_first() {
        let needs = |name: &str| (!name.starts_with("gpt-")).then(|| format!("what {name} needs"));
        let menu = model_menu(
            needs,
            Some(OpenAiAuth::ApiKey),
            &["gpt-5.6-terra", "gpt-6.1-sol"],
            &[],
        );
        assert_eq!(menu[0].name, "OpenAI, with your API key");
        assert_eq!(menu[0].needs, None);
        let names: Vec<&str> = menu[0]
            .models
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "gpt-6.1-sol",
                "gpt-6-astra",
                "gpt-6.1-terra",
                "gpt-6-luna",
                "gpt-5.6-terra"
            ]
        );
        assert!(menu[1..].iter().all(|group| group.needs.is_some()));
        assert_eq!(menu[1].name, "Claude Code, on your Claude plan");
        assert_eq!(
            menu[1].models[0],
            ("cc:claude-opus-5-5".to_string(), "cc:opus")
        );
        // Every model listed is one Arko knows.
        for group in &menu {
            for (name, _) in &group.models {
                assert!(resolve_model(name).is_ok(), "{name}");
            }
        }
    }

    /// OpenRouter serves any model, so its group lists the ones picked, newest
    /// first and eight at most, or one to start with while none were. The
    /// agents' models join it when they aren't among them.
    #[test]
    fn the_openrouter_group_lists_the_models_picked_or_one_to_start_with() {
        let group = |current: &[&str], recent: &[String]| {
            model_menu(|_| None, None, current, recent)
                .into_iter()
                .find(|group| group.name.starts_with("OpenRouter"))
                .unwrap()
                .models
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            group(&[], &[]),
            ["openrouter:anthropic/claude-opus-5.5"],
            "one to start with"
        );
        let picked: Vec<String> = (0..10).map(|n| format!("openrouter:acme/m{n}")).collect();
        let listed = group(&[], &picked);
        assert_eq!(listed.len(), 8);
        assert_eq!(listed[..2], ["openrouter:acme/m0", "openrouter:acme/m1"]);
        assert!(
            !listed.contains(&"openrouter:anthropic/claude-opus-5.5".to_string()),
            "{listed:?}"
        );
        // The current models that aren't picked come after.
        let listed = group(
            &["openrouter:acme/m1", "openrouter:other/now", "opus"],
            &picked[..2],
        );
        assert_eq!(
            listed,
            [
                "openrouter:acme/m0",
                "openrouter:acme/m1",
                "openrouter:other/now"
            ]
        );
        // Whether it can be used is still told by its sample model.
        let menu = model_menu(
            |name| (name == "openrouter:anthropic/claude-opus-5.5").then(|| "a key".to_string()),
            None,
            &[],
            &picked,
        );
        let openrouter = menu
            .iter()
            .find(|g| g.name.starts_with("OpenRouter"))
            .unwrap();
        assert_eq!(openrouter.needs.as_deref(), Some("a key"));
    }

    /// What the OpenRouter settings rule out is named when a model isn't on
    /// the list.
    #[test]
    fn a_model_the_settings_rule_out_is_said_to_be_ruled_out() {
        let catalog = catalog(CATALOG);
        let mut session = settings(model("openrouter:acme/typo"), model("opus"));
        let error = apply_catalog(
            &mut session,
            Ok(&catalog),
            false,
            Some("zero data retention, US only"),
        )
        .unwrap_err();
        assert!(
            error.contains("that your settings allow (zero data retention, US only)"),
            "{error}"
        );
        assert!(error.contains("/openrouter changes them"), "{error}");
        let error = apply_catalog(&mut session, Ok(&catalog), false, None).unwrap_err();
        assert!(
            error.contains("isn't in OpenRouter's model catalog of models that take tool calls"),
            "{error}"
        );
        assert!(!error.contains("settings"), "{error}");
    }

    /// Claude Code and the Anthropic API offer the same Claude models, Haiku
    /// 5.5 and Mythos 5.1 among them, and only Mythos comes with a note,
    /// since it needs access first.
    #[test]
    fn claude_code_and_the_api_offer_the_same_claude_models() {
        let menu = model_menu(|_| None, None, &[], &[]);
        let models = |way: &str| {
            menu.iter()
                .find(|group| group.name.starts_with(way))
                .unwrap()
                .models
                .clone()
        };
        let (cc, api) = (models("Claude Code"), models("Anthropic API"));
        let on_cc: Vec<&str> = cc
            .iter()
            .map(|(name, _)| name.strip_prefix(CLAUDE_CODE).unwrap())
            .collect();
        let on_api: Vec<&str> = api.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(on_cc, on_api);
        assert!(cc.contains(&("cc:claude-haiku-5-5".to_string(), "cc:haiku")));
        assert!(cc.contains(&("cc:claude-mythos-5-1".to_string(), "cc:mythos")));
        assert!(api.contains(&("claude-haiku-5-5".to_string(), "haiku")));
        assert!(api.contains(&("claude-mythos-5-1".to_string(), "mythos")));
        for (name, alias) in cc.iter().chain(&api) {
            let note = model_note(name);
            assert_eq!(note, model_note(alias), "{alias}");
            assert_eq!(
                note.is_some(),
                name.ends_with("claude-mythos-5-1"),
                "{name}"
            );
        }
        assert_eq!(model_note("cc:mythos"), Some(MYTHOS_ACCESS));
    }

    /// Claude Code's refusal of Claude Mythos 5.1 says what access it needs.
    /// Other errors, also the same refusal of another model, stay as they
    /// are.
    #[test]
    fn a_refused_mythos_says_what_access_it_needs() {
        let refused = "There's an issue with the selected model (claude-mythos-5-1). It may not exist or you may not have access to it. Run /model to pick a different model.";
        let explained = explain_error(refused);
        assert!(explained.starts_with(refused), "{explained}");
        assert!(
            explained.ends_with(
                "Claude Mythos 5.1 needs access through Anthropic's Cyber Verification Program."
            ),
            "{explained}"
        );
        for other in [
            "There's an issue with the selected model (claude-opus-5-5). It may not exist or you may not have access to it.",
            "Claude Code's request failed (server_error, status 500). It retries (attempt 1 of 10).",
        ] {
            assert_eq!(explain_error(other), other);
        }
    }

    #[test]
    fn a_model_is_named_by_its_alias_when_it_has_one() {
        assert_eq!(short_model_name("gpt-6.1-sol"), "sol");
        assert_eq!(short_model_name("claude-opus-5-5"), "opus");
        assert_eq!(short_model_name("cc:claude-haiku-5-5"), "cc:haiku");
        assert_eq!(
            short_model_name("gpt-5.4-nano-2026-03-17"),
            "gpt-5.4-nano-2026-03-17"
        );
        let long = "openrouter:some-maker/some-very-long-model-name-latest";
        let short = short_model_name(long);
        assert_eq!(short.chars().count(), 28);
        assert!(short.starts_with("openrouter:some-maker/") && short.ends_with('…'));
    }

    /// A note with no room for the aliases keeps what is wrong with the
    /// name and what it may have meant.
    #[test]
    fn an_unknown_model_error_is_cut_before_its_aliases() {
        let full = resolve_model("sool").unwrap_err();
        assert!(full.contains("an alias (opus = "), "{full}");
        assert_eq!(
            briefly(&full),
            "`sool` isn't a model Arko knows. Did you mean sol?"
        );
        assert_eq!(
            briefly(&resolve_model("gpt9").unwrap_err()),
            "`gpt9` isn't a model Arko knows."
        );
        // Other errors are short as they are.
        let openrouter = resolve_model("openrouter:").unwrap_err();
        assert_eq!(briefly(&openrouter), openrouter);
    }

    /// Every small model is small by its ID, its alias, their `cc:` forms
    /// (Claude models only) and its exact OpenRouter slug, with or without a
    /// routing suffix.
    #[test]
    fn each_small_model_is_small_on_every_route() {
        for id in SMALL_MODELS {
            let registered = Model::find(id).unwrap_or_else(|| panic!("{id} isn't registered"));
            let mut names = vec![id.to_string()];
            names.extend(
                ALIASES
                    .iter()
                    .filter(|(_, registry)| *registry == id)
                    .map(|(alias, _)| alias.to_string()),
            );
            if registered.provider == Provider::Anthropic {
                names.extend(names.clone().into_iter().map(|name| format!("cc:{name}")));
            }
            let slug = OPENROUTER_EQUIVALENTS
                .iter()
                .find(|(registry, _)| *registry == id)
                .unwrap_or_else(|| panic!("{id} has no OpenRouter slug"))
                .1;
            names.push(format!("openrouter:{slug}"));
            for suffix in OPENROUTER_ROUTING_SUFFIXES {
                names.push(format!("openrouter:{slug}{suffix}"));
            }
            for name in names {
                let model = resolve_model(&name).unwrap();
                assert!(is_small(&model), "{name}");
            }
        }
    }

    /// The table holds the Haiku and Luna models the registry has, so a new
    /// one fails here until someone decides whether it is small.
    #[test]
    fn the_small_models_are_the_registered_haiku_and_luna() {
        let mut registered: Vec<String> = Model::all()
            .into_iter()
            .map(|model| model.id)
            .filter(|id| id.contains("haiku") || id.contains("luna"))
            .collect();
        registered.sort();
        let mut small: Vec<String> = SMALL_MODELS.iter().map(|id| id.to_string()).collect();
        small.sort();
        assert_eq!(small, registered);
        for (id, slug) in OPENROUTER_EQUIVALENTS {
            assert!(Model::find(id).is_some(), "{id}");
            assert_eq!(slug.split('/').count(), 2, "{slug}");
            assert!(!slug.contains([':', '@', '~', ' ']), "{slug}");
        }
    }

    /// A name that only resembles a small model is not one: other versions,
    /// publishers, fine-tunes, variants other than the routing suffixes, two
    /// suffixes, other letter cases and registry IDs that OpenRouter doesn't
    /// use as slugs.
    #[test]
    fn lookalikes_are_not_small() {
        for slug in [
            "anthropic/claude-haiku-5.5-v2",
            "anthropic/claude-haiku-5.5:preview",
            "anthropic/claude-haiku-5.5:online",
            "anthropic/claude-haiku-5.5:beta",
            "anthropic/claude-haiku-5.5:turbo",
            "anthropic/claude-haiku-5.5:free:floor",
            "anthropic/claude-haiku-5.5:Free",
            "anthropic/claude-haiku-5.5-v2:floor",
            "someorg/claude-haiku-5.5:free",
            "anthropic/claude-haiku-5.5@preset/short-answers",
            "anthropic/claude-haiku-5-5",
            "anthropic/claude-haiku-5.5-ft-acme",
            "anthropic/claude-haiku-4.5-20251001",
            "~anthropic/claude-haiku-latest",
            "Anthropic/Claude-Haiku-5.5",
            "someorg/claude-haiku-5.5",
            "someorg/haiku-finetune",
            "haiku",
            "claude-haiku-5-5",
            "openai/gpt-6-luna-preview",
            "openai/gpt-6-luna:preview",
            "openai/gpt-6-luna:online",
            "openai/gpt-6-luna:nitro:free",
            "openai/gpt-6-luna:NITRO",
            "OpenAI/GPT-6-Luna:floor",
            "openai/gpt-6-luna-ft-acme:nitro",
            "openai/gpt-6.1-luna",
            "openai/gpt-5.6-luna-mini",
            "someorg/luna",
            "luna",
            "luna-something",
            "gpt-6-luna",
            "openai/gpt-6-sol",
            "anthropic/claude-opus-5.5",
        ] {
            let model = resolve_model(&format!("openrouter:{slug}")).unwrap();
            assert!(!is_small(&model), "openrouter:{slug}");
        }
    }

    /// Every row pairs a registry model with a plain `org/name` slug, with no
    /// whitespace and no `:` suffix, and no ID or slug is there twice. Every
    /// aliased model has a row or is among those OpenRouter doesn't serve,
    /// and not both, so moving an alias fails here until the table follows.
    #[test]
    fn every_alias_has_one_decision_and_every_row_is_plain() {
        let mut ids = std::collections::HashSet::new();
        let mut slugs = std::collections::HashSet::new();
        for (id, slug) in OPENROUTER_EQUIVALENTS {
            assert!(Model::find(id).is_some(), "{id} isn't a registry model");
            let parts: Vec<&str> = slug.split('/').collect();
            assert_eq!(parts.len(), 2, "{slug}");
            assert!(parts.iter().all(|part| !part.is_empty()), "{slug}");
            assert!(
                !slug.contains(|c: char| c.is_whitespace() || ":@~".contains(c)),
                "{slug}"
            );
            assert!(ids.insert(*id), "{id} has two rows");
            assert!(slugs.insert(*slug), "{slug} has two rows");
        }
        for id in NOT_ON_OPENROUTER {
            assert!(Model::find(id).is_some(), "{id} isn't a registry model");
        }
        for (alias, id) in ALIASES {
            let tabled = ids.contains(id);
            let not_served = NOT_ON_OPENROUTER.contains(&id);
            assert!(
                tabled != not_served,
                "{alias} ({id}) needs a row or a place in NOT_ON_OPENROUTER, and not both"
            );
        }
    }

    /// A tabled slug, also with a routing suffix, is its model, and anything
    /// that only looks like one is none: other versions and variants,
    /// suffixes other than the routing ones, other publishers, fine-tunes
    /// and other cases.
    #[test]
    fn only_a_tabled_slug_is_its_model() {
        for (id, slug) in OPENROUTER_EQUIVALENTS {
            assert_eq!(openrouter_equivalent(slug), Some(*id), "{slug}");
            for suffix in OPENROUTER_ROUTING_SUFFIXES {
                assert_eq!(
                    openrouter_equivalent(&format!("{slug}{suffix}")),
                    Some(*id),
                    "{slug}{suffix}"
                );
            }
            let (org, name) = slug.split_once('/').unwrap();
            for lookalike in [
                format!("{slug}-v2"),
                format!("{slug}:batch"),
                format!("{slug}:free:floor"),
                format!("{slug}:FREE"),
                format!("{slug}-v2:floor"),
                format!("someorg/{name}"),
                format!("{slug}-ft-acme"),
                format!("~{slug}"),
                slug.to_uppercase(),
                format!("{org}/{name}x"),
                name.to_string(),
                id.to_string(),
            ] {
                assert_eq!(openrouter_equivalent(&lookalike), None, "{lookalike}");
            }
        }
    }

    /// Each slug in the table is one OpenRouter lists now. Run with the
    /// other live tests (`--ignored`): it reads OpenRouter's model list,
    /// which needs no key.
    #[tokio::test]
    #[ignore = "reads OpenRouter's model list"]
    async fn openrouter_still_lists_every_tabled_slug() {
        let body = arko_ai::http_client_builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap()
            .get("https://openrouter.ai/api/v1/models")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let listed: std::collections::HashSet<&str> = listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|model| model["id"].as_str())
            .collect();
        let gone: Vec<&str> = OPENROUTER_EQUIVALENTS
            .iter()
            .map(|(_, slug)| *slug)
            .filter(|slug| !listed.contains(slug))
            .collect();
        assert!(gone.is_empty(), "OpenRouter no longer lists {gone:?}");
    }

    /// The other models, by every route they have, are not small, and a
    /// name Arko doesn't know resolves to no model at all.
    #[test]
    fn the_other_models_are_not_small() {
        for model in Model::all() {
            let small = SMALL_MODELS.contains(&model.id.as_str());
            assert_eq!(is_small(&model), small, "{}", model.id);
            if model.provider == Provider::Anthropic {
                let model = Model::claude_code(model);
                assert_eq!(is_small(&model), small, "cc:{}", model.id);
            }
        }
        for name in ["luna-something", "haiku-finetune", "claude-haiku-5-5-v2"] {
            assert!(resolve_model(name).is_err(), "{name}");
        }
    }
}

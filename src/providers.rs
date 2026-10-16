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

use std::path::Path;
use std::time::Duration;

use arko_ai::model::ModelType;
use arko_ai::{
    AnthropicCacheTtl, BillingKind, CodexAuthFile, GeminiThinkingLevel, GenerationConfig,
    LLMClient, Model, OpenAIServiceTier, OpenRouterCatalog, OpenRouterReasoning, PlanUsage,
    Provider, ReasoningConfig, ReasoningEffort, ReasoningSummary, ServiceTier, SubscriptionPlan,
    UsageInfo,
};
use arko_workflows::budget::{CostLedger, CostSource};
use serde::{Deserialize, Serialize};

use crate::agents::{Effort, ModelSettings, default_handover};
use crate::claude_code;
use crate::credentials::{self, Need};
use crate::session::Settings;

/// The prefix of a model on OpenRouter: `openrouter:<slug>`.
pub const OPENROUTER: &str = "openrouter:";
/// The prefix of a Claude model run in Claude Code: `cc:<model>`.
pub const CLAUDE_CODE: &str = "cc:";
/// The long form of [`CLAUDE_CODE`], also accepted.
const CLAUDE_CODE_LONG: &str = "claude-code:";

/// Short names for the models people use most.
pub const ALIASES: [(&str, &str); 8] = [
    ("opus", "claude-opus-5-5"),
    ("fable", "claude-fable-5-1"),
    ("sonnet", "claude-sonnet-5-5"),
    ("sol", "gpt-6.1-sol"),
    ("astra", "gpt-6-astra"),
    ("luna", "gpt-6-luna"),
    ("gemini", "gemini-3.1-pro-preview"),
    ("flash", "gemini-3.8-flash"),
];

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
                "`{name}` names no OpenRouter model: give its slug, such as openrouter:anthropic/claude-opus-5.5"
            ));
        }
        return Ok(Model::openrouter(slug));
    }
    if let Some(rest) =
        strip_prefix(name, CLAUDE_CODE).or_else(|| strip_prefix(name, CLAUDE_CODE_LONG))
    {
        let model = registered(rest).map_err(|_| {
            format!(
                "`{name}` names no Claude model: Claude Code runs Claude models, such as cc:opus (claude-opus-5-5) or cc:fable (claude-fable-5-1){}",
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

/// The error for a name that names no model, with what it may have meant.
fn unknown(name: &str) -> String {
    let mut text = format!("`{name}` isn't a model Arko knows.");
    if name.contains('/') {
        text.push_str(&format!(
            " Models on OpenRouter take its prefix: openrouter:{name}."
        ));
    } else if let Some(close) = suggestion(name) {
        text.push_str(&format!(" Did you mean {close}?"));
    }
    text.push_str(&format!(
        " Use a model ID, an alias ({}), openrouter:<slug> for a model on OpenRouter, or cc:<model> for a Claude model in Claude Code (cc:opus).",
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

fn strip_prefix<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
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

/// Whether `model` takes Anthropic's prompt-cache lifetime (the 1-hour
/// cache): Claude models on an API key or on OpenRouter. Claude Code
/// chooses its own: an hour on a Claude plan.
pub fn takes_cache_ttl(model: &Model) -> bool {
    match model.provider {
        Provider::Anthropic => true,
        Provider::OpenRouter => is_claude_slug(&model.id),
        _ => false,
    }
}

/// The families of small models: fast, cheap models that do better with
/// plain, explicit rules than the large ones need (`agents::extra_rules`).
const SMALL_FAMILIES: [&str; 2] = ["haiku", "luna"];

/// Whether `model` is a small model: its ID names a small family, whatever
/// the route (`claude-haiku-5-5`, `cc:claude-haiku-5-5`,
/// `openrouter:anthropic/claude-haiku-5.5`, `gpt-6-luna`).
pub fn is_small(model: &Model) -> bool {
    let id = model.id.to_ascii_lowercase();
    SMALL_FAMILIES.iter().any(|family| id.contains(family))
}

/// The generation settings of an agent on `model`: its reasoning at
/// `effort`, and fast mode and the 1-hour cache where the provider has them.
pub fn generation(
    model: &Model,
    effort: Effort,
    fast: bool,
    cache_1h: bool,
    show_thinking: bool,
) -> Result<GenerationConfig, String> {
    let mut generation = GenerationConfig::default().with_prompt_caching(true);
    let reasons = model.model_type == ModelType::Reasoning;
    match model.provider {
        Provider::Anthropic | Provider::ClaudeCode => {
            if fast && !model.supports_anthropic_fast_mode() {
                return Err(format!(
                    "{} has no fast mode: Anthropic offers it on Claude Opus 5.5, Opus 5 and Opus 4.8",
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
        }
        Provider::OpenAI => {
            if reasons {
                generation = generation
                    .with_reasoning(ReasoningConfig::openai(effort.into(), show_thinking));
            }
            if fast {
                generation = generation.with_openai_service_tier(OpenAIServiceTier::Fast);
            }
        }
        Provider::Google => {
            if fast {
                return Err(format!(
                    "{} has no fast mode: Gemini models have none",
                    model.id
                ));
            }
            if reasons {
                generation = generation.with_reasoning(ReasoningConfig::gemini(
                    gemini_level(model, effort),
                    show_thinking,
                ));
            }
        }
        Provider::OpenRouter => {
            if fast {
                return Err(format!(
                    "{} has no fast mode: OpenRouter has none (a :nitro slug, such as {}:nitro, prefers its fastest providers)",
                    model_name(model),
                    model_name(model)
                ));
            }
            // OpenRouter maps the effort to the model's own levels, and
            // ignores it for models that don't reason.
            let mut reasoning = OpenRouterReasoning::effort(reasoning_effort(effort));
            if show_thinking {
                reasoning = reasoning.with_summary(ReasoningSummary::Auto);
            }
            generation = generation.with_reasoning(ReasoningConfig::openrouter(reasoning));
        }
    }
    if cache_1h && takes_cache_ttl(model) {
        generation = generation.with_anthropic_cache_ttl(AnthropicCacheTtl::OneHour);
    }
    Ok(generation)
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

/// How a session reaches OpenAI models: one way for all of them, since
/// arko's client holds one OpenAI client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum OpenAiAuth {
    /// The ChatGPT login (a Codex auth.json): usage counts against the
    /// ChatGPT plan.
    #[serde(rename = "chatgpt")]
    #[value(name = "chatgpt")]
    ChatGpt,
    /// An OpenAI API key, billed per token.
    #[serde(rename = "api-key")]
    #[value(name = "api-key")]
    ApiKey,
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
        return <OpenAiAuth as clap::ValueEnum>::from_str(value.trim(), true)
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
    auth_file: Option<&Path>,
) -> Option<OpenAiAuth> {
    let login = (var("OPENAI_OAUTH_ACCESS_TOKEN").is_some()
        && var("OPENAI_OAUTH_ACCOUNT_ID").is_some())
        || auth_file.is_some_and(Path::is_file)
        || (auth_file.is_none()
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
    available_openai_auth(
        &credentials::var,
        credentials::codex_auth(codex_auth).as_deref(),
    )
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
    <Effort as clap::ValueEnum>::from_str(text.trim(), true).map_err(|_| {
        format!("`{text}` isn't an effort. The efforts are low, medium, high, xhigh and max.")
    })
}

/// What the user is told when `model` can't be used: it needs `needs`
/// (from [`missing_credential`] or [`credential_name`]), and how to get it.
pub fn needs_text(model: &str, needs: &str) -> String {
    if needs == "Claude Code signed in" {
        format!("{model} needs Claude Code signed in. Sign in with `claude auth login`.")
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

/// A group of the models `/model` offers: one way to reach them, and what
/// it needs when that isn't there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGroup {
    /// The way, as the picker heads it: "Claude Code, on your Claude plan".
    pub name: String,
    /// What the way needs that isn't there; its models can't be chosen.
    pub needs: Option<String>,
    /// Each model: its name as `/model` takes it, and its alias.
    pub models: Vec<(String, &'static str)>,
}

/// The models `/model` offers, by the way they are reached: Claude Code,
/// the Anthropic API, OpenAI, Gemini and OpenRouter, the ways that can be
/// used first. `needs` says what a model (a way's first) needs that isn't
/// there, and `openai_auth` how OpenAI models are reached. `current` (the
/// agents' models now) are listed in their group when the list leaves
/// them out.
pub fn model_menu(
    needs: impl Fn(&str) -> Option<String>,
    openai_auth: Option<OpenAiAuth>,
    current: &[&str],
) -> Vec<ModelGroup> {
    let openai = match openai_auth {
        Some(OpenAiAuth::ApiKey) => "OpenAI, with your API key",
        _ => "OpenAI, on your ChatGPT plan",
    };
    let groups: [(String, &str, &[(&str, &'static str)]); 5] = [
        (
            "Claude Code, on your Claude plan".to_string(),
            "cc:claude-opus-5-5",
            &[
                ("cc:claude-opus-5-5", "cc:opus"),
                ("cc:claude-fable-5-1", "cc:fable"),
                ("cc:claude-sonnet-5-5", "cc:sonnet"),
            ],
        ),
        (
            "Anthropic API, billed per token".to_string(),
            "claude-opus-5-5",
            &[
                ("claude-opus-5-5", "opus"),
                ("claude-fable-5-1", "fable"),
                ("claude-sonnet-5-5", "sonnet"),
            ],
        ),
        (
            openai.to_string(),
            "gpt-6.1-sol",
            &[
                ("gpt-6.1-sol", "sol"),
                ("gpt-6-astra", "astra"),
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
        let last = Self {
            main: settings.main.model.clone(),
            worker: (settings.worker.model != settings.main.model)
                .then(|| settings.worker.model.clone()),
        };
        std::fs::create_dir_all(root)?;
        let path = root.join(LAST_MODELS_FILE);
        let temporary = root.join(format!("{LAST_MODELS_FILE}.tmp"));
        std::fs::write(&temporary, serde_json::to_string_pretty(&last)?)?;
        std::fs::rename(temporary, path)
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
                "{} has no Fast mode with an API key: OpenAI doesn't offer it for this model",
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
pub fn client(codex_auth: Option<&Path>, settings: &Settings) -> Result<LLMClient, Failure> {
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
        credentials::var,
        credentials::codex_auth(codex_auth),
        claude_code::check_recent,
    )
}

/// [`client`], with the settings read through `var`, the Codex auth.json
/// the user named (else Codex's own), and how Claude Code is checked.
fn client_with(
    settings: &Settings,
    var: impl Fn(&str) -> Option<String>,
    auth_file: Option<std::path::PathBuf>,
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
        false => available_openai_auth(&var, auth_file.as_deref()),
    };
    let from_env = |auth: Option<OpenAiAuth>| {
        LLMClient::from_env_with(|name| {
            let wanted = if name.starts_with("OPENAI_OAUTH_") {
                auth == Some(OpenAiAuth::ChatGpt) && auth_file.is_none()
            } else if name == credentials::OPENAI_KEY {
                auth == Some(OpenAiAuth::ApiKey)
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
    if spare && auth == Some(OpenAiAuth::ChatGpt) && !client.has_provider(&Provider::OpenAI) {
        let file = match &auth_file {
            Some(path) => CodexAuthFile::load(path),
            None => CodexAuthFile::load_default(),
        };
        if let Ok(file) = file
            && file.credentials().is_ok()
            && let Ok(with) = client.clone().with_codex_auth_file(&file)
        {
            client = with;
        }
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
        let auth = match &auth_file {
            Some(path) => CodexAuthFile::load(path),
            None => CodexAuthFile::load_default(),
        }
        .map_err(|error| need(error.to_string()))?;
        auth.credentials().map_err(|error| {
            need(format!(
                "{} holds no ChatGPT login ({error}). Sign in with `codex login`",
                auth.path().display()
            ))
        })?;
        client = client
            .with_codex_auth_file(&auth)
            .map_err(|error| need(error.to_string()))?;
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

/// How long Arko waits for OpenRouter's catalog.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(10);

/// Checks a new session's OpenRouter models against OpenRouter's public
/// model catalog (read without the API key; it costs nothing): the slug
/// exists and takes tool calls, and, unless `--handover-at` was given
/// (`explicit_handover`), the handover is sized from its context window.
/// Returns what to tell the user. Without the catalog (offline), the
/// session starts unchecked, handing over at [`UNKNOWN_LIMIT_HANDOVER`].
pub async fn check_openrouter(
    settings: &mut Settings,
    explicit_handover: bool,
    base_url: &str,
) -> Result<Vec<String>, String> {
    let uses = [&settings.main, &settings.worker]
        .iter()
        .any(|model| strip_prefix(&model.model, OPENROUTER).is_some());
    if !uses {
        return Ok(Vec::new());
    }
    let client = arko_ai::OpenRouterClient::new("").with_base_url(base_url);
    let catalog = match tokio::time::timeout(CATALOG_TIMEOUT, client.fetch_models()).await {
        Ok(Ok(catalog)) => Ok(catalog),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("it didn't answer in 10 s".to_string()),
    };
    apply_catalog(settings, catalog.as_deref(), explicit_handover)
}

/// [`check_openrouter`] with the catalog read (or why it couldn't be).
fn apply_catalog(
    settings: &mut Settings,
    catalog: Result<&OpenRouterCatalog, &String>,
    explicit_handover: bool,
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
        let handover =
            |model: &ModelSettings| model.handover_at.unwrap_or(UNKNOWN_LIMIT_HANDOVER) / 1000;
        let catalog = match catalog {
            Ok(catalog) => catalog,
            Err(error) => {
                note(
                    &mut notes,
                    format!(
                        "Could not read OpenRouter's model catalog ({error}), so Arko can't check {name} or tell its context size: it hands over at {}k tokens (--handover-at sets it).",
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
                    "Arko can't tell the context size of the preset {name}: it hands over at {}k tokens (--handover-at sets it).",
                    handover(model)
                ),
            );
            continue;
        }
        let Some(listed) = catalog.get(&slug) else {
            return Err(format!(
                "{name} isn't in OpenRouter's model catalog. https://openrouter.ai/models lists the slugs (such as anthropic/claude-opus-5.5)"
            ));
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
        }
    }
    // What sessions read before the handover was set per model.
    settings.handover_at = settings.main.handover_at.unwrap_or(settings.handover_at);
    Ok(notes)
}

/// A plan's usage windows, as the status line and `/cost` say them:
/// `Claude plan 5 h 12%, 7 d 3%`. `None` when it reports no window.
pub fn plan_usage_text(usage: &PlanUsage) -> Option<String> {
    let windows = [(&usage.primary, "5 h"), (&usage.secondary, "7 d")]
        .into_iter()
        .filter_map(|(window, name)| {
            let window = window.as_ref()?;
            let name = match window.window_minutes {
                Some(300) => "5 h".to_string(),
                Some(10_080) => "7 d".to_string(),
                Some(minutes) => format!("{minutes} min"),
                None => name.to_string(),
            };
            Some(format!("{name} {:.0}%", window.used_percent))
        })
        .collect::<Vec<_>>();
    if windows.is_empty() {
        return None;
    }
    Some(format!("{} {}", plan_name(usage.plan), windows.join(", ")))
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

/// What a call would have cost at API list prices: its estimate when a
/// plan covered it, else what it was billed.
fn at_api_prices(usage: &UsageInfo, billed: f64) -> f64 {
    if usage.billing == BillingKind::Subscription {
        usage.total_cost
    } else {
        billed
    }
}

/// What the session's usage would have cost at API list prices: what the
/// calls billed per token cost, and the list-price estimate of those a
/// plan covered (Claude Code, the ChatGPT login). `None` when nothing ran
/// on a plan: what was billed says it all then.
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

/// One agent's (the main agent's, or the workers') calls, for `/cost`.
#[derive(Default)]
struct Part {
    billed: f64,
    api: f64,
    input: usize,
    output: usize,
    /// The plan that covered its calls, when one did.
    plan: Option<&'static str>,
}

impl Part {
    fn tokens(&self) -> String {
        format!(
            "{}k input and {}k output tokens",
            self.input / 1000,
            self.output / 1000
        )
    }
}

/// `/cost`: what the session was billed, and, for what ran on a plan, what
/// it would have cost at API prices and the plan's usage windows. The main
/// agent and the workers are told apart when they were paid for
/// differently (one on a plan, the other per token).
pub fn cost_report(
    ledger: &CostLedger,
    settings: &Settings,
    plans: &std::collections::BTreeMap<String, String>,
) -> Vec<CostLine> {
    let plan_of = |model: &ModelSettings| match resolve_model(&model.model).map(|m| m.provider) {
        Ok(Provider::ClaudeCode) => Some(plan_name(Some(SubscriptionPlan::ClaudeCode))),
        Ok(Provider::OpenAI) if settings.openai_auth != Some(OpenAiAuth::ApiKey) => {
            Some(plan_name(Some(SubscriptionPlan::ChatGpt)))
        }
        _ => None,
    };
    let mut main = Part::default();
    let mut workers = Part::default();
    for entry in &ledger.entries {
        let CostSource::Agent { agent_name, usage } = &entry.source else {
            continue;
        };
        let (part, model) = if agent_name == crate::agents::MAIN {
            (&mut main, &settings.main)
        } else {
            (&mut workers, &settings.worker)
        };
        part.billed += entry.amount;
        part.api += at_api_prices(usage, entry.amount);
        part.input += usage.input_tokens;
        part.output += usage.output_tokens;
        if usage.billing == BillingKind::Subscription {
            part.plan = plan_of(model);
        }
    }
    let line = |text: String| CostLine {
        text,
        detail: false,
    };
    let detail = |text: String| CostLine { text, detail: true };
    let used = |part: &Part| part.input + part.output > 0;
    let mut lines = vec![line(format!(
        "${:.2} billed in this session · {}k input and {}k output tokens",
        main.billed + workers.billed,
        (main.input + workers.input) / 1000,
        (main.output + workers.output) / 1000
    ))];
    let parts = [&main, &workers];
    let paid_alike = !used(&main) || !used(&workers) || main.plan == workers.plan;
    if paid_alike {
        let plan = parts.iter().find_map(|part| part.plan);
        if let Some(plan) = plan {
            lines.push(line(format!(
                "≈ ${:.2} at API prices: the {plan} covers it",
                main.api + workers.api
            )));
        }
        if used(&workers) {
            lines.push(detail(match plan {
                Some(_) => format!(
                    "of which the workers: ≈ ${:.2} at API prices · {}",
                    workers.api,
                    workers.tokens()
                ),
                None => format!(
                    "of which the workers: ${:.2} · {}",
                    workers.billed,
                    workers.tokens()
                ),
            }));
        }
    } else {
        for (name, part) in [("main agent", &main), ("workers", &workers)] {
            lines.push(detail(match part.plan {
                Some(plan) => format!(
                    "{name}: ≈ ${:.2} at API prices, on the {plan} · {}",
                    part.api,
                    part.tokens()
                ),
                None => format!(
                    "{name}: ${:.2} billed per token · {}",
                    part.billed,
                    part.tokens()
                ),
            }));
        }
    }
    // The plans' usage windows, as last reported.
    for part in parts {
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

/// The OpenRouter catalog's base URL, for [`check_openrouter`].
pub fn openrouter_url() -> &'static str {
    arko_ai::providers::openrouter::OPENROUTER_BASE_URL
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::ModelSettings;

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
                .anthropic_cache_ttl,
            Some(AnthropicCacheTtl::OneHour)
        );
        assert_eq!(
            config("cc:opus", Effort::High, false, true)
                .unwrap()
                .anthropic_cache_ttl,
            None,
            "Claude Code chooses its cache"
        );
        // OpenAI: its reasoning and Fast tier.
        let sol = config("sol", Effort::Max, true, true).unwrap();
        assert!(matches!(
            sol.reasoning,
            Some(ReasoningConfig::OpenAI { .. })
        ));
        assert_eq!(sol.openai_service_tier, Some(OpenAIServiceTier::Fast));
        assert_eq!(sol.anthropic_cache_ttl, None);
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
        assert_eq!(claude.anthropic_cache_ttl, Some(AnthropicCacheTtl::OneHour));
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
            500_000
        );
        assert_eq!(
            default_handover(&resolve_model("cc:fable").unwrap(), None),
            500_000
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
        let notes = apply_catalog(&mut session, Ok(&catalog), false).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(session.main.handover_at, Some(500_000));
        assert_eq!(session.worker.handover_at, Some(102_400), "80% of 128k");
        assert_eq!(session.handover_at, 500_000);

        // --handover-at stands.
        let mut session = settings(model("openrouter:meta/small-model"), model("opus"));
        session.main.handover_at = Some(300_000);
        apply_catalog(&mut session, Ok(&catalog), true).unwrap();
        assert_eq!(session.main.handover_at, Some(300_000));

        // A slug the catalog doesn't list, or a model without tools, is
        // refused before the session starts.
        let mut session = settings(model("openrouter:acme/typo"), model("opus"));
        let error = apply_catalog(&mut session, Ok(&catalog), false).unwrap_err();
        assert!(
            error.contains("isn't in OpenRouter's model catalog"),
            "{error}"
        );
        let mut session = settings(model("openrouter:acme/no-tools"), model("opus"));
        let error = apply_catalog(&mut session, Ok(&catalog), false).unwrap_err();
        assert!(error.contains("doesn't take tool calls"), "{error}");

        // Offline: the session starts, at the fallback, and says so.
        let mut session = settings(model("openrouter:meta/small-model"), model("opus"));
        let offline = "connection refused".to_string();
        let notes = apply_catalog(&mut session, Err(&offline), false).unwrap();
        assert_eq!(session.main.handover_at, Some(UNKNOWN_LIMIT_HANDOVER));
        assert!(
            notes[0].contains("Could not read OpenRouter's model catalog"),
            "{notes:?}"
        );
        assert!(notes[0].contains("100k tokens"), "{notes:?}");

        // Sessions without OpenRouter models aren't touched.
        let mut session = settings(model("opus"), model("cc:fable"));
        assert!(
            apply_catalog(&mut session, Ok(&catalog), false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(session.main.handover_at, Some(500_000));
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
        let mut session = settings(model("openrouter:meta/small-model"), model("opus"));
        let notes = check_openrouter(&mut session, false, &url).await.unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(session.main.handover_at, Some(102_400));
        let head = server.await.unwrap();
        assert!(head.starts_with("GET /api/v1/models "), "{head}");
        assert!(
            !head.to_ascii_lowercase().contains("authorization"),
            "no key is sent: {head}"
        );

        // Nothing listens here.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/v1", closed.local_addr().unwrap());
        drop(closed);
        let mut session = settings(model("openrouter:meta/small-model"), model("opus"));
        let notes = check_openrouter(&mut session, false, &url).await.unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
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
        match client_with(session, lookup, Some("/no/such/auth.json".into()), ready) {
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
        let client = client_with(&sol, lookup, None, || Err(claude_code::Problem::Missing))
            .ok()
            .unwrap();
        assert!(!client.is_openai_chatgpt_oauth().await, "the API key");
        sol.openai_auth = Some(OpenAiAuth::ChatGpt);
        let client = client_with(&sol, lookup, None, || Err(claude_code::Problem::Missing))
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
        let missing = client_with(&cc, |_| None, None, || Err(claude_code::Problem::Missing));
        assert!(matches!(
            missing,
            Err(Failure::Need(Need::ClaudeCode { problem: claude_code::Problem::Missing, ref model })) if model == "cc:claude-opus-5-5"
        ));
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
    }

    fn last(main: &str, worker: Option<&str>) -> Option<LastModels> {
        Some(LastModels {
            main: main.into(),
            worker: worker.map(String::from),
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
        assert_eq!(
            LastModels::load(root.path()),
            last("cc:claude-opus-5-5", None)
        );
        session.worker = model("flash");
        LastModels::remember(root.path(), &session).unwrap();
        assert_eq!(
            LastModels::load(root.path()),
            last("cc:claude-opus-5-5", Some("gemini-3.8-flash"))
        );
        std::fs::write(root.path().join(LAST_MODELS_FILE), "not json").unwrap();
        assert_eq!(
            LastModels::load(root.path()),
            None,
            "a damaged file is no default"
        );
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
        cost_report(&CostLedger { entries }, session, &plans)
            .into_iter()
            .map(|line| (line.detail, line.text))
            .collect()
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
                    "≈ $3.40 at API prices: the Claude plan covers it".to_string()
                ),
                (
                    true,
                    "of which the workers: ≈ $0.40 at API prices · 30k input and 2k output tokens"
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
            "≈ $0.90 at API prices: the ChatGPT plan covers it"
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
                    "of which the workers: $0.20 · 30k input and 2k output tokens".to_string()
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
                (true, "main agent: ≈ $3.00 at API prices, on the Claude plan · 120k input and 8k output tokens".to_string()),
                (true, "workers: $0.50 billed per token · 30k input and 2k output tokens".to_string()),
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

    /// The picker offers only the efforts each model takes, and a model
    /// switched to without one gets the nearest to the effort before.
    #[test]
    fn each_model_takes_its_own_efforts() {
        use Effort::{High, Low, Max, Medium, Xhigh};
        let efforts = |name: &str| efforts(&resolve_model(name).unwrap());
        assert_eq!(efforts("opus"), [Low, Medium, High, Xhigh, Max]);
        assert_eq!(efforts("cc:fable"), [Low, Medium, High, Xhigh, Max]);
        assert_eq!(efforts("sol"), [Low, Medium, High, Xhigh, Max]);
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
            ["gpt-6.1-sol", "gpt-6-astra", "gpt-6-luna", "gpt-5.6-terra"]
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
}

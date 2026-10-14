//! The providers fin runs its agents on, and how models are named.
//!
//! A model ID from arko's registry (`claude-opus-5-5`, `gpt-6.1-sol`,
//! `gemini-3.1-pro-preview`) or an alias (`opus`, `sol`, `gemini`) names a
//! model at its own provider: Claude models with an Anthropic API key,
//! OpenAI models with a ChatGPT login or an OpenAI API key, Gemini models
//! with a Google Gemini API key. A prefix names another route:
//! `openrouter:<slug>` any model OpenRouter serves
//! (`openrouter:anthropic/claude-opus-5.5`), on an OpenRouter key.
//!
//! Plain names stay as they were: a registry ID names one model at one
//! provider (IDs are unique across the registry). A prefix appears only
//! where the same model can be reached another way, and it says which
//! account pays. A session saves the name in this form
//! (`ModelSettings::model`), so it reads the same in `session.json`, in
//! `fin --sessions` and on the command line.
//!
//! OpenAI models are the exception: arko's client holds one OpenAI client,
//! so a session reaches them all one way, its [`OpenAiAuth`], saved with
//! the session.

use std::path::Path;
use std::time::Duration;

use arko_ai::model::ModelType;
use arko_ai::{
    AnthropicCacheTtl, CodexAuthFile, GeminiThinkingLevel, GenerationConfig, LLMClient, Model,
    OpenAIServiceTier, OpenRouterCatalog, OpenRouterReasoning, PlanUsage, Provider,
    ReasoningConfig, ReasoningEffort, ReasoningSummary, ServiceTier, SubscriptionPlan,
};
use serde::{Deserialize, Serialize};

use crate::agents::{Effort, ModelSettings, default_handover};
use crate::credentials::{self, Need};
use crate::session::Settings;

/// The prefix of a model on OpenRouter: `openrouter:<slug>`.
pub const OPENROUTER: &str = "openrouter:";

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

/// The handover size of a model whose input limit fin can't tell (an
/// OpenRouter model when its catalog can't be read): 80% of 128k, the
/// smallest context window common among the models OpenRouter serves.
pub const UNKNOWN_LIMIT_HANDOVER: usize = 100_000;

/// The model `name` names: a registry ID or alias, or `openrouter:<slug>`.
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
    let mut text = format!("`{name}` isn't a model fin knows.");
    if name.contains('/') {
        text.push_str(&format!(
            " Models on OpenRouter take its prefix: openrouter:{name}."
        ));
    } else if let Some(close) = suggestion(name) {
        text.push_str(&format!(" Did you mean {close}?"));
    }
    text.push_str(&format!(
        " Use a model ID, an alias ({}), or openrouter:<slug> for a model on OpenRouter.",
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
        _ => model.id.clone(),
    }
}

/// A Claude model on OpenRouter: `anthropic/…`, or a `~anthropic/…-latest`
/// alias.
pub fn is_claude_slug(slug: &str) -> bool {
    slug.trim_start_matches('~').starts_with("anthropic/")
}

/// Whether `model` takes Anthropic's prompt-cache lifetime (the 1-hour
/// cache): Claude models on an API key or on OpenRouter.
pub fn takes_cache_ttl(model: &Model) -> bool {
    match model.provider {
        Provider::Anthropic => true,
        Provider::OpenRouter => is_claude_slug(&model.id),
        _ => false,
    }
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
                    "{} has no fast mode; Anthropic offers it on Claude Opus 5.5, Opus 5 and Opus 4.8",
                    model_name(model)
                ));
            }
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
/// FIN_CODEX_AUTH or Codex's own) or the OPENAI_OAUTH_* variables.
pub fn has_chatgpt_login(codex_auth: Option<&Path>) -> bool {
    credentials::codex_auth(codex_auth).is_some()
        || credentials::var("OPENAI_OAUTH_ACCESS_TOKEN").is_some()
        || CodexAuthFile::default_path().is_some_and(|path| path.is_file())
}

/// How a new session reaches OpenAI models: `flag`, else FIN_OPENAI_AUTH,
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
            .map_err(|_| format!("FIN_OPENAI_AUTH is `{value}`; set it to chatgpt or api-key"));
    }
    let login = has_chatgpt_login(codex_auth);
    let key = credentials::var(credentials::OPENAI_KEY).is_some();
    Ok(match (login, key) {
        (false, true) => (OpenAiAuth::ApiKey, false),
        (login, key) => (OpenAiAuth::ChatGpt, login && key),
    })
}

/// The model of a new session without `--model`, by the credentials there
/// are: Claude Opus 5.5 with an Anthropic API key, else GPT-6.1 Sol with a
/// ChatGPT login, else Opus on OpenRouter, Sol on an OpenAI API key, or
/// Gemini 3.1 Pro on a Gemini key; `None` without any.
pub fn default_model(codex_auth: Option<&Path>) -> Option<&'static str> {
    if credentials::var(credentials::ANTHROPIC_KEY).is_some() {
        Some("claude-opus-5-5")
    } else if has_chatgpt_login(codex_auth) {
        Some("gpt-6.1-sol")
    } else if credentials::var(credentials::OPENROUTER_KEY).is_some() {
        Some("openrouter:anthropic/claude-opus-5.5")
    } else if credentials::var(credentials::OPENAI_KEY).is_some() {
        Some("gpt-6.1-sol")
    } else if gemini_key().is_some() {
        Some("gemini-3.1-pro-preview")
    } else {
        None
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
/// credentials it needs, and OpenAI as the session's [`OpenAiAuth`] says
/// (never both ways).
pub fn client(codex_auth: Option<&Path>, settings: &Settings) -> Result<LLMClient, Failure> {
    client_with(
        settings,
        credentials::var,
        credentials::codex_auth(codex_auth),
    )
}

/// [`client`], with the settings read through `var` and the Codex
/// auth.json the user named (else Codex's own).
fn client_with(
    settings: &Settings,
    var: impl Fn(&str) -> Option<String>,
    auth_file: Option<std::path::PathBuf>,
) -> Result<LLMClient, Failure> {
    let models = [&settings.main, &settings.worker]
        .into_iter()
        .map(|model| Ok((model, resolve_model(&model.model)?)))
        .collect::<Result<Vec<(&ModelSettings, Model)>, String>>()
        .map_err(Failure::Other)?;
    let uses = |provider: Provider| models.iter().any(|(_, model)| model.provider == provider);
    // Sessions saved before the choice existed reached OpenAI models over
    // the ChatGPT login.
    let auth = uses(Provider::OpenAI).then(|| settings.openai_auth.unwrap_or(OpenAiAuth::ChatGpt));
    let mut client = LLMClient::from_env_with(|name| {
        let wanted = if name.starts_with("OPENAI_OAUTH_") {
            auth == Some(OpenAiAuth::ChatGpt) && auth_file.is_none()
        } else if name == credentials::OPENAI_KEY {
            auth == Some(OpenAiAuth::ApiKey)
        } else {
            true
        };
        wanted.then(|| var(name)).flatten()
    })
    .map_err(|error| Failure::Other(error.to_string()))?;
    if auth == Some(OpenAiAuth::ChatGpt) && !client.has_provider(&Provider::OpenAI) {
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
                "{} holds no ChatGPT login ({error}); sign in with `codex login`",
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
    Ok(client)
}

/// How long fin waits for OpenRouter's catalog.
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
                        "Could not read OpenRouter's model catalog ({error}), so fin can't check {name} or tell its context size: it hands over at {}k tokens (--handover-at sets it).",
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
                    "fin can't tell the context size of the preset {name}: it hands over at {}k tokens (--handover-at sets it).",
                    handover(model)
                ),
            );
            continue;
        }
        let Some(listed) = catalog.get(&slug) else {
            return Err(format!(
                "{name} isn't in OpenRouter's model catalog; https://openrouter.ai/models lists the slugs (such as anthropic/claude-opus-5.5)"
            ));
        };
        if listed
            .openrouter_info()
            .is_some_and(|info| !info.supports_parameter("tools"))
        {
            return Err(format!(
                "{name} doesn't take tool calls on OpenRouter, and fin's agents work through tools"
            ));
        }
        if !explicit_handover {
            model.handover_at = Some(default_handover(&listed));
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
    let plan = match usage.plan {
        Some(SubscriptionPlan::ClaudeCode) => "Claude plan",
        // Reports saved before plans were named came from ChatGPT.
        Some(SubscriptionPlan::ChatGpt) | None => "ChatGPT plan",
    };
    Some(format!("{plan} {}", windows.join(", ")))
}

/// Every plan's latest usage, in one line: a session with agents on both
/// plans shows both.
pub fn plan_usage_line(plans: &std::collections::BTreeMap<String, String>) -> Option<String> {
    (!plans.is_empty()).then(|| plans.values().cloned().collect::<Vec<_>>().join(" · "))
}

/// What the agents' configurations take from the session's providers
/// besides their models.
#[derive(Debug, Clone)]
pub struct Setup {
    /// How the session reaches OpenAI models.
    pub openai_auth: Option<OpenAiAuth>,
}

impl Setup {
    pub fn new(openai_auth: Option<OpenAiAuth>) -> Self {
        Self { openai_auth }
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
            handover_at: Some(default_handover(&resolve_model(name).unwrap())),
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
        ] {
            let model = resolve_model(name).unwrap();
            assert_eq!(model.provider, provider, "{name}");
            let saved_name = model_name(&model);
            assert_eq!(saved_name, saved, "{name}");
            // The saved name names the same model.
            let again = resolve_model(&saved_name).unwrap();
            assert_eq!((again.provider, again.id), (model.provider, model.id));
        }
    }

    #[test]
    fn a_name_that_names_no_model_says_what_was_meant() {
        let error = resolve_model("opsu").unwrap_err();
        assert!(error.contains("Did you mean opus?"), "{error}");
        assert!(error.contains("openrouter:<slug>"), "{error}");
        let error = resolve_model("anthropic/claude-opus-5.5").unwrap_err();
        assert!(
            error.contains("openrouter:anthropic/claude-opus-5.5"),
            "{error}"
        );
        let error = resolve_model("claude-opus-5.5").unwrap_err();
        assert!(error.contains("Did you mean claude-opus-5-5?"), "{error}");
        assert!(resolve_model("openrouter:").is_err());
        assert!(resolve_model("no-such-model-at-all").is_err());
    }

    #[test]
    fn each_provider_gets_its_own_reasoning_fast_mode_and_cache() {
        let config = |name: &str, effort: Effort, fast: bool, cache_1h: bool| {
            generation(&resolve_model(name).unwrap(), effort, fast, cache_1h, false)
        };
        // Claude: adaptive thinking at the effort, fast mode on Opus, the
        // 1-hour cache.
        for name in ["opus"] {
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
        assert_eq!(default_handover(&resolve_model("opus").unwrap()), 500_000);
        assert_eq!(default_handover(&resolve_model("sol").unwrap()), 200_000);
        assert_eq!(default_handover(&resolve_model("gemini").unwrap()), 200_000);
        // An OpenRouter model without its catalog has no known limit.
        assert_eq!(
            default_handover(&resolve_model("openrouter:x/y").unwrap()),
            UNKNOWN_LIMIT_HANDOVER
        );
        let mut small = resolve_model("openrouter:anthropic/claude-opus-5.5").unwrap();
        small.max_input_tokens = 200_000;
        assert_eq!(default_handover(&small), 160_000);
        small.max_input_tokens = 1_000_000;
        assert_eq!(default_handover(&small), 500_000, "Claude on OpenRouter");
    }

    fn settings(main: ModelSettings, worker: ModelSettings) -> Settings {
        Settings {
            workspace: "/work".into(),
            handover_at: main.handover_at.unwrap_or(200_000),
            main,
            worker,
            max_workers: 4,
            allow_outside_workspace: false,
            show_thinking: false,
            created: chrono::Utc::now(),
            title: None,
            windows_shell: None,
            openai_auth: None,
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
        let mut session = settings(model("opus"), model("fable"));
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

    /// What `client_with` needs when nothing is set and the ChatGPT login
    /// is a file that isn't there.
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
        match client_with(session, lookup, Some("/no/such/auth.json".into())) {
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
        let client = client_with(&sol, lookup, None).ok().unwrap();
        assert!(!client.is_openai_chatgpt_oauth().await, "the API key");
        sol.openai_auth = Some(OpenAiAuth::ChatGpt);
        let client = client_with(&sol, lookup, None).ok().unwrap();
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
}

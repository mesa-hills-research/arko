//! A finished worker's prompt cache, and what continuing it costs.
//!
//! Continuing a worker sends its whole conversation again. While its prompt
//! cache lasts, that is read from the cache at about a tenth of the input
//! price. Once the cache has expired, the conversation is read again at the
//! full input price and written to the cache again, which on Claude costs
//! more than the input price (1.25 times for the 5-minute cache, twice for
//! the hour). The agents, and the user, are asked before continuing a
//! worker whose cache expired, when its conversation is large enough for
//! that to matter ([`CONFIRM_TOKENS`]).
//!
//! The cache lasts from the run's latest model request, which the run's
//! task records with its input (`TaskInfo::latest_request`). So a session
//! that resumes knows each worker's cache at once. A worker an older Arko
//! saved has no record, so continuing it asks first ([`WorkerCacheState`]).

use std::time::Duration;

use arko_ai::Provider;
use arko_ai::prompt_cache::PromptCacheLifetime;
use chrono::{DateTime, Utc};

use crate::details::SwitchCost;
use crate::providers::{self, OpenAiAuth};

/// Below this many tokens, re-reading a worker's conversation without its
/// cache costs little, so continuing it asks nothing.
pub const CONFIRM_TOKENS: usize = 30_000;

/// The `message_agent` parameter that continues a worker whose cache
/// expired after all.
pub const CONFIRM_PARAM: &str = "confirm_uncached";

/// How long Claude Code keeps a conversation's cache when its run asked for
/// no lifetime: an hour on a Claude plan.
const CLAUDE_CODE_DEFAULT: Duration = Duration::from_secs(3_600);

/// A finished worker's prompt cache, and what continuing the worker costs.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerCache {
    /// The worker, such as `r3`.
    pub worker: String,
    /// When its cache expires, or expired: its last request, plus the
    /// cache lifetime its run used, less the safety margin
    /// ([`lapses_at`](crate::cache_timer::lapses_at)).
    pub expires_at: DateTime<Utc>,
    /// What continuing it re-reads, and writes to the cache again: its
    /// conversation's input at its last request, and what that costs at its
    /// model's price, or the plan that pays.
    pub cost: SwitchCost,
}

impl WorkerCache {
    /// Whether its cache has expired at `now`.
    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }

    /// Whether continuing it at `now` asks first: its cache has expired, and
    /// its conversation holds at least [`CONFIRM_TOKENS`].
    pub fn asks(&self, now: DateTime<Utc>) -> bool {
        self.expired(now) && self.cost.tokens >= CONFIRM_TOKENS
    }

    /// What continuing it costs, as the warning says it: "r3's cache
    /// expired 2 hours ago. Continuing re-reads about 340k tokens without it
    /// (≈ $1.70 at API prices)."
    pub fn warning(&self, now: DateTime<Utc>) -> String {
        let tokens = short_tokens(self.cost.tokens);
        let cost = match (self.cost.dollars, self.cost.plan) {
            (Some(dollars), _) => format!(" (≈ ${dollars:.2} at API prices)"),
            (None, Some(plan)) => format!(", using {plan}"),
            (None, None) => String::new(),
        };
        format!(
            "{}'s cache expired {}. Continuing re-reads about {tokens} tokens without it{cost}.",
            self.worker,
            ago(now - self.expires_at)
        )
    }

    /// Its cache in a few words, for `list_agents`: "cache expired 2 h ago,
    /// continuing re-reads about 340k tokens", or "cache lasts 12 more min,
    /// about 340k tokens".
    pub fn line(&self, now: DateTime<Utc>) -> String {
        let tokens = short_tokens(self.cost.tokens);
        if self.expired(now) {
            format!(
                "cache expired {} ago, continuing re-reads about {tokens} tokens",
                short_span(now - self.expires_at)
            )
        } else {
            format!(
                "cache lasts {} more, about {tokens} tokens",
                short_span(self.expires_at - now)
            )
        }
    }

    /// Why `message_agent` didn't continue it, and what to do instead.
    pub fn refusal(&self, now: DateTime<Utc>) -> String {
        format!(
            "{} Start a fresh worker for this, or call message_agent again with {CONFIRM_PARAM}: true to continue {}.",
            self.warning(now),
            self.worker
        )
    }
}

/// A finished worker's prompt cache as the session knows it.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkerCacheState {
    /// Its latest request is recorded, so its cache is known.
    Known(WorkerCache),
    /// The worker (such as `r3`) was saved by an older Arko, which didn't
    /// record its latest request, so continuing it asks first.
    Unknown(String),
}

impl WorkerCacheState {
    /// Whether continuing the worker at `now` asks first.
    pub fn asks(&self, now: DateTime<Utc>) -> bool {
        match self {
            Self::Known(cache) => cache.asks(now),
            Self::Unknown(_) => true,
        }
    }

    /// What continuing it costs, as the warning says it.
    pub fn warning(&self, now: DateTime<Utc>) -> String {
        match self {
            Self::Known(cache) => cache.warning(now),
            Self::Unknown(worker) => format!(
                "{worker}'s cache isn't known, because an older Arko saved it. Continuing may re-read its whole conversation without it."
            ),
        }
    }

    /// Why `message_agent` didn't continue it, and what to do instead.
    pub fn refusal(&self, now: DateTime<Utc>) -> String {
        match self {
            Self::Known(cache) => cache.refusal(now),
            Self::Unknown(worker) => format!(
                "{} Start a fresh worker for this, or call message_agent again with {CONFIRM_PARAM}: true to continue {worker}.",
                self.warning(now)
            ),
        }
    }
}

/// The prompt cache of a worker run whose latest request was sent at
/// `last_request`, with `tokens` of input, on `llm` (its model and
/// generation settings), in a session that reaches OpenAI models by
/// `openai_auth`. Its lifetime is the one the request's cache writes showed
/// (`recorded_lifetime`, a Claude Code model's), else the one its settings
/// give. `None` when its cache lifetime isn't known (Gemini, OpenRouter
/// models other than Claude) or it has none.
pub(crate) fn of(
    worker: &str,
    llm: &arko_agents::AgentLLMConfig,
    last_request: DateTime<Utc>,
    tokens: usize,
    recorded_lifetime: Option<Duration>,
    openai_auth: Option<OpenAiAuth>,
) -> Option<WorkerCache> {
    let model = &llm.model;
    let generation = &llm.generation_config;
    let chatgpt = model.provider == Provider::OpenAI && openai_auth != Some(OpenAiAuth::ApiKey);
    let configured =
        || match arko_ai::prompt_cache::prompt_cache_lifetime(model, generation, chatgpt) {
            PromptCacheLifetime::Known(lifetime) => Some(lifetime),
            PromptCacheLifetime::Unknown if model.provider == Provider::ClaudeCode => {
                Some(CLAUDE_CODE_DEFAULT)
            }
            _ => None,
        };
    let lifetime = recorded_lifetime.or_else(configured)?;
    let fast = generation.anthropic_fast_mode == Some(true)
        || generation.openai_service_tier == Some(arko_ai::OpenAIServiceTier::Fast);
    let long_cache = generation.long_prompt_cache == Some(true);
    let lapses = crate::cache_timer::lapses_at(last_request.timestamp_millis(), lifetime);
    Some(WorkerCache {
        worker: worker.to_string(),
        expires_at: DateTime::from_timestamp_millis(lapses)?,
        cost: reread_cost(model, tokens, fast, long_cache, chatgpt),
    })
}

/// What re-reading `tokens` of a conversation on `model` without its cache
/// costs, writing them to the cache again: at the model's cache write price
/// (`fast` mode's, and the long cache's with `long_cache`), the same price
/// `/cost` uses, or the plan that pays when one does (an OpenAI model's
/// when the session reaches it on the ChatGPT login, `chatgpt`).
pub(crate) fn reread_cost(
    model: &arko_ai::Model,
    tokens: usize,
    fast: bool,
    long_cache: bool,
    chatgpt: bool,
) -> SwitchCost {
    let plan = match model.provider {
        Provider::ClaudeCode => Some("your Claude plan"),
        Provider::OpenAI if chatgpt => Some("your ChatGPT plan"),
        _ => None,
    };
    let dollars = plan.is_none().then(|| {
        let (write, _) = providers::cache_prices(model, tokens, fast, long_cache);
        tokens as f64 * write / 1e6
    });
    SwitchCost {
        tokens,
        dollars,
        plan,
    }
}

/// The worker and the message of a `/message` line, as the session reads
/// it: "/message r3 Go on." gives `r3` and "Go on.". A tab separates as a
/// space does. `None` for any other line, and for a `/message` without both.
pub fn message_parts(line: &str) -> Option<(&str, &str)> {
    let (name, argument) = crate::harness::split_live_command(line.trim())?;
    if name != "/message" {
        return None;
    }
    let (to, text) = argument.split_once(char::is_whitespace)?;
    let text = text.trim();
    (!to.is_empty() && !text.is_empty()).then_some((to, text))
}

/// A count of tokens, short: "820", "8.2k", "340k", "1.2M".
pub fn short_tokens(tokens: usize) -> String {
    let value = tokens as f64;
    let trimmed = |value: f64| {
        let text = format!("{value:.1}");
        text.strip_suffix(".0").map_or(text.clone(), str::to_string)
    };
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 9_950 {
        format!("{}k", trimmed(value / 1_000.))
    } else if tokens < 999_500 {
        format!("{}k", (value / 1_000.).round())
    } else {
        format!("{}M", trimmed(value / 1_000_000.))
    }
}

/// How long ago `span` was, as a sentence says it: "just now", "5 minutes
/// ago", "2 hours ago", "3 days ago".
fn ago(span: chrono::Duration) -> String {
    let minutes = span.num_minutes().max(0);
    let counted =
        |count: i64, unit: &str| format!("{count} {unit}{} ago", if count == 1 { "" } else { "s" });
    match minutes {
        0 => "just now".to_string(),
        1..60 => counted(minutes, "minute"),
        60..2_880 => counted(minutes / 60, "hour"),
        _ => counted(minutes / 1_440, "day"),
    }
}

/// `span` in a few letters: "1 min", "12 min", "2 h", "3 d".
fn short_span(span: chrono::Duration) -> String {
    let minutes = span.num_minutes().max(1);
    match minutes {
        1..60 => format!("{minutes} min"),
        60..2_880 => format!("{} h", minutes / 60),
        _ => format!("{} d", minutes / 1_440),
    }
}

#[cfg(test)]
mod tests;

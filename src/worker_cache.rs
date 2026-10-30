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
//! The cache lasts from the run's latest model request, which [`Requests`]
//! keeps for each worker run in the session's folder, with the request's
//! input. So a session that resumes knows each worker's cache at once.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use arko_ai::Provider;
use arko_ai::prompt_cache::PromptCacheLifetime;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::details::SwitchCost;
use crate::lock;
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
    /// cache lifetime its run used.
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

/// The prompt cache of a worker run whose latest request went out at
/// `last_request`, with `tokens` of input, on `llm` (its model and
/// generation settings), in a session that reaches OpenAI models by
/// `openai_auth`. `None` when its cache lifetime isn't known (Gemini,
/// OpenRouter models other than Claude) or it has none.
pub(crate) fn of(
    worker: &str,
    llm: &arko_agents::AgentLLMConfig,
    last_request: DateTime<Utc>,
    tokens: usize,
    openai_auth: Option<OpenAiAuth>,
) -> Option<WorkerCache> {
    let model = &llm.model;
    let generation = &llm.generation_config;
    let chatgpt = model.provider == Provider::OpenAI && openai_auth != Some(OpenAiAuth::ApiKey);
    let lifetime = match arko_ai::prompt_cache::prompt_cache_lifetime(model, generation, chatgpt) {
        PromptCacheLifetime::Known(lifetime) => lifetime,
        PromptCacheLifetime::Unknown if model.provider == Provider::ClaudeCode => {
            CLAUDE_CODE_DEFAULT
        }
        _ => return None,
    };
    let fast = generation.anthropic_fast_mode == Some(true)
        || generation.openai_service_tier == Some(arko_ai::OpenAIServiceTier::Fast);
    let long_cache = generation.long_prompt_cache == Some(true);
    Some(WorkerCache {
        worker: worker.to_string(),
        expires_at: last_request + chrono::Duration::from_std(lifetime).ok()?,
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

/// The file in the session's folder that keeps each worker run's latest
/// request.
const REQUESTS_FILE: &str = "worker-requests.json";

/// A worker run's latest model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LastRequest {
    /// When it went out (Unix milliseconds). The cache lasts from then.
    pub at: i64,
    /// Its input, in tokens, which continuing the run sends again.
    pub input: usize,
}

/// Each worker run's latest model request, kept in memory and in the
/// session's folder. A request counts once it has ended (its usage came
/// back), from the time it went out. So a run that stopped during a
/// request counts from the one before, which can only say the cache lapses
/// sooner than it does.
#[derive(Debug)]
pub(crate) struct Requests {
    path: PathBuf,
    /// The requests that went out and haven't ended, by run, with when they
    /// went out.
    going: Mutex<HashMap<Uuid, i64>>,
    ended: Mutex<BTreeMap<Uuid, LastRequest>>,
}

impl Requests {
    /// The record in the session's folder `dir`. A missing or damaged one is
    /// empty, as for a session saved before the record existed.
    pub(crate) fn load(dir: &Path) -> Self {
        let path = dir.join(REQUESTS_FILE);
        let ended = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            going: Mutex::default(),
            ended: Mutex::new(ended),
        }
    }

    /// A request of `run` went out at `at` (Unix milliseconds).
    pub(crate) fn started(&self, run: Uuid, at: i64) {
        lock(&self.going).insert(run, at);
    }

    /// The request of `run` that went out last ended with `input` tokens of
    /// input. It is kept, and the record saved.
    pub(crate) fn ended(&self, run: Uuid, input: usize) {
        let Some(at) = lock(&self.going).remove(&run) else {
            return;
        };
        self.put([(run, LastRequest { at, input })]);
    }

    /// The latest request of `run` that ended.
    pub(crate) fn get(&self, run: Uuid) -> Option<LastRequest> {
        lock(&self.ended).get(&run).copied()
    }

    /// Keeps `requests` as their runs' latest, and saves the record.
    pub(crate) fn put(&self, requests: impl IntoIterator<Item = (Uuid, LastRequest)>) {
        let mut ended = lock(&self.ended);
        ended.extend(requests);
        let written = serde_json::to_vec_pretty(&*ended)
            .map_err(std::io::Error::other)
            .and_then(|text| crate::session::write_whole(&self.path, &text));
        if let Err(error) = written {
            log::warn!(
                "could not keep the workers' latest requests in {} ({error})",
                self.path.display()
            );
        }
    }
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

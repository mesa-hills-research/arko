//! The API keys while fin runs: `/keys` lists the providers' keys, and adds,
//! replaces and removes them. A key is checked with its provider first (a
//! free request where the provider has one), saved in fin's `.env` as the
//! setup screen saves, and used at once: the session's model client gets the
//! provider for the agents' next requests, and the search indexes embed with
//! a Voyage key. It travels from the screen to here as a [`Secret`], so it
//! never reaches the input history, the transcript, the feed or the logs.
//!
//! A replaced key is used from the next request on, by the agents already
//! running too (the old one may be why it was replaced). A removed key is
//! kept by the agents already using it until fin quits, and nothing new
//! starts on it: the session stops offering its provider, new workers can't
//! start on it, and searches go back to keywords.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use arko_ai::{LLMClient, Model, Provider};

use crate::credentials::{self, Key, KeySource, KeyState};
use crate::index;
use crate::providers::{self, OpenAiAuth};
use crate::remote::{self, Level, Source};

/// A key's value. It is never shown: `Debug` writes `Secret(…)`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, to save and use.
    pub fn reveal(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many characters it has.
    pub fn len(&self) -> usize {
        self.0.chars().count()
    }

    pub fn push(&mut self, c: char) {
        self.0.push(c);
    }

    pub fn pop(&mut self) {
        self.0.pop();
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// What a check of a key with its provider found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    /// The provider took it.
    Valid,
    /// The provider refused it: it isn't saved.
    Refused(String),
    /// The provider couldn't be asked, or didn't say: the key is saved
    /// unchecked, and this says why.
    Unchecked(String),
}

/// What adding or removing a key did, for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Saved or removed: what was done, and what to warn about.
    Done { said: String, warnings: Vec<String> },
    /// Refused by the provider, or not saved: nothing changed.
    Failed(String),
}

/// How long a check waits for the provider.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The Voyage model the check embeds one word with (the docs indexes').
const VOYAGE_CHECK_MODEL: &str = index::DOCS_MODEL;

/// The session's API keys: what the model client and the search indexes use,
/// and which providers the session reaches.
pub struct Keys {
    client: Arc<LLMClient>,
    indexes: Option<Arc<index::Indexes>>,
    feed: Option<remote::Feed>,
    /// Where the keys are checked and the providers reached: the providers'
    /// own addresses, or (tests) a local server for all of them.
    local: std::sync::OnceLock<String>,
    state: Mutex<State>,
}

struct State {
    /// How the client reaches OpenAI models; `None` when it doesn't.
    openai: Option<OpenAiAuth>,
    /// The providers whose key was removed with no other to take its place.
    withdrawn: Vec<Provider>,
}

impl Keys {
    /// The keys of a session whose model client is `client` (which reaches
    /// OpenAI models as `openai` says: see [`openai_way`]) and search indexes
    /// `indexes`. `feed` gets a notice of each change.
    pub fn new(
        client: Arc<LLMClient>,
        openai: Option<OpenAiAuth>,
        indexes: Option<Arc<index::Indexes>>,
        feed: Option<remote::Feed>,
    ) -> Arc<Self> {
        Arc::new(Self {
            client,
            indexes,
            feed,
            local: std::sync::OnceLock::new(),
            state: Mutex::new(State {
                openai,
                withdrawn: Vec::new(),
            }),
        })
    }

    /// Checks keys with, and builds the providers' clients for, the server
    /// at `url` (tests, and debug builds on a local test model server).
    #[cfg(any(test, debug_assertions))]
    pub fn use_local(&self, url: &str) {
        let _ = self.local.set(url.to_string());
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether the session reaches `provider` for anything that starts now.
    pub fn reaches(&self, provider: &Provider) -> bool {
        self.client.has_provider(provider) && !self.state().withdrawn.contains(provider)
    }

    /// The providers the session reaches, as `remote::Models::reachable`
    /// names them.
    pub fn reachable(&self) -> Vec<String> {
        [
            Provider::Anthropic,
            Provider::OpenAI,
            Provider::Google,
            Provider::OpenRouter,
        ]
        .iter()
        .filter(|provider| self.reaches(provider))
        .map(|provider| providers::provider_key(provider).to_string())
        .collect()
    }

    /// How the session reaches OpenAI models, when it does.
    pub fn openai_auth(&self) -> Option<OpenAiAuth> {
        let state = self.state();
        state
            .openai
            .filter(|_| !state.withdrawn.contains(&Provider::OpenAI))
    }

    /// Why a new worker can't start on `model`: its provider's key was
    /// removed while fin runs. (Said to the main agent.)
    pub fn refusal(&self, model: &Model) -> Option<String> {
        if !self.state().withdrawn.contains(&model.provider) {
            return None;
        }
        let name = match model.provider {
            Provider::Anthropic => credentials::ANTHROPIC_KEY,
            Provider::OpenAI => credentials::OPENAI_KEY,
            Provider::Google => credentials::GEMINI_KEY,
            _ => credentials::OPENROUTER_KEY,
        };
        Some(format!(
            "New workers can't start on {}: the user removed its API key ({name}) while fin runs. Ask the user to add one with /keys, or to choose another worker model with /model worker.",
            providers::model_name(model),
        ))
    }

    /// Adds `key` (or replaces it) with `value`: checked with its provider,
    /// saved in fin's `.env`, and used at once.
    pub async fn add(&self, key: &'static Key, value: Secret) -> Outcome {
        let value = value.reveal().trim();
        if let Err(problem) = credentials::check_pasted(value, key) {
            return Outcome::Failed(problem);
        }
        let checked = check(key, value, self.local.get().map(String::as_str)).await;
        let mut warnings = Vec::new();
        match &checked {
            Checked::Refused(why) => return Outcome::Failed(why.clone()),
            Checked::Unchecked(why) => {
                warnings.push(why.clone());
                warnings.extend(credentials::prefix_note(value, key));
            }
            Checked::Valid => {}
        }
        let saved = match credentials::save_key(key, value) {
            Ok(saved) => saved,
            Err(error) => return Outcome::Failed(capitalized(&error)),
        };
        log::info!("keys: {} saved and used", key.name);
        let now = self.apply(key, Some(value));
        let said = match checked {
            Checked::Valid => format!("{} accepted the key, and fin saved it. {now}", key.provider),
            _ => format!("fin saved the {}. {now}", without_article(key.what)),
        };
        warnings.extend(saved.note);
        self.notice(&format!(
            "{} added. {now}",
            capitalized(without_article(key.what))
        ));
        Outcome::Done { said, warnings }
    }

    /// Removes `key`, when fin saved the one in use: from fin's `.env`, and
    /// for what starts from now on. A key another source sets takes its
    /// place.
    pub fn remove(&self, key: &'static Key) -> Outcome {
        let after = match credentials::remove_key(key) {
            Ok(after) => after,
            Err(error) => return Outcome::Failed(capitalized(&error)),
        };
        log::info!("keys: {} removed", key.name);
        let value = credentials::key_value(key);
        let now = self.apply(key, value.as_deref());
        let what = without_article(key.what);
        let said = match (&after.source, value.is_some()) {
            (Some(source), true) => format!(
                "fin removed the {what} from its .env, and now uses {}.",
                credentials::source_text(source, after.variable)
            ),
            _ => format!("fin removed the {what} from its .env. {now}"),
        };
        self.notice(&format!(
            "{} removed.",
            capitalized(without_article(key.what))
        ));
        Outcome::Done {
            said,
            warnings: Vec::new(),
        }
    }

    /// Uses `value` as `key` from now on (none: the key was removed, and
    /// none takes its place). Returns what that means, as a sentence.
    pub(crate) fn apply(&self, key: &Key, value: Option<&str>) -> String {
        let provider = match key.name {
            credentials::ANTHROPIC_KEY => Provider::Anthropic,
            credentials::OPENAI_KEY => Provider::OpenAI,
            credentials::GEMINI_KEY => Provider::Google,
            credentials::OPENROUTER_KEY => Provider::OpenRouter,
            _ => {
                if let Some(indexes) = &self.indexes {
                    indexes.set_voyage_key(value.map(str::to_string));
                }
                return match (value, &self.indexes) {
                    (Some(_), Some(_)) => "The search indexes use it now.",
                    // (This session's indexes couldn't be opened.)
                    (Some(_), None) => "New sessions use it for the search indexes.",
                    (None, _) => "Searches started from now on match keywords only.",
                }
                .to_string();
            }
        };
        let mut state = self.state();
        if provider == Provider::OpenAI && state.openai == Some(OpenAiAuth::ChatGpt) {
            return match value {
                Some(_) => "This session reaches OpenAI models with your ChatGPT login. A new session started with --openai-auth api-key uses the key.",
                None => "This session reaches OpenAI models with your ChatGPT login.",
            }
            .to_string();
        }
        let Some(value) = value else {
            if self.client.has_provider(&provider) && !state.withdrawn.contains(&provider) {
                state.withdrawn.push(provider);
            }
            return "Agents already using it keep it until fin quits.".to_string();
        };
        match provider {
            Provider::Anthropic => self.client.set_anthropic_client(self.local_url(
                arko_ai::providers::anthropic::AnthropicClient::new(value),
                arko_ai::providers::anthropic::AnthropicClient::with_base_url,
            )),
            Provider::OpenAI => {
                self.client.set_openai_client(self.local_url(
                    arko_ai::providers::openai::OpenAIClient::new(value),
                    arko_ai::providers::openai::OpenAIClient::with_base_url,
                ));
                state.openai = Some(OpenAiAuth::ApiKey);
            }
            Provider::Google => self.client.set_google_client(self.local_url(
                arko_ai::providers::google::GoogleClient::new(value),
                arko_ai::providers::google::GoogleClient::with_base_url,
            )),
            _ => self.client.set_openrouter_client(self.local_url(
                arko_ai::OpenRouterClient::new(value),
                arko_ai::OpenRouterClient::with_base_url,
            )),
        }
        state.withdrawn.retain(|withdrawn| *withdrawn != provider);
        format!("{} can be used now.", models_of(&provider))
    }

    /// `client`, on the local server when there is one (tests).
    fn local_url<T>(&self, client: T, with_base_url: impl FnOnce(T, String) -> T) -> T {
        match self.local.get() {
            Some(url) => with_base_url(client, url.clone()),
            None => client,
        }
    }

    fn notice(&self, text: &str) {
        if let Some(feed) = &self.feed {
            feed.notice(Level::Info, text);
        }
    }
}

/// How `client` reaches OpenAI models, when it does.
pub async fn openai_way(client: &LLMClient) -> Option<OpenAiAuth> {
    match client.has_provider(&Provider::OpenAI) {
        false => None,
        true if client.is_openai_chatgpt_oauth().await => Some(OpenAiAuth::ChatGpt),
        true => Some(OpenAiAuth::ApiKey),
    }
}

/// A provider's models, as a sentence names them.
fn models_of(provider: &Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "Claude models on the Anthropic API",
        Provider::OpenAI => "OpenAI models",
        Provider::Google => "Gemini models",
        _ => "OpenRouter's models",
    }
}

/// "OpenAI API key" with a capital: as a sentence starts.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// "an OpenAI API key" without its article.
fn without_article(what: &str) -> &str {
    what.split_once(' ').map_or(what, |(_, rest)| rest)
}

/// Checks `value` as `key` with its provider, at its own address or at
/// `local` (tests): listing the models where that is free (Anthropic,
/// OpenAI, Gemini), OpenRouter's key endpoint, and for Voyage, which has no
/// free request, an embedding of one word. Only a clear refusal counts
/// against the key.
pub async fn check(key: &Key, value: &str, local: Option<&str>) -> Checked {
    let (address, host) = match key.name {
        credentials::ANTHROPIC_KEY => ("https://api.anthropic.com", "api.anthropic.com"),
        credentials::OPENAI_KEY => ("https://api.openai.com", "api.openai.com"),
        credentials::GEMINI_KEY => (
            "https://generativelanguage.googleapis.com",
            "generativelanguage.googleapis.com",
        ),
        credentials::OPENROUTER_KEY => ("https://openrouter.ai", "openrouter.ai"),
        _ => ("https://api.voyageai.com", "api.voyageai.com"),
    };
    let base = local.unwrap_or(address);
    let Ok(mut secret) = reqwest::header::HeaderValue::from_str(value) else {
        return Checked::Refused(format!(
            "{} can't be sent in a request. Paste it again.",
            capitalized(key.what)
        ));
    };
    secret.set_sensitive(true);
    let bearer =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {value}")).map(|mut header| {
            header.set_sensitive(true);
            header
        });
    let http = arko_ai::http_client();
    let request = match key.name {
        credentials::ANTHROPIC_KEY => http
            .get(format!("{base}/v1/models?limit=1"))
            .header("x-api-key", secret)
            .header("anthropic-version", "2023-06-01"),
        credentials::GEMINI_KEY => http
            .get(format!("{base}/v1beta/models?pageSize=1"))
            .header("x-goog-api-key", secret),
        name => {
            let Ok(bearer) = bearer else {
                return Checked::Unchecked(format!("fin couldn't check it with {}.", key.provider));
            };
            match name {
                credentials::OPENAI_KEY => http.get(format!("{base}/v1/models")),
                credentials::OPENROUTER_KEY => http.get(format!("{base}/api/v1/key")),
                _ => http
                    .post(format!("{base}/v1/embeddings"))
                    .header("content-type", "application/json")
                    .body(
                        serde_json::json!({"input": ["fin"], "model": VOYAGE_CHECK_MODEL})
                            .to_string(),
                    ),
            }
            .header("authorization", bearer)
        }
    };
    let response = match request.timeout(CHECK_TIMEOUT).send().await {
        Ok(response) => response,
        Err(_) => {
            return Checked::Unchecked(format!(
                "fin couldn't reach {} to check it.",
                if local.is_some() { base } else { host }
            ));
        }
    };
    let status = response.status().as_u16();
    // OpenAI answers 403 to a key that may not list models, or from a
    // region it doesn't serve: neither says the key is wrong.
    let refused = match status {
        401 => true,
        403 => key.name != credentials::OPENAI_KEY,
        // Gemini says a key isn't valid with a 400.
        400 if key.name == credentials::GEMINI_KEY => response
            .text()
            .await
            .is_ok_and(|body| body.contains("API_KEY_INVALID")),
        _ => false,
    };
    match status {
        200..=299 => Checked::Valid,
        _ if refused => Checked::Refused(format!(
            "{} refused {} ({status}), so it isn't saved. Paste another key.",
            key.provider,
            credentials::masked(value, key.prefix)
        )),
        _ => Checked::Unchecked(format!(
            "{} answered {status}, so fin couldn't check the key.",
            key.provider
        )),
    }
}

/// `/keys` as the line prompt and the web page show it: each key, masked
/// (`values`, at the terminal only), and where it comes from or what it is
/// for.
pub fn list(states: &[KeyState], values: bool) -> Vec<String> {
    let mut lines = vec!["API keys".to_string()];
    for state in states {
        let shown = match (&state.masked, values) {
            (None, _) => "not set".to_string(),
            (Some(masked), true) => masked.clone(),
            (Some(_), false) => "set".to_string(),
        };
        let about = match &state.source {
            None => state.key.purpose.to_string(),
            Some(KeySource::Fin(_)) => "saved by fin".to_string(),
            Some(source) => format!("from {}", credentials::source_text(source, state.variable)),
        };
        lines.push(format!("  {:<15}{shown:<16}{about}", state.key.provider));
    }
    lines
}

/// How `/keys` is used.
pub const USAGE: &str = "/keys lists the API keys. /keys add PROVIDER adds or replaces one, and /keys remove PROVIDER removes one fin saved. The providers are anthropic, openai, gemini, openrouter and voyage.";

/// The key `/keys add PROVIDER` (a whole line) adds, which the line prompt
/// reads with the input hidden.
pub fn added_at_prompt(line: &str) -> Option<&'static Key> {
    match line.split_whitespace().collect::<Vec<_>>().as_slice() {
        ["/keys", "add", provider] => credentials::key_named(provider),
        _ => None,
    }
}

/// The line prompt's `/keys add`: reads `key` with the input hidden, checks
/// it, saves it and uses it, saying what happens. (On the input thread, so
/// that the terminal is the prompt's: `runtime` runs the check.)
pub fn add_at_prompt(keys: &Keys, key: &'static Key, runtime: &tokio::runtime::Handle) {
    let out = crate::ui::Output::new(None, false);
    let value = match credentials::read_key(&out, key) {
        Ok(Some(value)) => value,
        Ok(None) => return out.line(&out.dim("No key was added.")),
        Err(error) => return out.line(&out.error(&format!("fin: {error}"))),
    };
    out.line(&out.dim(&format!("Checking it with {}…", key.provider)));
    match runtime.block_on(keys.add(key, Secret::new(value))) {
        Outcome::Done { said, warnings } => {
            out.line(&said);
            for warning in warnings {
                out.line(&out.warn(&warning));
            }
        }
        Outcome::Failed(why) => out.line(&out.error(&why)),
    }
}

impl crate::Harness {
    /// `/keys`, typed at the line prompt or sent from the page: lists the
    /// API keys (masked at the terminal, and only whether each is set on the
    /// page), and removes one fin saved. Adding one takes hidden input,
    /// which the full-screen view and the line prompt read themselves.
    pub(crate) fn keys_command(&mut self, argument: &str, source: Source) {
        let words: Vec<&str> = argument.split_whitespace().collect();
        let key = |word: &str| {
            credentials::key_named(word)
                .ok_or_else(|| format!("fin takes no API key for `{word}`. {USAGE}"))
        };
        let result = match words.as_slice() {
            [] => {
                let states = credentials::key_states();
                if source == Source::Terminal {
                    // The values, masked, at the line prompt only.
                    for line in list(&states, true) {
                        self.output.echo(&line);
                    }
                    self.output.echo(&self.output.dim(USAGE));
                    self.feed.output(&list(&states, false).join("\n"));
                } else {
                    self.mirrored(|harness| {
                        for line in list(&states, false) {
                            harness.output.line(&line);
                        }
                        harness
                            .output
                            .line("Add or remove keys at the terminal. This page isn't encrypted.");
                    });
                }
                return;
            }
            ["add" | "remove", ..] if source == Source::Web => Err(
                "API keys are added and removed at the terminal, not from the web page."
                    .to_string(),
            ),
            ["add", word] => key(word).and_then(|_| {
                Err("/keys add reads the key with the input hidden, which needs a terminal. Set it in the environment or a .env file instead.".to_string())
            }),
            ["remove", word] => key(word).map(|key| self.keys.remove(key)),
            _ => Err(USAGE.to_string()),
        };
        self.mirrored(|harness| match result {
            Ok(Outcome::Done { said, warnings }) => {
                harness.output.line(&said);
                for warning in warnings {
                    harness.output.line(&harness.output.warn(&warning));
                }
            }
            Ok(Outcome::Failed(why)) | Err(why) => harness.output.line(&harness.output.error(&why)),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local server that answers each request with the next of `replies`
    /// (a status and a body), and keeps each request's head.
    async fn serve(
        replies: Vec<(u16, &'static str)>,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let heads = Arc::new(Mutex::new(Vec::new()));
        let seen = heads.clone();
        let task = tokio::spawn(async move {
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 8192];
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&bytes[..end]).to_string();
                    let length: usize = head
                        .to_ascii_lowercase()
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .map_or(0, |value| value.trim().parse().unwrap());
                    if bytes.len() >= end + 4 + length {
                        seen.lock().unwrap().push(head);
                        break;
                    }
                }
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (url, heads, task)
    }

    /// Each provider is asked in its own way, with the key in a header (never
    /// the address), and only a clear refusal counts against the key.
    #[tokio::test]
    async fn a_key_is_checked_with_its_provider() {
        use credentials::{ANTHROPIC, GEMINI, OPENAI, OPENROUTER, VOYAGE};
        let (url, heads, _task) = serve(vec![
            (200, "{}"),
            (401, r#"{"error":{"message":"invalid x-api-key"}}"#),
            (200, "{}"),
            (403, "{}"),
            (400, r#"{"error":{"status":"INVALID_ARGUMENT","details":[{"reason":"API_KEY_INVALID"}]}}"#),
            (400, r#"{"error":{"status":"INVALID_ARGUMENT"}}"#),
            (401, "{}"),
            (200, "{}"),
            (503, "{}"),
        ])
        .await;
        let url = Some(url.as_str());
        let key = "sk-ant-api03-0123456789abcdef";
        assert_eq!(check(&ANTHROPIC, key, url).await, Checked::Valid);
        let Checked::Refused(why) = check(&ANTHROPIC, key, url).await else {
            panic!("not refused")
        };
        assert_eq!(
            why,
            "Anthropic refused sk-ant-…cdef (401), so it isn't saved. Paste another key."
        );
        assert_eq!(
            check(&OPENAI, "sk-proj-0123456789abcdef", url).await,
            Checked::Valid
        );
        assert!(matches!(
            check(&OPENAI, "sk-proj-0123456789abcdef", url).await,
            Checked::Unchecked(_)
        ));
        assert!(matches!(
            check(&GEMINI, "AIza0123456789abcdef", url).await,
            Checked::Refused(_)
        ));
        assert!(matches!(
            check(&GEMINI, "AIza0123456789abcdef", url).await,
            Checked::Unchecked(_)
        ));
        assert!(matches!(
            check(&OPENROUTER, "sk-or-v1-0123456789abcdef", url).await,
            Checked::Refused(_)
        ));
        assert_eq!(
            check(&VOYAGE, "pa-0123456789abcdef", url).await,
            Checked::Valid
        );
        assert_eq!(
            check(&VOYAGE, "pa-0123456789abcdef", url).await,
            Checked::Unchecked("Voyage answered 503, so fin couldn't check the key.".into())
        );
        let heads = heads.lock().unwrap().clone();
        let first = |head: &str| head.lines().next().unwrap_or_default().to_string();
        assert!(first(&heads[0]).starts_with("GET /v1/models?limit=1 "));
        assert!(heads[0].contains(&format!("x-api-key: {key}")));
        assert!(heads[2].contains("authorization: Bearer sk-proj-0123456789abcdef"));
        assert!(first(&heads[4]).starts_with("GET /v1beta/models?pageSize=1 "));
        assert!(heads[4].contains("x-goog-api-key: AIza0123456789abcdef"));
        assert!(first(&heads[6]).starts_with("GET /api/v1/key "));
        assert!(first(&heads[7]).starts_with("POST /v1/embeddings "));
        for head in &heads {
            assert!(
                !first(head).contains("0123456789"),
                "a key is never in the address: {head}"
            );
        }
    }

    /// Without the provider, a key is saved unchecked.
    #[tokio::test]
    async fn a_provider_out_of_reach_leaves_the_key_unchecked() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let Checked::Unchecked(why) =
            check(&credentials::OPENAI, "sk-proj-0123456789abcdef", Some(&url)).await
        else {
            panic!("checked")
        };
        assert_eq!(why, format!("fin couldn't reach {url} to check it."));
    }

    #[test]
    fn a_secret_is_never_shown() {
        let secret = Secret::new("sk-ant-api03-secret");
        assert_eq!(format!("{secret:?}"), "Secret(…)");
        assert_eq!(secret.len(), 19);
    }

    #[test]
    fn the_list_shows_values_at_the_terminal_only() {
        let states = vec![
            KeyState {
                key: &credentials::ANTHROPIC,
                masked: Some("sk-ant-…a1b2".into()),
                variable: credentials::ANTHROPIC_KEY,
                source: Some(KeySource::Fin("/home/me/.local/share/fin/.env".into())),
            },
            KeyState {
                key: &credentials::GEMINI,
                masked: Some("AIza…x9Yz".into()),
                variable: credentials::GOOGLE_KEY,
                source: Some(KeySource::Environment),
            },
            KeyState {
                key: &credentials::VOYAGE,
                masked: None,
                variable: credentials::VOYAGE_KEY,
                source: None,
            },
        ];
        let at_terminal = list(&states, true).join("\n");
        assert_eq!(
            at_terminal,
            "API keys
  Anthropic      sk-ant-…a1b2    saved by fin
  Google Gemini  AIza…x9Yz       from GOOGLE_API_KEY in fin's environment
  Voyage         not set         The search indexes embed with it, to search by meaning."
        );
        let on_the_page = list(&states, false).join("\n");
        assert!(!on_the_page.contains("a1b2") && !on_the_page.contains("x9Yz"));
        assert!(on_the_page.contains("Anthropic      set             saved by fin"));
    }
}

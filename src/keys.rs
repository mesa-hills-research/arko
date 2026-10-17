//! The API keys while Arko runs: `/keys` lists the providers' keys, and adds,
//! replaces and removes them. A key is saved in Arko's `.env` as the setup
//! screen saves, and used at once: the session's model client gets the
//! provider for the agents' next requests, and the search indexes embed with
//! a Voyage key. Nothing is sent to the provider until an agent or a search
//! uses the key. It travels from the screen to here as a [`Secret`], so it
//! never reaches the input history, the transcript, the feed or the logs.
//!
//! A replaced key is used from the next request on, by the agents already
//! running too (the old one may be why it was replaced). A removed key is
//! kept by the agents already using it until Arko quits, and nothing new
//! starts on it: the session stops offering its provider, new workers can't
//! start on it, and searches go back to keywords.

use std::sync::{Arc, Mutex};

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

/// What adding or removing a key did, for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Saved or removed: what was done, and what to warn about.
    Done { said: String, warnings: Vec<String> },
    /// Another provider's key, or not saved: nothing changed.
    Failed(String),
}

/// The session's API keys: what the model client and the search indexes use,
/// and which providers the session reaches.
pub struct Keys {
    /// These keys, as the changes they announce say ([`share`](Self::share)).
    id: u64,
    /// Arko's folder, whose other sessions hear of the keys added and
    /// removed here, and tell theirs.
    shared: std::sync::OnceLock<std::path::PathBuf>,
    client: Arc<LLMClient>,
    indexes: Option<Arc<index::Indexes>>,
    feed: Option<remote::Feed>,
    /// The session's context: its settings and keys are read and saved as
    /// the session's, whatever thread calls (an app's).
    context: Option<Arc<crate::context::SessionContext>>,
    /// Where the providers are reached: their own addresses, or (tests) a
    /// local server for all of them.
    local: std::sync::OnceLock<String>,
    state: Mutex<State>,
}

struct State {
    /// How the client reaches OpenAI models; `None` when it doesn't.
    openai: Option<OpenAiAuth>,
    /// The providers whose key was removed with no other to take its place.
    withdrawn: Vec<Provider>,
}

/// Adds `key` (or replaces it) with `value` without a session (an app's
/// accounts): saved in the `.env` of Arko's folder `root` as the keys menu
/// saves it, and used at once by the sessions open on `root` in this
/// process, and by those that start.
pub fn add(root: &std::path::Path, key: &'static Key, value: Secret) -> Outcome {
    let env = credentials::Env::for_folder_in(None, true, Some(root));
    crate::context::with_env(env, || add_in(root, key, value))
}

fn add_in(root: &std::path::Path, key: &'static Key, value: Secret) -> Outcome {
    let value = value.reveal().trim();
    if let Err(problem) = credentials::check_pasted(value, key) {
        return Outcome::Failed(problem);
    }
    let saved = match credentials::save_key(key, value) {
        Ok(saved) => saved,
        Err(error) => return Outcome::Failed(capitalized(&error)),
    };
    log::info!("keys: {} saved", key.name);
    share(root, key, Some(value));
    let mut warnings: Vec<String> = credentials::prefix_note(value, key).into_iter().collect();
    warnings.extend(saved.note);
    Outcome::Done {
        said: format!("Arko saved the {}.", without_article(key.what)),
        warnings,
    }
}

/// Removes `key` without a session, when Arko saved the one in use: from
/// the `.env` of Arko's folder `root`, and from the sessions open on `root`
/// (each goes by the next it reads, if any).
pub fn remove(root: &std::path::Path, key: &'static Key) -> Outcome {
    let env = credentials::Env::for_folder_in(None, true, Some(root));
    crate::context::with_env(env, || remove_in(root, key))
}

fn remove_in(root: &std::path::Path, key: &'static Key) -> Outcome {
    let after = match credentials::remove_key(key) {
        Ok(after) => after,
        Err(error) => return Outcome::Failed(capitalized(&error)),
    };
    log::info!("keys: {} removed", key.name);
    share(root, key, None);
    let what = without_article(key.what);
    Outcome::Done {
        said: match &after.source {
            Some(source) => format!(
                "Arko removed the {what} from its .env, and now uses {}.",
                credentials::source_text(source, after.variable)
            ),
            None => format!("Arko removed the {what} from its .env."),
        },
        warnings: Vec::new(),
    }
}

/// Tells the sessions open on Arko's folder `root` of `key` added (`value`)
/// or removed outside them.
fn share(root: &std::path::Path, key: &'static Key, value: Option<&str>) {
    credentials::announce(credentials::KeyChange {
        root: root.to_path_buf(),
        key,
        value: value.map(str::to_string),
        // (No session's keys.)
        origin: 0,
    });
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
        static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Arc::new(Self {
            id: IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            shared: std::sync::OnceLock::new(),
            client,
            indexes,
            feed,
            context: crate::context::handle(),
            local: std::sync::OnceLock::new(),
            state: Mutex::new(State {
                openai,
                withdrawn: Vec::new(),
            }),
        })
    }

    /// Shares the keys added and removed with the other sessions open on
    /// Arko's folder `root` in this process, both ways: a key added in one
    /// is used by all of them at once, as if added in each.
    pub fn share(self: &Arc<Self>, root: std::path::PathBuf) {
        if self.shared.set(root.clone()).is_err() {
            return;
        }
        let mut changes = credentials::changes();
        let keys = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let change = match changes.recv().await {
                    Ok(change) => change,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let Some(keys) = keys.upgrade() else {
                    break;
                };
                if change.origin == keys.id || change.root != root {
                    continue;
                }
                // Removed: what this session reads next, if anything.
                let value = change
                    .value
                    .clone()
                    .or_else(|| credentials::key_value(change.key));
                let now = keys.apply(change.key, value.as_deref());
                let done = if change.value.is_some() {
                    "added"
                } else {
                    "removed"
                };
                keys.notice(&format!(
                    "{} {done} in another session. {now}",
                    capitalized(without_article(change.key.what))
                ));
            }
        });
    }

    /// Tells the other sessions on Arko's folder of a key added or removed
    /// here (see [`share`](Self::share)).
    fn announce(&self, key: &'static Key, value: Option<&str>) {
        if let Some(root) = self.shared.get() {
            credentials::announce(credentials::KeyChange {
                root: root.clone(),
                key,
                value: value.map(str::to_string),
                origin: self.id,
            });
        }
    }

    /// Builds the providers' clients for the server at `url` (tests).
    #[cfg(test)]
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
    /// removed while Arko runs. (Said to the main agent.)
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
            "New workers can't start on {}: the user removed its API key ({name}) while Arko runs. Ask the user to add one with /keys, or to choose another worker model with /model worker.",
            providers::model_name(model),
        ))
    }

    /// Adds `key` (or replaces it) with `value`: saved in Arko's `.env`, and
    /// used at once.
    pub fn add(&self, key: &'static Key, value: Secret) -> Outcome {
        crate::context::within(self.context.clone(), || self.add_here(key, value))
    }

    fn add_here(&self, key: &'static Key, value: Secret) -> Outcome {
        let value = value.reveal().trim();
        if let Err(problem) = credentials::check_pasted(value, key) {
            return Outcome::Failed(problem);
        }
        let saved = match credentials::save_key(key, value) {
            Ok(saved) => saved,
            Err(error) => return Outcome::Failed(capitalized(&error)),
        };
        log::info!("keys: {} saved and used", key.name);
        let now = self.apply(key, Some(value));
        self.announce(key, Some(value));
        let said = format!("Arko saved the {}. {now}", without_article(key.what));
        let mut warnings: Vec<String> = credentials::prefix_note(value, key).into_iter().collect();
        warnings.extend(saved.note);
        self.notice(&format!(
            "{} added. {now}",
            capitalized(without_article(key.what))
        ));
        Outcome::Done { said, warnings }
    }

    /// Removes `key`, when Arko saved the one in use: from Arko's `.env`, and
    /// for what starts from now on. A key another source sets takes its
    /// place.
    pub fn remove(&self, key: &'static Key) -> Outcome {
        crate::context::within(self.context.clone(), || self.remove_here(key))
    }

    fn remove_here(&self, key: &'static Key) -> Outcome {
        let after = match credentials::remove_key(key) {
            Ok(after) => after,
            Err(error) => return Outcome::Failed(capitalized(&error)),
        };
        log::info!("keys: {} removed", key.name);
        let value = credentials::key_value(key);
        let now = self.apply(key, value.as_deref());
        self.announce(key, None);
        let what = without_article(key.what);
        let said = match (&after.source, value.is_some()) {
            (Some(source), true) => format!(
                "Arko removed the {what} from its .env, and now uses {}.",
                credentials::source_text(source, after.variable)
            ),
            _ => format!("Arko removed the {what} from its .env. {now}"),
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
            return "Agents already using it keep it until Arko quits.".to_string();
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
            Some(KeySource::Arko(_)) => "saved by Arko".to_string(),
            Some(source) => format!("from {}", credentials::source_text(source, state.variable)),
        };
        lines.push(format!("  {:<15}{shown:<16}{about}", state.key.provider));
    }
    lines
}

/// How `/keys` is used.
pub const USAGE: &str = "/keys lists the API keys. /keys add PROVIDER adds or replaces one, and /keys remove PROVIDER removes one Arko saved. The providers are anthropic, openai, gemini, openrouter and voyage.";

/// The key `/keys add PROVIDER` (a whole line) adds, which the line prompt
/// reads with the input hidden.
#[cfg(feature = "tui")]
pub fn added_at_prompt(line: &str) -> Option<&'static Key> {
    match line.split_whitespace().collect::<Vec<_>>().as_slice() {
        ["/keys", "add", provider] => credentials::key_named(provider),
        _ => None,
    }
}

/// The line prompt's `/keys add`: reads `key` with the input hidden, saves
/// it and uses it, saying what happens. (On the input thread, so that the
/// terminal is the prompt's.)
#[cfg(feature = "tui")]
pub fn add_at_prompt(keys: &Keys, key: &'static Key) {
    let out = crate::ui::Output::new(None, false);
    let value = match credentials::read_key(&out, key) {
        Ok(Some(value)) => value,
        Ok(None) => return out.line(&out.dim("No key was added.")),
        Err(error) => return out.line(&out.error(&format!("arko: {error}"))),
    };
    // Its start and last 4 characters: the key pasted is the one meant.
    out.line(&out.dim(&format!(
        "{}: {}",
        capitalized(without_article(key.what)),
        credentials::masked(&value, key.prefix)
    )));
    match keys.add(key, Secret::new(value)) {
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
    /// page), and removes one Arko saved. Adding one takes hidden input,
    /// which the full-screen view and the line prompt read themselves.
    pub(crate) fn keys_command(&mut self, argument: &str, source: Source) {
        let words: Vec<&str> = argument.split_whitespace().collect();
        let key = |word: &str| {
            credentials::key_named(word)
                .ok_or_else(|| format!("Arko takes no API key for `{word}`. {USAGE}"))
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

    /// A key added without a session (an app's accounts) is used at once by
    /// the sessions open on that Arko folder.
    #[tokio::test]
    async fn a_key_added_without_a_session_reaches_open_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let feed = remote::Feed::new("main");
        let keys = Keys::new(Arc::new(LLMClient::new()), None, None, Some(feed));
        keys.share(root.clone());
        // As `add` does once the key is saved.
        share(&root, &credentials::OPENROUTER, Some("sk-or-test"));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !keys.reaches(&Provider::OpenRouter) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    /// A key added in one session is used at once by the others open on the
    /// same Arko folder (not by those on another), and they say so; the one
    /// that added it has it already.
    #[tokio::test]
    async fn a_key_added_reaches_every_open_session() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        let session = |feed: &remote::Feed| {
            Keys::new(Arc::new(LLMClient::new()), None, None, Some(feed.clone()))
        };
        let (feed_a, feed_b, feed_c) = (
            remote::Feed::new("main"),
            remote::Feed::new("main"),
            remote::Feed::new("main"),
        );
        let (a, b, elsewhere) = (session(&feed_a), session(&feed_b), session(&feed_c));
        a.share(root.clone());
        b.share(root.clone());
        elsewhere.share(temp.path().join("other"));
        // As `add` does once the key is saved and used.
        a.announce(&credentials::OPENROUTER, Some("sk-or-test"));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !b.reaches(&Provider::OpenRouter) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let notices = |feed: &remote::Feed| {
            feed.subscribe()
                .0
                .items
                .into_iter()
                .map(|item| item.text)
                .collect::<Vec<_>>()
        };
        assert!(
            notices(&feed_b)
                .iter()
                .any(|text| text.starts_with("OpenRouter API key added in another session.")),
            "{:?}",
            notices(&feed_b)
        );
        assert!(!a.reaches(&Provider::OpenRouter), "its own change");
        assert!(
            !elsewhere.reaches(&Provider::OpenRouter),
            "another Arko folder"
        );
        assert!(notices(&feed_c).is_empty());
    }

    /// An app's accounts save keys and the ChatGPT login in the `.env` of
    /// the Arko folder they are given, and touch no other.
    #[test]
    fn accounts_save_in_the_arko_folder_they_are_given() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("arko");
        std::fs::create_dir_all(&root).unwrap();
        // (The user's own Arko folder is only looked at, never read.)
        let own = credentials::Env::for_folder(None, false).arko_dotenv().ok();
        let stamp = || {
            own.as_ref()
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|meta| (meta.len(), meta.modified().ok()))
        };
        let before = stamp();
        let saved = || std::fs::read_to_string(root.join(".env")).unwrap_or_default();
        let key: &'static Key = credentials::KEYS
            .into_iter()
            .find(|key| key.name == credentials::OPENROUTER_KEY)
            .unwrap();
        let outcome = add(
            &root,
            key,
            Secret::new("sk-or-v1-0123456789abcdef0123456789abcdef"),
        );
        assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
        assert!(saved().contains("OPENROUTER_API_KEY="));
        if std::env::var_os(credentials::OPENROUTER_KEY).is_none() {
            let outcome = remove(&root, key);
            assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
            assert!(!saved().contains("OPENROUTER_API_KEY="));
        }
        let auth = temp.path().join("auth.json");
        std::fs::write(
            &auth,
            r#"{"tokens": {"access_token": "access-1", "account_id": "account-1"}}"#,
        )
        .unwrap();
        let login = credentials::set_chatgpt_login(&root, &auth).unwrap();
        assert_eq!(login.path.as_deref(), Some(auth.as_path()));
        assert!(saved().contains("ARKO_CODEX_AUTH="));
        assert_eq!(stamp(), before, "the user's own .env is untouched");
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
                source: Some(KeySource::Arko("/home/me/.local/share/arko/.env".into())),
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
  Anthropic      sk-ant-…a1b2    saved by Arko
  Google Gemini  AIza…x9Yz       from GOOGLE_API_KEY in Arko's environment
  Voyage         not set         The search indexes embed with it, to search by meaning."
        );
        let on_the_page = list(&states, false).join("\n");
        assert!(!on_the_page.contains("a1b2") && !on_the_page.contains("x9Yz"));
        assert!(on_the_page.contains("Anthropic      set             saved by Arko"));
    }
}

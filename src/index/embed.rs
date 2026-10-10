//! Embedding models: Voyage through arko_ai (kind `code` → `voyage-code-4`, `docs`
//! → `voyage-4`), and arko_index's offline `HashEmbedder` for tests.
//!
//! arko_index's `Embedder` trait is synchronous and is called from blocking
//! threads (index builds and embedding runs on `std::thread`s, searches in
//! `spawn_blocking`). [`VoyageEmbedder`] therefore holds a handle to fin's tokio
//! runtime and blocks on it, never a runtime of its own (creating or dropping a
//! runtime on an async thread panics). It must not be called on an async thread.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arko_ai::{
    BackoffPolicy, EmbeddingClient, EmbeddingConfig, EmbeddingModelDetails,
    EmbeddingProviderIdentifier, RetryClass, TokenCounter, VoyageInputType,
};
use arko_index::bench_v1::Error;
use arko_index::dense::{
    EmbedLimits, Embedder, Embedding, HashEmbedder, InputKind, ModelDescriptor, StopSignal,
};
use tokio::runtime::Handle;

use super::store::Kind;

/// The model of code indexes.
pub const CODE_MODEL: &str = "voyage-code-4";
/// The model of docs indexes.
pub const DOCS_MODEL: &str = "voyage-4";
/// arko_index's offline test embedder.
pub const HASH_MODEL: &str = "hash-v1";
/// The dimensions of [`HASH_MODEL`] vectors.
const HASH_DIMENSIONS: usize = 64;

/// What the environment variable fin reads the key from is called, for messages.
pub const KEY_NAME: &str = "VOYAGE_API_KEY";

/// The Voyage model for an index of `kind`.
pub fn voyage_model(kind: Kind) -> &'static str {
    match kind {
        Kind::Code => CODE_MODEL,
        Kind::Docs => DOCS_MODEL,
    }
}

/// Where embeddings come from.
#[derive(Clone)]
pub enum Backend {
    /// Voyage, with fin's key (none: lexical indexes and search only).
    Voyage { key: Option<String> },
    /// arko_index's `HashEmbedder`: offline and deterministic, for tests.
    #[cfg(test)]
    Hash,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Voyage { key } => f
                .debug_struct("Voyage")
                .field("key", &key.as_ref().map(|_| "<set>"))
                .finish(),
            #[cfg(test)]
            Backend::Hash => f.write_str("Hash"),
        }
    }
}

impl Backend {
    /// A Voyage backend; a blank key counts as none.
    pub fn voyage(key: Option<String>) -> Self {
        Backend::Voyage {
            key: key
                .map(|key| key.trim().to_string())
                .filter(|key| !key.is_empty()),
        }
    }

    /// The model new indexes of `kind` embed with.
    pub fn model_for(&self, kind: Kind) -> String {
        match self {
            Backend::Voyage { .. } => voyage_model(kind).to_string(),
            #[cfg(test)]
            Backend::Hash => HASH_MODEL.to_string(),
        }
    }

    /// Whether embedding requests (indexing and queries) can be made.
    pub fn has_key(&self) -> bool {
        match self {
            Backend::Voyage { key } => key.is_some(),
            #[cfg(test)]
            Backend::Hash => true,
        }
    }

    /// Whether vectors of `descriptor` can be made now: its provider is available.
    pub fn can_embed(&self, descriptor: &ModelDescriptor) -> bool {
        match descriptor.provider.as_str() {
            "hash" => true,
            "voyage" => matches!(self, Backend::Voyage { key: Some(_) }),
            _ => false,
        }
    }

    /// The descriptor of the vectors `model` makes, as the upstream Voyage adapter
    /// names them (provider `voyage`, the model's default dimensions, input types on),
    /// so indexes are interchangeable with arko_index's own tools.
    pub fn descriptor(model: &str) -> Result<ModelDescriptor, String> {
        if model == HASH_MODEL {
            return Ok(HashEmbedder::new(HASH_DIMENSIONS).descriptor());
        }
        let details = voyage_details(model)?;
        Ok(
            ModelDescriptor::new("voyage", details.id.clone(), details.default_dimensions)
                .with_input_types(true),
        )
    }

    /// An embedder for the vectors of `descriptor` (an index's saved model, so a
    /// query embedder always matches it). `stop` interrupts retry waits.
    pub fn embedder(
        &self,
        descriptor: &ModelDescriptor,
        handle: &Handle,
        stop: Option<Arc<StopSignal>>,
    ) -> Result<Box<dyn Embedder>, String> {
        match descriptor.provider.as_str() {
            "hash" => Ok(Box::new(HashEmbedder::new(descriptor.dimensions))),
            "voyage" => {
                let key = match self {
                    Backend::Voyage { key } => key.clone(),
                    #[cfg(test)]
                    Backend::Hash => None,
                };
                Ok(Box::new(VoyageEmbedder::new(
                    key,
                    descriptor.clone(),
                    handle.clone(),
                    stop,
                )?))
            }
            other => Err(format!(
                "the index embeds with {}, a provider fin doesn't support ({other})",
                descriptor.display()
            )),
        }
    }
}

/// Dollars per million tokens of `model` (0 for the test model or an unknown one).
pub fn price_per_million(model: &str) -> f64 {
    voyage_details(model)
        .map(|details| details.cost_per_million_input_tokens)
        .unwrap_or(0.0)
}

/// The dollars `tokens` of `model` cost.
pub fn cost(model: &str, tokens: u64) -> f64 {
    tokens as f64 * price_per_million(model) / 1_000_000.0
}

fn voyage_details(model: &str) -> Result<EmbeddingModelDetails, String> {
    EmbeddingModelDetails::get(EmbeddingProviderIdentifier::Voyage, model)
        .map_err(|error| format!("unknown embedding model {model:?}: {error}"))
}

/// Voyage's total tokens per request where arko_ai's model table has none:
/// voyage-code-4 answered "The max allowed tokens per submitted batch is 320000"
/// (arko_index's Voyage notes). Other models without a figure get the smallest
/// documented limit.
fn request_token_limit(details: &EmbeddingModelDetails) -> u64 {
    match (details.max_batch_total_tokens, details.id.as_str()) {
        (Some(n), _) => n as u64,
        (None, "voyage-code-4") => 320_000,
        (None, _) => 120_000,
    }
}

/// arko_index's `Embedder` on arko_ai's Voyage client (adapted from arko_index's
/// `examples/support/voyage.rs`): documents use `input_type: document` and queries
/// `input_type: query`, the output dimension is always sent, and nothing is
/// truncated. Transient and capacity errors are retried (4 times, with backoff).
/// Without a key it still names its vectors and counts tokens (with the model's
/// tokenizer, cached by Hugging Face's hub after the first download), so builds
/// and free estimates work; embedding fails.
pub struct VoyageEmbedder {
    client: Option<EmbeddingClient>,
    details: EmbeddingModelDetails,
    descriptor: ModelDescriptor,
    handle: Handle,
    counter: Mutex<Option<TokenCounter>>,
    stop: Option<Arc<StopSignal>>,
    max_retries: u32,
    backoff: BackoffPolicy,
}

impl VoyageEmbedder {
    pub fn new(
        key: Option<String>,
        descriptor: ModelDescriptor,
        handle: Handle,
        stop: Option<Arc<StopSignal>>,
    ) -> Result<Self, String> {
        let details = voyage_details(&descriptor.model)?;
        if !details
            .supported_dimensions
            .contains(&descriptor.dimensions)
        {
            return Err(format!(
                "{} supports {:?} dimensions, not {}",
                details.id, details.supported_dimensions, descriptor.dimensions
            ));
        }
        Ok(Self {
            client: key.map(|key| EmbeddingClient::new().with_voyage_api_key(&key)),
            details,
            descriptor,
            handle,
            counter: Mutex::new(None),
            stop,
            max_retries: 4,
            backoff: BackoffPolicy::new(Duration::from_secs(2), Duration::from_secs(60)),
        })
    }

    fn config(&self, kind: Option<InputKind>) -> EmbeddingConfig {
        let config = EmbeddingConfig::new(EmbeddingProviderIdentifier::Voyage, &self.details.id)
            .with_output_dimensionality(self.descriptor.dimensions);
        match kind {
            Some(kind) if self.descriptor.input_types => {
                config.with_voyage_input_type(match kind {
                    InputKind::Document => VoyageInputType::Document,
                    InputKind::Query => VoyageInputType::Query,
                })
            }
            _ => config,
        }
    }

    fn counter(&self) -> Result<TokenCounter, Error> {
        let mut slot = self.counter.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(counter) = slot.as_ref() {
            return Ok(counter.clone());
        }
        let config = self.config(None);
        let counter = self
            .handle
            .block_on(EmbeddingClient::new().token_counter(&config))
            .map_err(|error| format!("loading the {} tokenizer: {error}", self.details.id))?;
        *slot = Some(counter.clone());
        Ok(counter)
    }

    /// Sleeps `wait`, or less when the run is stopped; whether it was stopped.
    fn pause(&self, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        loop {
            if self.stop.as_ref().is_some_and(|stop| stop.is_stopped()) {
                return true;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
    }
}

impl Embedder for VoyageEmbedder {
    fn descriptor(&self) -> ModelDescriptor {
        self.descriptor.clone()
    }

    fn limits(&self) -> EmbedLimits {
        EmbedLimits::new(
            self.details.max_batch_size.unwrap_or(1000),
            request_token_limit(&self.details),
            self.details.max_input_tokens_per_item as u64,
        )
    }

    fn count_tokens(&self, texts: &[&str]) -> Result<Vec<u64>, Error> {
        let counter = self.counter()?;
        texts
            .iter()
            .map(|text| Ok(counter.count(text)? as u64))
            .collect()
    }

    fn embed(&self, texts: &[&str], kind: InputKind) -> Result<Embedding, Error> {
        let Some(client) = &self.client else {
            return Err(format!("embedding with Voyage needs {KEY_NAME}").into());
        };
        let config = self.config(Some(kind));
        let texts: Vec<String> = texts.iter().map(|text| (*text).to_owned()).collect();
        let mut attempt = 0;
        loop {
            match self.handle.block_on(client.embed(texts.clone(), &config)) {
                Ok(response) => {
                    return Ok(Embedding {
                        vectors: response.embeddings,
                        tokens: response.usage.input_tokens as u64,
                    });
                }
                Err(error)
                    if attempt < self.max_retries
                        && matches!(
                            error.retry_class(),
                            RetryClass::Transient | RetryClass::Capacity
                        ) =>
                {
                    attempt += 1;
                    let wait = self.backoff.next_delay(attempt, error.retry_after());
                    log::warn!(
                        "Voyage embedding request failed ({error}); retry {attempt} in {:.1} s",
                        wait.as_secs_f64()
                    );
                    if self.pause(wait) {
                        return Err(format!("stopped while retrying: {error}").into());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

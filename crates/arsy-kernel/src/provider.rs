//! Model provider boundary: canonical request in, normalized events out.
//!
//! See `docs/09-model-provider-layer.md` and ADR-0012. An adapter owns wire
//! format, authentication, and error normalization; nothing above this module
//! sees a provider dialect. Operations never touch a provider, so a second
//! adapter is a new [`ModelProvider`] impl and nothing else.
//!
//! Model profiles and capability probing live in [`crate::model_profile`] so
//! observing model behavior stays separate from provider wire execution.

pub mod anthropic;
pub mod wire;

use crate::protocol::IdempotencyKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, time::Duration};

/// Provider-qualified model name. The provider half selects the adapter; the
/// model half is passed through to the wire untouched.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ModelKey {
    pub provider: String,
    pub model: String,
}

impl fmt::Display for ModelKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    User,
    Assistant,
}

/// One piece of conversation content in canonical form.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelContent {
    Text {
        text: String,
    },
    /// A tool call the model already made, replayed back as history.
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    /// The result the host produced for a previous tool call.
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: Vec<ModelContent>,
}

/// A tool offered to the model. The schema is passed through as-is; adapting it
/// to a provider dialect is the adapter's job.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Provider-independent request. Every field here has the same meaning for
/// every adapter; anything that does not is not allowed in this struct.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CanonicalModelRequest {
    pub model: ModelKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<ModelMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSchema>,
    pub max_output_tokens: u32,
    /// Carried so a retry is provably the same request, not a second one.
    pub idempotency_key: IdempotencyKey,
}

/// Why the model stopped producing output.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Other,
}

/// Normalized stream event. Identical shapes from every adapter.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ModelEvent {
    TextDelta {
        text: String,
    },
    /// A tool call has started. Arguments are not known yet and the call is not
    /// runnable at this point.
    ToolCallStarted {
        index: usize,
        id: String,
        name: String,
    },
    /// Raw, still-incomplete argument bytes exactly as received.
    ///
    /// Emitted for progress rendering only: the fragment is never parsed and
    /// never dispatched, so a truncated stream cannot execute a partial call.
    ToolCallDelta {
        index: usize,
        fragment: String,
    },
    /// A complete tool call whose arguments parsed. Only this variant is
    /// executable.
    ToolCallCompleted {
        index: usize,
        id: String,
        name: String,
        arguments: Value,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Completed {
        stop: StopReason,
    },
}

/// Normalized event stream. Lazy so a caller can stop reading early.
pub type ModelEventStream = Box<dyn Iterator<Item = Result<ModelEvent, ProviderError>> + Send>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderDescriptor {
    pub id: String,
    /// Largest number of extra attempts the adapter considers safe.
    pub max_retries: u32,
}

pub trait ModelProvider: Send + Sync {
    fn descriptor(&self) -> &ProviderDescriptor;

    /// Start a streaming completion. Errors are already normalized.
    fn stream(&self, request: &CanonicalModelRequest) -> Result<ModelEventStream, ProviderError>;
}

/// Normalized failure. Adapters map their status codes and error bodies onto
/// these variants so callers never branch on a provider dialect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderError {
    /// Credential missing, rejected, or lacking permission.
    Auth(String),
    /// The request is wrong and will fail again unchanged.
    InvalidRequest(String),
    NotFound(String),
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// Provider-side capacity or fault.
    Server {
        status: u16,
        message: String,
    },
    /// The request never reached a response.
    Transport(String),
    /// A response arrived but did not match the provider's own wire contract.
    Decode(String),
}

impl ProviderError {
    /// Stable machine-readable code for logs and protocol failures.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Auth(_) => "provider_auth",
            Self::InvalidRequest(_) => "provider_invalid_request",
            Self::NotFound(_) => "provider_not_found",
            Self::RateLimited { .. } => "provider_rate_limited",
            Self::Server { .. } => "provider_server",
            Self::Transport(_) => "provider_transport",
            Self::Decode(_) => "provider_decode",
        }
    }

    /// How long to wait before retrying, or `None` when a retry cannot help.
    ///
    /// A hint the provider sent wins over the default backoff; a class that is
    /// deterministic in the request (auth, invalid request, decode) never
    /// retries, because the identical body would fail identically.
    pub fn retry_after(&self, attempt: u32) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => Some(retry_after.unwrap_or(backoff(attempt))),
            Self::Server { status, .. } if *status >= 500 => Some(backoff(attempt)),
            Self::Transport(_) => Some(backoff(attempt)),
            _ => None,
        }
    }
}

/// Exponential backoff, capped. Deterministic: jitter belongs to the caller's
/// scheduler, not to a value used in assertions.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.saturating_pow(attempt.min(6)))
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth(message) => write!(formatter, "provider authentication failed: {message}"),
            Self::InvalidRequest(message) => {
                write!(formatter, "provider rejected request: {message}")
            }
            Self::NotFound(message) => write!(formatter, "provider resource not found: {message}"),
            Self::RateLimited { retry_after } => match retry_after {
                Some(delay) => write!(formatter, "provider rate limited, retry in {delay:?}"),
                None => formatter.write_str("provider rate limited"),
            },
            Self::Server { status, message } => {
                write!(formatter, "provider server error {status}: {message}")
            }
            Self::Transport(message) => write!(formatter, "provider transport failed: {message}"),
            Self::Decode(message) => write!(formatter, "provider response undecodable: {message}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// Start a stream, retrying only classes that a retry can fix.
///
/// The *same* request value is resubmitted every attempt, so its idempotency
/// key is unchanged and a provider that deduplicates sees one logical request
/// rather than several. `sleep` is injected so the delay is observable in a
/// test instead of really elapsing.
pub fn stream_with_retry(
    provider: &dyn ModelProvider,
    request: &CanonicalModelRequest,
    sleep: &mut dyn FnMut(Duration),
) -> Result<ModelEventStream, ProviderError> {
    let max_retries = provider.descriptor().max_retries;
    let mut attempt = 0;
    loop {
        match provider.stream(request) {
            Ok(stream) => return Ok(stream),
            Err(error) => match error.retry_after(attempt) {
                Some(delay) if attempt < max_retries => {
                    sleep(delay);
                    attempt += 1;
                }
                _ => return Err(error),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, sync::Mutex};

    struct FlakyProvider {
        descriptor: ProviderDescriptor,
        failures: Mutex<u32>,
        seen_keys: Mutex<Vec<IdempotencyKey>>,
    }

    impl ModelProvider for FlakyProvider {
        fn descriptor(&self) -> &ProviderDescriptor {
            &self.descriptor
        }

        fn stream(
            &self,
            request: &CanonicalModelRequest,
        ) -> Result<ModelEventStream, ProviderError> {
            self.seen_keys
                .lock()
                .unwrap()
                .push(request.idempotency_key.clone());
            let mut failures = self.failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(ProviderError::Server {
                    status: 503,
                    message: "overloaded".to_owned(),
                });
            }
            Ok(Box::new(std::iter::once(Ok(ModelEvent::Completed {
                stop: StopReason::EndTurn,
            }))))
        }
    }

    fn request() -> CanonicalModelRequest {
        CanonicalModelRequest {
            model: ModelKey {
                provider: "stub".to_owned(),
                model: "m".to_owned(),
            },
            system: None,
            messages: vec![ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: "hi".to_owned(),
                }],
            }],
            tools: Vec::new(),
            max_output_tokens: 64,
            idempotency_key: IdempotencyKey::new("turn-1").unwrap(),
        }
    }

    #[test]
    fn retries_resubmit_the_same_idempotency_key_and_honour_the_hint() {
        let provider = FlakyProvider {
            descriptor: ProviderDescriptor {
                id: "stub".to_owned(),
                max_retries: 3,
            },
            failures: Mutex::new(2),
            seen_keys: Mutex::new(Vec::new()),
        };
        let slept = RefCell::new(Vec::new());

        let stream = stream_with_retry(&provider, &request(), &mut |delay| {
            slept.borrow_mut().push(delay);
        })
        .unwrap();

        assert_eq!(stream.count(), 1);
        let keys = provider.seen_keys.lock().unwrap();
        assert_eq!(keys.len(), 3, "two retries after the first attempt");
        assert!(
            keys.windows(2).all(|pair| pair[0] == pair[1]),
            "every attempt carries the original idempotency key"
        );
        assert_eq!(
            slept.into_inner(),
            vec![Duration::from_millis(500), Duration::from_millis(1000)]
        );
    }

    #[test]
    fn deterministic_failures_are_not_retried() {
        let provider = FlakyProvider {
            descriptor: ProviderDescriptor {
                id: "stub".to_owned(),
                max_retries: 3,
            },
            failures: Mutex::new(0),
            seen_keys: Mutex::new(Vec::new()),
        };
        assert_eq!(
            ProviderError::Auth("bad key".to_owned()).retry_after(0),
            None
        );
        assert_eq!(
            ProviderError::InvalidRequest("too long".to_owned()).retry_after(0),
            None
        );
        assert_eq!(
            ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            }
            .retry_after(4),
            Some(Duration::from_secs(7)),
            "a provider hint overrides the default backoff"
        );
        // The trait is object-safe and usable without knowing the adapter.
        assert_eq!(provider.descriptor().id, "stub");
    }
}

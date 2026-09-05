//! HTTP shapes every provider adapter shares.
//!
//! The call itself is a trait rather than a dependency, so an adapter's wire
//! contract — body shape, headers, status mapping, SSE semantics — is
//! exercisable without a network. See `crate::provider::http` for the
//! implementation that really talks to a host.

use super::ProviderError;
use crate::secret::SecretValue;
use std::fmt;

/// One outbound HTTP request, fully formed by the adapter.
pub struct WireRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Response head plus a line-oriented body, which is what SSE needs.
pub struct WireResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub lines: Box<dyn Iterator<Item = Result<String, String>> + Send>,
}

impl WireResponse {
    /// `retry-after` in whole seconds, the only form providers send.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        header(&self.headers, "retry-after")
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
    }
}

/// Case-insensitive header lookup, because header casing is not guaranteed.
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The HTTP call itself. Injected so an adapter stays dependency-free.
pub trait WireTransport: Send + Sync {
    fn send(&self, request: WireRequest) -> Result<WireResponse, ProviderError>;
}

/// API credential. Never printed: `Debug` is redacted so a credential cannot
/// reach a log through a derived formatter.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Credential resolved from a [`SecretHandle`](crate::secret::SecretHandle).
    /// The broker has already registered the value for redaction, so the key
    /// exists in cleartext only between here and the wire.
    pub fn from_secret(secret: &SecretValue) -> Self {
        Self(secret.expose().to_owned())
    }

    /// Read the credential. Every call site is an egress point and belongs as
    /// close to the wire as possible.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(redacted)")
    }
}

//! Secret broker: credentials travel as handles, raw values as short-lived
//! [`SecretValue`]s, and every external sink runs the redaction pipeline first.
//!
//! See `docs/26-observability.md` and `docs/29-threat-model.md`. A
//! [`SecretHandle`] is inert: it is the only form allowed in prompts, events,
//! logs, and telemetry, so serializing one can never leak a credential. A raw
//! value exists only after [`SecretBroker::resolve`], which also registers the
//! value with the broker's [`Redactor`] — resolving a credential is therefore
//! the same act as teaching every sink to hide it.
//!
//! Resolution has no plaintext fallback: a handle whose store is not registered
//! fails with [`SecretError::UnknownStore`]. Binding an OS keyring as a
//! [`CredentialStore`] belongs to the `arsy auth` CLI surface, which supplies
//! the store implementation this module deliberately does not hard-code.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

pub const OS_STORE_ID: &str = "os";
const OS_SERVICE: &str = "arsy";

/// Scheme every handle string carries.
pub const SECRET_SCHEME: &str = "secret://";

/// Shortest value the redactor accepts. A very short secret would match
/// unrelated text everywhere, so redaction of it is refused rather than turned
/// into a corrupted stream that still looks sanitized.
pub const MIN_SECRET_BYTES: usize = 8;

/// Reference to a credential. Safe to log, persist, and send to a model.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct SecretHandle {
    store: String,
    name: String,
}

impl SecretHandle {
    /// `secret://<store>/<name>`; both halves must be non-empty.
    pub fn new(store: impl Into<String>, name: impl Into<String>) -> Result<Self, SecretError> {
        let (store, name) = (store.into(), name.into());
        if store.is_empty() || name.is_empty() || store.contains('/') {
            return Err(SecretError::MalformedHandle(format!(
                "{SECRET_SCHEME}{store}/{name}"
            )));
        }
        Ok(Self { store, name })
    }

    pub fn store(&self) -> &str {
        &self.store
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Text that replaces the credential in a sanitized stream. Names the
    /// handle so an operator can still tell *which* credential was used.
    pub fn placeholder(&self) -> String {
        format!("[redacted:{self}]")
    }
}

impl fmt::Display for SecretHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{SECRET_SCHEME}{}/{}", self.store, self.name)
    }
}

impl TryFrom<String> for SecretHandle {
    type Error = SecretError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let rest = value
            .strip_prefix(SECRET_SCHEME)
            .ok_or_else(|| SecretError::MalformedHandle(value.clone()))?;
        let (store, name) = rest
            .split_once('/')
            .ok_or_else(|| SecretError::MalformedHandle(value.clone()))?;
        Self::new(store, name)
    }
}

impl From<SecretHandle> for String {
    fn from(value: SecretHandle) -> Self {
        value.to_string()
    }
}

/// A resolved credential. Deliberately not `Serialize`, and its `Debug` shows
/// the handle only, so no derived formatter can print the value.
#[derive(Clone)]
pub struct SecretValue {
    handle: SecretHandle,
    value: String,
}

impl SecretValue {
    /// Read the raw credential. Every call site is an egress point and should
    /// be as close to the wire as possible.
    pub fn expose(&self) -> &str {
        &self.value
    }

    pub fn handle(&self) -> &SecretHandle {
        &self.handle
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "SecretValue({})", self.handle)
    }
}

/// Backing credential store, for example an OS keyring.
pub trait CredentialStore: Send + Sync {
    /// Store half of the handles this implementation answers for.
    fn id(&self) -> &str;

    /// Raw credential, or [`SecretError::NotFound`]. An implementation must not
    /// substitute a value from another source on a miss.
    fn resolve(&self, name: &str) -> Result<String, SecretError>;
}

/// Native macOS Keychain, Windows Credential Manager, or Linux Secret Service.
/// Unsupported platforms fail instead of falling back to memory or a file.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsCredentialStore;

impl OsCredentialStore {
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn entry(name: &str) -> Result<keyring::Entry, SecretError> {
        #[cfg(target_os = "linux")]
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            return Err(SecretError::Store {
                handle: SecretHandle::new(OS_STORE_ID, name).expect("fixed store ID is valid"),
                message: "D-Bus session bus is unavailable".to_owned(),
            });
        }
        keyring::Entry::new(OS_SERVICE, name).map_err(|error| SecretError::Store {
            handle: SecretHandle::new(OS_STORE_ID, name).expect("fixed store ID is valid"),
            message: error.to_string(),
        })
    }

    pub fn set(&self, name: &str, value: &str) -> Result<(), SecretError> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            Self::entry(name)?
                .set_password(value)
                .map_err(|error| SecretError::Store {
                    handle: SecretHandle::new(OS_STORE_ID, name).expect("fixed store ID is valid"),
                    message: error.to_string(),
                })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        Err(SecretError::UnknownStore(OS_STORE_ID.to_owned()))
    }

    pub fn remove(&self, name: &str) -> Result<(), SecretError> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            Self::entry(name)?.delete_credential().map_err(|error| {
                let handle = SecretHandle::new(OS_STORE_ID, name).expect("fixed store ID is valid");
                if matches!(error, keyring::Error::NoEntry) {
                    SecretError::NotFound(handle)
                } else {
                    SecretError::Store {
                        handle,
                        message: error.to_string(),
                    }
                }
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        Err(SecretError::UnknownStore(OS_STORE_ID.to_owned()))
    }
}

impl CredentialStore for OsCredentialStore {
    fn id(&self) -> &str {
        OS_STORE_ID
    }

    fn resolve(&self, name: &str) -> Result<String, SecretError> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            Self::entry(name)?.get_password().map_err(|error| {
                let handle = SecretHandle::new(OS_STORE_ID, name).expect("fixed store ID is valid");
                if matches!(error, keyring::Error::NoEntry) {
                    SecretError::NotFound(handle)
                } else {
                    SecretError::Store {
                        handle,
                        message: error.to_string(),
                    }
                }
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        Err(SecretError::UnknownStore(OS_STORE_ID.to_owned()))
    }
}

/// Owns the registered stores and the redactor they feed.
pub struct SecretBroker {
    stores: BTreeMap<String, Box<dyn CredentialStore>>,
    redactor: Redactor,
}

impl SecretBroker {
    pub fn new() -> Self {
        Self {
            stores: BTreeMap::new(),
            redactor: Redactor::new(),
        }
    }

    pub fn register_store(&mut self, store: Box<dyn CredentialStore>) {
        self.stores.insert(store.id().to_owned(), store);
    }

    /// Resolve a handle and register the value for redaction in one step.
    pub fn resolve(&mut self, handle: &SecretHandle) -> Result<SecretValue, SecretError> {
        let store = self
            .stores
            .get(handle.store())
            .ok_or_else(|| SecretError::UnknownStore(handle.store().to_owned()))?;
        let value = store.resolve(handle.name())?;
        self.redactor.register(handle, &value)?;
        Ok(SecretValue {
            handle: handle.clone(),
            value,
        })
    }

    /// Redactor to install on every sink that leaves the process.
    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }
}

impl Default for SecretBroker {
    fn default() -> Self {
        Self::new()
    }
}

/// Replaces known credentials with their handle placeholders.
///
/// Cheap to clone so each sink can hold its own snapshot.
#[derive(Clone, Default)]
pub struct Redactor {
    /// Longest value first, so a secret that contains a shorter one is redacted
    /// whole instead of being partially rewritten.
    entries: Vec<(SecretHandle, String)>,
}

impl Redactor {
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Teach the redactor one credential.
    ///
    /// Rejects a value the pipeline could not hide safely: too short to match
    /// only itself, or one that its own placeholder contains — which would make
    /// a second pass rewrite the placeholder and break idempotency.
    pub fn register(&mut self, handle: &SecretHandle, value: &str) -> Result<(), SecretError> {
        if value.len() < MIN_SECRET_BYTES {
            return Err(SecretError::Unredactable(handle.clone()));
        }
        if handle.placeholder().contains(value) {
            return Err(SecretError::Unredactable(handle.clone()));
        }
        self.entries.retain(|(known, _)| known != handle);
        self.entries.push((handle.clone(), value.to_owned()));
        self.entries
            .sort_by(|left, right| right.1.len().cmp(&left.1.len()).then(left.0.cmp(&right.0)));
        Ok(())
    }

    /// Sanitize text bound for a sink outside the process.
    ///
    /// Fails closed: the result is verified to contain no registered value, so
    /// a substitution that reassembled a secret across a replacement boundary
    /// aborts the write instead of emitting it.
    pub fn sanitize(&self, text: &str) -> Result<String, SecretError> {
        let mut sanitized = text.to_owned();
        for (handle, value) in &self.entries {
            if sanitized.contains(value.as_str()) {
                sanitized = sanitized.replace(value.as_str(), &handle.placeholder());
            }
        }
        for (handle, value) in &self.entries {
            if sanitized.contains(value.as_str()) {
                return Err(SecretError::RedactionFailed(handle.clone()));
            }
        }
        Ok(sanitized)
    }

    /// Handles known to the redactor, for diagnostics. Never the values.
    pub fn handles(&self) -> impl Iterator<Item = &SecretHandle> {
        self.entries.iter().map(|(handle, _)| handle)
    }
}

impl fmt::Debug for Redactor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Redactor")
            .field("handles", &self.handles().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretError {
    MalformedHandle(String),
    /// No store answers for this handle. There is no plaintext fallback.
    UnknownStore(String),
    NotFound(SecretHandle),
    /// The store itself failed, for example a locked keyring.
    Store {
        handle: SecretHandle,
        message: String,
    },
    /// The value cannot be redacted safely, so it is refused up front.
    Unredactable(SecretHandle),
    /// A sanitized payload still contained the credential.
    RedactionFailed(SecretHandle),
}

impl SecretError {
    /// Stable machine-readable code for logs and protocol failures.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MalformedHandle(_) => "secret_malformed_handle",
            Self::UnknownStore(_) => "secret_unknown_store",
            Self::NotFound(_) => "secret_not_found",
            Self::Store { .. } => "secret_store_failed",
            Self::Unredactable(_) => "secret_unredactable",
            Self::RedactionFailed(_) => "secret_redaction_failed",
        }
    }
}

impl fmt::Display for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedHandle(raw) => write!(formatter, "malformed secret handle: {raw}"),
            Self::UnknownStore(store) => {
                write!(formatter, "no credential store registered for {store:?}")
            }
            Self::NotFound(handle) => write!(formatter, "credential {handle} not found"),
            Self::Store { handle, message } => {
                write!(formatter, "credential store failed for {handle}: {message}")
            }
            Self::Unredactable(handle) => write!(
                formatter,
                "credential {handle} cannot be redacted safely and was refused"
            ),
            Self::RedactionFailed(handle) => {
                write!(
                    formatter,
                    "redaction failed for {handle}; output suppressed"
                )
            }
        }
    }
}

impl std::error::Error for SecretError {}

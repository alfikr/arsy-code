//! The credential catalog: which provider holds which handle, and where the
//! handles are kept.

use crate::{now, secret_failed, storage_failed, Diagnostic, Emitter};
use arsy_kernel::secret::{CredentialStore, FileCredentialStore, SecretError, SecretHandle};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

/// The catalog under the `file` store, beside the user configuration.
const CATALOG_FILE: &str = "credentials.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AuthRecord {
    provider: String,
    handle: SecretHandle,
    created_at: u64,
    last_used: Option<u64>,
    /// How the credential was obtained. Defaulted so a catalog written before
    /// OAuth existed still reads.
    #[serde(default)]
    kind: CredentialKind,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CredentialKind {
    #[default]
    ApiKey,
    // Spelled out, because the derived snake_case of `OAuth` is `o_auth`,
    // which is not what an operator reading the catalog expects to see. The
    // alias keeps a catalog written under the derived name readable, so the
    // rename cannot turn one into "corrupt".
    #[serde(rename = "oauth", alias = "o_auth")]
    OAuth,
}

/// Read the credential catalog.
///
/// The catalog is metadata — handles, provider names, timestamps — and never a
/// secret value. It lives in one place, beside the user configuration, owned by
/// the operator: there is no second store left to choose between.
fn read_catalog() -> Result<Option<String>, Diagnostic> {
    match FileCredentialStore.resolve(CATALOG_FILE) {
        Ok(raw) => Ok(Some(raw)),
        Err(SecretError::NotFound(_)) => Ok(None),
        Err(error) => Err(secret_failed(error)),
    }
}

fn write_catalog(raw: &str) -> Result<(), Diagnostic> {
    let path = FileCredentialStore::path(CATALOG_FILE)
        .ok_or_else(|| secret_failed("this platform has no user configuration directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(storage_failed)?;
    }
    // Created owner-only rather than created and then narrowed: a chmod after
    // the write leaves a window where the catalog is readable by the whole
    // machine.
    let mut file = owner_only(&path)?;
    file.write_all(format!("{raw}\n").as_bytes())
        .map_err(storage_failed)
}

/// A catalog file is not a secret, but it names every provider the operator
/// has a credential for, so it is not the whole machine's business either.
/// Truncate or create `path` readable by its owner alone.
///
/// The catalog is not a secret, but it names every provider the operator holds
/// a credential for, which is not the whole machine's business either.
fn owner_only(path: &Path) -> Result<std::fs::File, Diagnostic> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(storage_failed)
}

fn catalog() -> Result<Vec<AuthRecord>, Diagnostic> {
    let Some(raw) = read_catalog()? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw).map_err(|_| secret_failed("credential catalog is corrupt"))
}

fn save_catalog(records: &[AuthRecord]) -> Result<(), Diagnostic> {
    let raw = serde_json::to_string(records).map_err(|error| secret_failed(error.to_string()))?;
    write_catalog(&raw)
}

//! Turning configuration into a usable model provider.
//!
//! Configuration names an endpoint; the OS keyring and the environment hold
//! the credential for it. This module is the only place the two meet, and it
//! is deliberately the last step before the wire: a resolved credential is
//! registered for redaction as it is read, so every sink already knows to hide
//! it by the time a request is built.

use crate::{Diagnostic, ARSY_PRV_1000};
use arsy_kernel::{
    config::{Config, Dialect, Endpoint},
    provider::{
        anthropic::AnthropicProvider, http::HttpTransport, openai::OpenAiProvider, wire::ApiKey,
        ModelProvider,
    },
    secret::{CredentialStore, OsCredentialStore, Redactor, SecretError, SecretHandle},
};

/// Where a credential came from. Reported by `arsy doctor` so an operator can
/// tell a keyring entry from an inherited environment variable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialSource {
    ConfiguredEnv,
    Keyring,
    DefaultEnv,
}

impl CredentialSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfiguredEnv => "configured_env",
            Self::Keyring => "keyring",
            Self::DefaultEnv => "default_env",
        }
    }
}

/// The provider a turn will use, plus how it was assembled.
pub struct Resolved {
    pub provider: Box<dyn ModelProvider>,
    pub endpoint: Endpoint,
    pub source: CredentialSource,
}

/// Build the provider for `requested`, or for the configured default.
///
/// The credential is looked for in the order an operator would expect to
/// override it: an environment variable the config names, then the keyring
/// entry the config names, then the dialect's conventional variable. The first
/// one that holds a value wins, so exporting a key for one shell is enough to
/// override a stored one without editing anything.
pub fn resolve(config: &Config, requested: Option<&str>) -> Result<Resolved, Diagnostic> {
    let endpoint = config.endpoint(requested).cloned().ok_or_else(|| {
        Diagnostic::error(
            ARSY_PRV_1000,
            match requested.or_else(|| config.provider_default()) {
                Some(id) => format!("no provider endpoint named `{id}` is configured"),
                None => "no provider endpoint is configured".to_owned(),
            },
            "add a `[provider.endpoint.<name>]` table with `kind` and `base_url` to the user \
             config.toml, then run `arsy config explain provider`",
        )
    })?;

    let (secret, source) = credential(&endpoint, &from_env)?;
    let mut redactor = Redactor::new();
    // Registering here, rather than at the wire, means a key echoed back into
    // a prompt or an event is already masked — whichever source it came from,
    // not only the keyring. A value too short to redact safely is refused
    // rather than sent with a pipeline that would corrupt unrelated text.
    redactor
        .register(&redaction_handle(&endpoint, source)?, &secret)
        .map_err(|error| credential_failed(&endpoint.id, error))?;
    let key = ApiKey::new(secret);
    let transport = HttpTransport::default();
    let provider: Box<dyn ModelProvider> = match endpoint.kind {
        Dialect::Anthropic => Box::new(
            AnthropicProvider::with_base_url(&endpoint.base_url, key, transport)
                .with_redactor(redactor),
        ),
        Dialect::Openai => Box::new(
            OpenAiProvider::with_base_url(&endpoint.base_url, key, transport)
                .with_id(&endpoint.id)
                .with_redactor(redactor),
        ),
    };
    Ok(Resolved {
        provider,
        endpoint,
        source,
    })
}

/// `env` is injected because the workspace forbids `unsafe`, and mutating the
/// process environment in a test needs it.
fn credential(
    endpoint: &Endpoint,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(String, CredentialSource), Diagnostic> {
    if let Some(name) = &endpoint.api_key_env {
        if let Some(value) = present(env(name)) {
            return Ok((value, CredentialSource::ConfiguredEnv));
        }
    }
    if let Some(handle) = &endpoint.credential {
        match OsCredentialStore.resolve(handle.name()) {
            Ok(value) => {
                if let Some(value) = present(Some(value)) {
                    return Ok((value, CredentialSource::Keyring));
                }
            }
            Err(SecretError::NotFound(_)) => {}
            Err(error) => return Err(credential_failed(&endpoint.id, error)),
        }
    }
    if let Some(value) = present(env(endpoint.kind.default_api_key_env())) {
        return Ok((value, CredentialSource::DefaultEnv));
    }
    Err(Diagnostic::error(
        ARSY_PRV_1000,
        format!("no credential is available for provider `{}`", endpoint.id),
        format!(
            "run `arsy auth set {}` and point `credential` at the handle it prints, or export {}",
            endpoint.id,
            endpoint
                .api_key_env
                .as_deref()
                .unwrap_or_else(|| endpoint.kind.default_api_key_env())
        ),
    ))
}

fn from_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// A source that is present but blank counts as absent, so an accidentally
/// empty export or keyring entry falls through to the next source instead of
/// failing at the wire as an authentication error.
fn present(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// A handle to name the credential in redacted output.
///
/// The configured keyring handle when there is one, otherwise a synthetic
/// handle naming the variable it came from, so `[redacted:secret://env/...]`
/// still tells an operator which credential was masked.
fn redaction_handle(
    endpoint: &Endpoint,
    source: CredentialSource,
) -> Result<SecretHandle, Diagnostic> {
    match (source, &endpoint.credential) {
        (CredentialSource::Keyring, Some(handle)) => Ok(handle.clone()),
        (_, _) => {
            let name = match source {
                CredentialSource::ConfiguredEnv => endpoint
                    .api_key_env
                    .as_deref()
                    .unwrap_or(endpoint.kind.default_api_key_env()),
                _ => endpoint.kind.default_api_key_env(),
            };
            SecretHandle::new("env", name).map_err(|error| credential_failed(&endpoint.id, error))
        }
    }
}

fn credential_failed(provider: &str, error: impl ToString) -> Diagnostic {
    Diagnostic::error(
        ARSY_PRV_1000,
        format!(
            "the credential for provider `{provider}` is unusable: {}",
            error.to_string()
        ),
        "unlock the OS credential store, or re-run `arsy auth set` for this provider",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::config::Layer;

    fn config(body: &str) -> Config {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        Config::load(&[(Layer::User, path)]).unwrap()
    }

    /// An environment holding exactly one variable.
    fn env(name: &'static str, value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |asked| (asked == name).then(|| value.to_owned())
    }

    #[test]
    fn a_named_environment_variable_wins_and_reports_its_source() {
        let config = config(
            r#"
schema_version = 1
[provider.endpoint.local]
kind = "openai"
base_url = "http://localhost:11434/v1"
api_key_env = "LOCAL_KEY"
"#,
        );
        let endpoint = config.endpoint(None).unwrap();

        assert_eq!(
            credential(endpoint, &env("LOCAL_KEY", "sk-from-env")).unwrap(),
            ("sk-from-env".to_owned(), CredentialSource::ConfiguredEnv)
        );
        assert_eq!(
            credential(endpoint, &env("OPENAI_API_KEY", "sk-conventional"))
                .unwrap()
                .1,
            CredentialSource::DefaultEnv,
            "the dialect's conventional variable is the last resort"
        );

        // Set but empty is treated as unset, so a blank export falls through
        // instead of failing at the wire as an authentication error.
        let error = credential(endpoint, &env("LOCAL_KEY", "")).unwrap_err();
        assert_eq!(error.code, ARSY_PRV_1000);
        assert!(
            error.remediation.contains("LOCAL_KEY"),
            "the remediation names the variable the operator configured: {}",
            error.remediation
        );
    }

    #[test]
    fn a_credential_is_named_for_redaction_whatever_source_it_came_from() {
        let config = config(
            r#"
schema_version = 1
[provider.endpoint.local]
kind = "openai"
api_key_env = "LOCAL_KEY"
credential = "secret://os/local"
"#,
        );
        let endpoint = config.endpoint(None).unwrap();

        assert_eq!(
            redaction_handle(endpoint, CredentialSource::Keyring)
                .unwrap()
                .to_string(),
            "secret://os/local"
        );
        assert_eq!(
            redaction_handle(endpoint, CredentialSource::ConfiguredEnv)
                .unwrap()
                .to_string(),
            "secret://env/LOCAL_KEY",
            "a key from the environment is masked too, not only a stored one"
        );
        assert_eq!(
            redaction_handle(endpoint, CredentialSource::DefaultEnv)
                .unwrap()
                .to_string(),
            "secret://env/OPENAI_API_KEY"
        );
    }

    #[test]
    fn an_unconfigured_or_misnamed_provider_is_a_diagnostic_not_a_fallback() {
        let empty = config("schema_version = 1\n");
        assert_eq!(
            resolve(&empty, None).err().map(|error| error.code),
            Some(ARSY_PRV_1000.to_owned())
        );

        let one = config(
            r#"
schema_version = 1
[provider.endpoint.local]
kind = "openai"
"#,
        );
        let message = resolve(&one, Some("typo")).err().unwrap().message;
        assert!(
            message.contains("`typo`"),
            "a misspelled provider must not silently resolve to another one: {message}"
        );
    }
}

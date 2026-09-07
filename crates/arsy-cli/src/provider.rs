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
    oauth::{self, TokenSet},
    provider::{
        anthropic::AnthropicProvider, google_code_assist::GoogleCodeAssistProvider,
        http::HttpTransport, openai::OpenAiProvider, openai_responses::OpenAiResponsesProvider,
        wire::ApiKey, ModelProvider,
    },
    secret::{
        CredentialStore, FileCredentialStore, OsCredentialStore, Redactor, SecretError,
        SecretHandle, FILE_STORE_ID, OS_STORE_ID,
    },
};
use std::sync::Arc;

/// Where a credential came from. Reported by `arsy doctor` so an operator can
/// tell a keyring entry from an inherited environment variable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialSource {
    ConfiguredEnv,
    Keyring,
    File,
    OAuth,
    DefaultEnv,
}

impl CredentialSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfiguredEnv => "configured_env",
            Self::Keyring => "keyring",
            Self::File => "file",
            Self::OAuth => "oauth",
            Self::DefaultEnv => "default_env",
        }
    }

    /// Where a stored key actually came from, so `arsy doctor` names the store
    /// that answered rather than assuming the keychain did.
    fn stored_in(store: &str) -> Self {
        match store {
            FILE_STORE_ID => Self::File,
            _ => Self::Keyring,
        }
    }
}

/// The provider a turn will use, plus how it was assembled.
///
/// The adapter is shared rather than owned so a turn can be streamed on its
/// own thread while the terminal keeps repainting.
pub struct Resolved {
    pub provider: Arc<dyn ModelProvider>,
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
    let provider: Arc<dyn ModelProvider> = match endpoint.kind {
        Dialect::Anthropic => Arc::new(
            AnthropicProvider::with_base_url(&endpoint.base_url, key, transport)
                .with_redactor(redactor),
        ),
        Dialect::Openai => Arc::new(
            OpenAiProvider::with_base_url(&endpoint.base_url, key, transport)
                .with_id(&endpoint.id)
                .with_redactor(redactor),
        ),
        Dialect::OpenaiResponses => Arc::new(
            OpenAiResponsesProvider::with_base_url(&endpoint.base_url, key, transport)
                .with_id(&endpoint.id)
                .with_redactor(redactor),
        ),
        Dialect::GoogleCodeAssist => Arc::new(
            GoogleCodeAssistProvider::with_base_url(&endpoint.base_url, key, transport)
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
        // The store half of the handle decides where to look. An unknown one is
        // an error rather than a quiet fall back to the keychain: a handle that
        // names a store ARSY does not have must not resolve to a different
        // credential than it asked for.
        let resolved = match handle.store() {
            OS_STORE_ID => OsCredentialStore.resolve(handle.name()),
            FILE_STORE_ID => FileCredentialStore.resolve(handle.name()),
            other => Err(SecretError::UnknownStore(other.to_owned())),
        };
        match resolved {
            Ok(value) => {
                if let Some(value) = present(Some(value)) {
                    return stored(endpoint, handle, value);
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

/// Interpret what the credential store holds for this endpoint.
///
/// `arsy auth login` writes a token set as JSON under the same handle an API
/// key would use, so the two are told apart by shape rather than by a second
/// lookup. An expired access token is refreshed and written back here, which
/// is the only place that can happen before the value reaches the wire.
/// What the credential store is holding for this endpoint.
#[derive(Debug, Eq, PartialEq)]
enum Stored {
    /// Anything that is not a token set, taken verbatim.
    ApiKey(String),
    Token(TokenSet),
    /// A token set that has to be renewed before it is used.
    Expired(TokenSet),
}

/// Tell an API key from a stored login by shape.
///
/// `arsy auth login` writes a token set as JSON under the same handle an API
/// key would use, so there is no second lookup to disambiguate them. Anything
/// that does not deserialize as a token set is an API key, which keeps a key
/// that happens to look like JSON usable.
fn classify(value: String, now: u64) -> Stored {
    match serde_json::from_str::<TokenSet>(&value) {
        Err(_) => Stored::ApiKey(value),
        Ok(tokens) if tokens.is_expired(now) => Stored::Expired(tokens),
        Ok(tokens) => Stored::Token(tokens),
    }
}

/// Interpret what the credential store holds, renewing a lapsed login.
///
/// This is the only place a refresh can happen before the value reaches the
/// wire, so it is also the only place the renewed token can be written back.
fn stored(
    endpoint: &Endpoint,
    handle: &SecretHandle,
    value: String,
) -> Result<(String, CredentialSource), Diagnostic> {
    let tokens = match classify(value, oauth::now()) {
        Stored::ApiKey(value) => return Ok((value, CredentialSource::stored_in(handle.store()))),
        Stored::Token(tokens) => return Ok((tokens.access_token, CredentialSource::OAuth)),
        Stored::Expired(tokens) => tokens,
    };
    // A hand-configured endpoint carries its own `[oauth]`; a built-in preset
    // does not, so fall back to the preset that shares the endpoint's id.
    let oauth_client = endpoint
        .oauth
        .clone()
        .or_else(|| oauth::presets::get(&endpoint.id).map(|preset| preset.oauth()))
        .ok_or_else(|| {
            Diagnostic::error(
                ARSY_PRV_1000,
                format!(
                    "the stored login for provider `{}` has expired and its OAuth client is no \
                     longer configured",
                    endpoint.id
                ),
                format!(
                    "restore the `[provider.endpoint.{}.oauth]` table",
                    endpoint.id
                ),
            )
        })?;
    let refreshed =
        oauth::refresh(&HttpTransport::default(), &oauth_client, &tokens).map_err(|error| {
            Diagnostic::error(
                ARSY_PRV_1000,
                format!(
                    "the stored login for provider `{}` could not be renewed: {error}",
                    endpoint.id
                ),
                format!("run `arsy auth login {}` again", endpoint.id),
            )
        })?;
    // Written back before use: a rotated refresh token is single-use, so
    // losing it here would cost the operator a re-login on the next run.
    let raw = serde_json::to_string(&refreshed)
        .map_err(|error| credential_failed(&endpoint.id, error))?;
    OsCredentialStore
        .set(handle.name(), &raw)
        .map_err(|error| credential_failed(&endpoint.id, error))?;
    Ok((refreshed.access_token, CredentialSource::OAuth))
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
        (CredentialSource::Keyring | CredentialSource::OAuth, Some(handle)) => Ok(handle.clone()),
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

    /// The keychain is one store among the handle's choices, not the place
    /// every handle ends up.
    #[test]
    fn the_handle_decides_which_store_answers_and_an_unknown_one_is_refused() {
        use std::io::Write;

        let directory = tempfile::tempdir().unwrap();
        let key = directory.path().join("myai.key");
        let mut file = std::fs::File::create(&key).unwrap();
        file.write_all(b"sk-from-a-file\n").unwrap();
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let endpoint_config = |handle: String| {
            config(&format!(
                r#"
schema_version = 1
[provider.endpoint.myai]
kind = "openai"
base_url = "https://example.test/v1"
credential = "{handle}"
"#
            ))
        };

        // A file handle is answered by the file, and reported as the file, so
        // `arsy doctor` does not claim a keychain that was never opened.
        let config = endpoint_config(format!("secret://file/{}", key.display()));
        let endpoint = config.endpoint(None).unwrap();
        assert_eq!(
            credential(endpoint, &env("UNUSED", "")).unwrap(),
            ("sk-from-a-file".to_owned(), CredentialSource::File)
        );

        // A store ARSY does not have must not quietly become the keychain.
        let config = endpoint_config("secret://vault/myai".to_owned());
        let endpoint = config.endpoint(None).unwrap();
        let error = credential(endpoint, &env("UNUSED", "")).unwrap_err();
        assert!(
            error.message.contains("vault"),
            "an unknown store did not name itself: {}",
            error.message
        );
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
            redaction_handle(endpoint, CredentialSource::OAuth)
                .unwrap()
                .to_string(),
            "secret://os/local",
            "an access token is masked under the handle it was stored against"
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
    fn a_stored_credential_is_told_apart_by_shape_not_by_a_second_lookup() {
        let now = 1_000_000;
        let token = |body: &str| classify(body.to_owned(), now);

        assert_eq!(
            token("sk-ant-api03-plain-key"),
            Stored::ApiKey("sk-ant-api03-plain-key".to_owned())
        );
        assert_eq!(
            token(r#"{"note":"not a login"}"#),
            Stored::ApiKey(r#"{"note":"not a login"}"#.to_owned()),
            "JSON that is not a token set is still an API key, taken verbatim"
        );

        let live = format!(r#"{{"access_token":"at","expires_at":{}}}"#, now + 3600);
        assert!(matches!(token(&live), Stored::Token(_)));

        assert!(
            matches!(token(r#"{"access_token":"at"}"#), Stored::Token(_)),
            "a login with no stated expiry is taken at face value, not refreshed every time"
        );

        assert!(matches!(
            token(&format!(
                r#"{{"access_token":"at","expires_at":{}}}"#,
                now - 1
            )),
            Stored::Expired(_)
        ));
        assert!(
            matches!(
                token(&format!(
                    r#"{{"access_token":"at","expires_at":{}}}"#,
                    now + oauth::EXPIRY_MARGIN.as_secs() - 1
                )),
                Stored::Expired(_)
            ),
            "a token inside the margin is renewed, so it cannot lapse between check and use"
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

//! Native configuration: `config.toml` resolved across authority layers.
//!
//! See `docs/35-configuration.md`. Only the keys the runtime can act on today
//! are applied; the rest of the documented schema is accepted and ignored, so a
//! valid file is never rejected for being ahead of the implementation, while a
//! key that belongs to no documented section still fails loudly.
//!
//! Provider endpoints are the security-relevant part of this module. A
//! `base_url` decides where prompts and a credential are sent, so a file inside
//! a repository must not be able to set one: cloning a repository would
//! otherwise redirect the model call to whatever host that repository names.
//! Endpoint keys are therefore accepted from the enterprise and user layers
//! only, and reported as a diagnostic anywhere else.

use crate::secret::SecretHandle;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

/// The only schema this build understands.
pub const SCHEMA_VERSION: i64 = 1;

/// Name every configuration layer uses.
pub const CONFIG_FILE: &str = "config.toml";

/// Documented sections that parse but have no runtime effect yet. Listing them
/// keeps "unknown keys are errors" true without rejecting a forward-looking
/// file.
const INERT_SECTIONS: &[&str] = &[
    "compat",
    "context",
    "execution",
    "git",
    "policy",
    "sandbox",
    "storage",
    "telemetry",
    "ui",
];

/// Where a value came from, in ascending authority order.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Enterprise,
    User,
    Workspace,
    Nested,
}

impl Layer {
    /// Whether the layer's file is under the operator's own control.
    ///
    /// Workspace and nested files travel with a repository, so they are
    /// untrusted content: they may express intent but cannot name an endpoint
    /// or a credential.
    pub const fn is_trusted(self) -> bool {
        matches!(self, Self::Enterprise | Self::User)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enterprise => "enterprise",
            Self::User => "user",
            Self::Workspace => "workspace",
            Self::Nested => "nested",
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Wire dialect an endpoint speaks. This selects the adapter, and nothing else
/// about a provider is inferred from its name.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    Anthropic,
    Openai,
}

impl Dialect {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
        }
    }

    /// Base URL used when an endpoint names a dialect but no host.
    pub const fn default_base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::Openai => "https://api.openai.com/v1",
        }
    }

    /// Environment variable consulted last, after config and the keyring.
    pub const fn default_api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Openai => "OPENAI_API_KEY",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::Openai),
            _ => None,
        }
    }
}

impl fmt::Display for Dialect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Config-driven OAuth client. No provider's client identifier is built in:
/// an operator supplies the whole client, so this works for any issuer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OAuth {
    pub authorize_url: String,
    pub token_url: String,
    /// Present when the issuer supports RFC 8628, which needs no loopback port.
    pub device_authorization_url: Option<String>,
    pub client_id: String,
    pub scopes: Vec<String>,
}

/// One resolved provider endpoint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Endpoint {
    pub id: String,
    pub kind: Dialect,
    pub base_url: String,
    /// Keyring handle. Never a value: this struct is safe to print.
    pub credential: Option<SecretHandle>,
    pub api_key_env: Option<String>,
    pub model: Option<String>,
    /// Cap on one response. Providers differ in what they accept and the
    /// Anthropic dialect requires a value, so it is configurable rather than
    /// fixed.
    pub max_output_tokens: u32,
    pub oauth: Option<OAuth>,
}

/// Response cap used when an endpoint does not set one. Large enough for a
/// substantial edit, small enough to bound a runaway response.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;

/// Effective value of one key and the file it won from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Origin {
    pub value: String,
    pub layer: Layer,
    pub path: PathBuf,
}

/// An input that was understood but not applied. Never fatal: the run
/// continues without the rejected value, and `arsy config explain` shows why.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    pub key: String,
    pub layer: Layer,
    pub path: PathBuf,
    pub message: String,
}

/// A file that could not be trusted to mean what it says, so the whole load
/// fails rather than proceeding with a partly-applied policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigError {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Debug, Default)]
pub struct Config {
    provider_default: Option<String>,
    model_default: Option<String>,
    endpoints: BTreeMap<String, Endpoint>,
    trace: BTreeMap<String, Origin>,
    diagnostics: Vec<Diagnostic>,
}

impl Config {
    /// Read every layer in authority order. A missing file is not an error;
    /// an unreadable or invalid one is.
    pub fn load(layers: &[(Layer, PathBuf)]) -> Result<Self, ConfigError> {
        let mut config = Self::default();
        for (layer, path) in layers {
            let raw = match std::fs::read_to_string(path) {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(ConfigError {
                        path: path.clone(),
                        message: error.to_string(),
                    })
                }
            };
            config.apply(*layer, path, &raw)?;
        }
        Ok(config)
    }

    pub fn provider_default(&self) -> Option<&str> {
        self.provider_default.as_deref()
    }

    pub fn model_default(&self) -> Option<&str> {
        self.model_default.as_deref()
    }

    pub fn endpoints(&self) -> impl Iterator<Item = &Endpoint> {
        self.endpoints.values()
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The endpoint a turn should use: the requested one, else the configured
    /// default, else the only one there is.
    ///
    /// A single configured endpoint needs no `provider.default`, and naming a
    /// provider that does not exist is `None` rather than a silent fallback to
    /// some other endpoint.
    pub fn endpoint(&self, requested: Option<&str>) -> Option<&Endpoint> {
        match requested.or(self.provider_default.as_deref()) {
            Some(id) => self.endpoints.get(id),
            None if self.endpoints.len() == 1 => self.endpoints.values().next(),
            None => None,
        }
    }

    /// Effective values with their sources, optionally narrowed to one key or
    /// key prefix. This is what `arsy config explain` prints.
    pub fn explain(&self, key: Option<&str>) -> serde_json::Value {
        let selected: BTreeMap<_, _> = self
            .trace
            .iter()
            .filter(|(name, _)| key.is_none_or(|key| matches(name, key)))
            .collect();
        serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "values": selected,
            "diagnostics": self.diagnostics,
        })
    }

    fn apply(&mut self, layer: Layer, path: &Path, raw: &str) -> Result<(), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        let table: toml::Table = raw.parse().map_err(|error: toml::de::Error| {
            reject(format!("is not valid TOML: {}", error.message()))
        })?;
        match table
            .get("schema_version")
            .and_then(toml::Value::as_integer)
        {
            Some(SCHEMA_VERSION) => {}
            Some(other) => return Err(reject(format!("unsupported schema_version {other}"))),
            None => return Err(reject("requires `schema_version = 1`".to_owned())),
        }
        for (key, value) in &table {
            match key.as_str() {
                "schema_version" => {}
                "provider" => self.apply_provider(layer, path, value)?,
                "model" => {
                    let table = as_table(value, "model", path)?;
                    if let Some(default) = string(table, "default", "model.default", path)? {
                        self.model_default = Some(default.clone());
                        self.record(layer, path, "model.default", default);
                    }
                }
                section if INERT_SECTIONS.contains(&section) => {}
                other => return Err(reject(format!("unknown key `{other}`"))),
            }
        }
        Ok(())
    }

    fn apply_provider(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        for (key, value) in as_table(value, "provider", path)? {
            match key.as_str() {
                "default" => {
                    let id = expect_string(value, "provider.default", path)?;
                    self.provider_default = Some(id.clone());
                    self.record(layer, path, "provider.default", id);
                }
                "endpoint" => self.apply_endpoints(layer, path, value)?,
                // Documented, resolved by a later phase.
                "allowed" | "residency" | "credential" => {}
                other => {
                    return Err(ConfigError {
                        path: path.to_path_buf(),
                        message: format!("unknown key `provider.{other}`"),
                    })
                }
            }
        }
        Ok(())
    }

    fn apply_endpoints(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let table = as_table(value, "provider.endpoint", path)?;
        if !layer.is_trusted() {
            // Refused, not merged: see the module note on repository content.
            for id in table.keys() {
                self.diagnostics.push(Diagnostic {
                    key: format!("provider.endpoint.{id}"),
                    layer,
                    path: path.to_path_buf(),
                    message: "a provider endpoint may only be set by the enterprise or user \
                              configuration, because it decides where prompts and credentials go"
                        .to_owned(),
                });
            }
            return Ok(());
        }
        for (id, value) in table {
            self.apply_endpoint(layer, path, id, value)?;
        }
        Ok(())
    }

    fn apply_endpoint(
        &mut self,
        layer: Layer,
        path: &Path,
        id: &str,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let prefix = format!("provider.endpoint.{id}");
        let table = as_table(value, &prefix, path)?;
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        for key in table.keys() {
            if !matches!(
                key.as_str(),
                "kind"
                    | "base_url"
                    | "credential"
                    | "api_key_env"
                    | "model"
                    | "max_output_tokens"
                    | "oauth"
            ) {
                return Err(reject(format!("unknown key `{prefix}.{key}`")));
            }
        }

        // A dialect is required the first time; a later layer may refine an
        // endpoint it already knows without repeating it.
        let kind = match string(table, "kind", &format!("{prefix}.kind"), path)? {
            Some(raw) => Dialect::parse(raw).ok_or_else(|| {
                reject(format!(
                    "`{prefix}.kind` must be \"anthropic\" or \"openai\", not \"{raw}\""
                ))
            })?,
            None => match self.endpoints.get(id) {
                Some(existing) => existing.kind,
                None => return Err(reject(format!("`{prefix}` requires `kind`"))),
            },
        };

        let mut endpoint = self.endpoints.remove(id).unwrap_or(Endpoint {
            id: id.to_owned(),
            kind,
            base_url: kind.default_base_url().to_owned(),
            credential: None,
            api_key_env: None,
            model: None,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            oauth: None,
        });
        if endpoint.kind != kind {
            endpoint.kind = kind;
            endpoint.base_url = kind.default_base_url().to_owned();
        }
        self.record(layer, path, &format!("{prefix}.kind"), kind.as_str());
        self.record(
            layer,
            path,
            &format!("{prefix}.base_url"),
            &endpoint.base_url,
        );

        if let Some(base_url) = string(table, "base_url", &format!("{prefix}.base_url"), path)? {
            validate_base_url(base_url).map_err(|message| {
                reject(format!("`{prefix}.base_url` {message}: \"{base_url}\""))
            })?;
            endpoint.base_url = base_url.trim_end_matches('/').to_owned();
            self.record(
                layer,
                path,
                &format!("{prefix}.base_url"),
                &endpoint.base_url,
            );
        }
        if let Some(raw) = string(table, "credential", &format!("{prefix}.credential"), path)? {
            let handle = SecretHandle::try_from(raw.clone())
                .map_err(|error| reject(format!("`{prefix}.credential` {error}")))?;
            self.record(
                layer,
                path,
                &format!("{prefix}.credential"),
                handle.to_string(),
            );
            endpoint.credential = Some(handle);
        }
        if let Some(name) = string(table, "api_key_env", &format!("{prefix}.api_key_env"), path)? {
            self.record(layer, path, &format!("{prefix}.api_key_env"), name);
            endpoint.api_key_env = Some(name.clone());
        }
        if let Some(model) = string(table, "model", &format!("{prefix}.model"), path)? {
            self.record(layer, path, &format!("{prefix}.model"), model);
            endpoint.model = Some(model.clone());
        }
        if let Some(value) = table.get("max_output_tokens") {
            let key = format!("{prefix}.max_output_tokens");
            let tokens = value
                .as_integer()
                .and_then(|tokens| u32::try_from(tokens).ok())
                .filter(|tokens| *tokens > 0)
                .ok_or_else(|| reject(format!("`{key}` must be a positive integer")))?;
            self.record(layer, path, &key, tokens.to_string());
            endpoint.max_output_tokens = tokens;
        }
        if let Some(oauth) = table.get("oauth") {
            endpoint.oauth = Some(self.apply_oauth(layer, path, &prefix, oauth)?);
        }

        self.endpoints.insert(id.to_owned(), endpoint);
        Ok(())
    }

    fn apply_oauth(
        &mut self,
        layer: Layer,
        path: &Path,
        parent: &str,
        value: &toml::Value,
    ) -> Result<OAuth, ConfigError> {
        let prefix = format!("{parent}.oauth");
        let table = as_table(value, &prefix, path)?;
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        for key in table.keys() {
            if !matches!(
                key.as_str(),
                "authorize_url" | "token_url" | "device_authorization_url" | "client_id" | "scopes"
            ) {
                return Err(reject(format!("unknown key `{prefix}.{key}`")));
            }
        }
        let required = |name: &str| -> Result<String, ConfigError> {
            let key = format!("{prefix}.{name}");
            let value = string(table, name, &key, path)?
                .ok_or_else(|| reject(format!("`{prefix}` requires `{name}`")))?;
            Ok(value.clone())
        };
        let authorize_url = required("authorize_url")?;
        let token_url = required("token_url")?;
        let client_id = required("client_id")?;
        let device_authorization_url = string(
            table,
            "device_authorization_url",
            &format!("{prefix}.device_authorization_url"),
            path,
        )?
        .cloned();
        for (name, url) in [("authorize_url", &authorize_url), ("token_url", &token_url)]
            .into_iter()
            .chain(
                device_authorization_url
                    .iter()
                    .map(|url| ("device_authorization_url", url)),
            )
        {
            validate_base_url(url)
                .map_err(|message| reject(format!("`{prefix}.{name}` {message}: \"{url}\"")))?;
        }
        let scopes = match table.get("scopes") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .ok_or_else(|| reject(format!("`{prefix}.scopes` must be an array of strings")))?
                .iter()
                .map(|scope| {
                    scope.as_str().map(str::to_owned).ok_or_else(|| {
                        reject(format!("`{prefix}.scopes` must be an array of strings"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        self.record(layer, path, &format!("{prefix}.client_id"), &client_id);
        self.record(layer, path, &format!("{prefix}.token_url"), &token_url);
        Ok(OAuth {
            authorize_url,
            token_url,
            device_authorization_url,
            client_id,
            scopes,
        })
    }

    fn record(&mut self, layer: Layer, path: &Path, key: &str, value: impl Into<String>) {
        self.trace.insert(
            key.to_owned(),
            Origin {
                value: value.into(),
                layer,
                path: path.to_path_buf(),
            },
        );
    }
}

/// `key` itself, or any key beneath it when `key` names a section.
fn matches(name: &str, key: &str) -> bool {
    name == key
        || name
            .strip_prefix(key)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Enough of a URL check to fail early and visibly. Whether plaintext is
/// acceptable for a given host is a transport decision, not a parse one.
fn validate_base_url(raw: &str) -> Result<(), &'static str> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .ok_or("must start with http:// or https://")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        return Err("has no host");
    }
    Ok(())
}

fn as_table<'a>(
    value: &'a toml::Value,
    key: &str,
    path: &Path,
) -> Result<&'a toml::Table, ConfigError> {
    value.as_table().ok_or_else(|| ConfigError {
        path: path.to_path_buf(),
        message: format!("`{key}` must be a table"),
    })
}

fn string<'a>(
    table: &'a toml::Table,
    name: &str,
    key: &str,
    path: &Path,
) -> Result<Option<&'a String>, ConfigError> {
    match table.get(name) {
        None => Ok(None),
        Some(toml::Value::String(value)) if !value.is_empty() => Ok(Some(value)),
        Some(toml::Value::String(_)) => Err(ConfigError {
            path: path.to_path_buf(),
            message: format!("`{key}` must not be empty"),
        }),
        Some(_) => Err(ConfigError {
            path: path.to_path_buf(),
            message: format!("`{key}` must be a string"),
        }),
    }
}

fn expect_string<'a>(
    value: &'a toml::Value,
    key: &str,
    path: &Path,
) -> Result<&'a String, ConfigError> {
    match value {
        toml::Value::String(value) if !value.is_empty() => Ok(value),
        _ => Err(ConfigError {
            path: path.to_path_buf(),
            message: format!("`{key}` must be a non-empty string"),
        }),
    }
}

/// Configuration files in authority order, per `docs/35-configuration.md`.
///
/// Nested files run from the workspace root toward `working`, parent before
/// child, so a deeper file wins. A `working` directory outside the workspace
/// contributes nothing.
pub fn layers(workspace: &Path, working: &Path) -> Vec<(Layer, PathBuf)> {
    let mut layers = Vec::new();
    if let Some(path) = enterprise_config() {
        layers.push((Layer::Enterprise, path));
    }
    if let Some(path) = user_config() {
        layers.push((Layer::User, path));
    }
    layers.push((Layer::Workspace, workspace.join(".arsy").join(CONFIG_FILE)));
    if let Ok(relative) = working.strip_prefix(workspace) {
        let mut directory = workspace.to_path_buf();
        for component in relative.components() {
            directory.push(component);
            layers.push((Layer::Nested, directory.join(".arsy").join(CONFIG_FILE)));
        }
    }
    layers
}

#[cfg(target_os = "linux")]
pub fn enterprise_config() -> Option<PathBuf> {
    Some(PathBuf::from("/etc/arsy/config.toml"))
}

#[cfg(target_os = "macos")]
pub fn enterprise_config() -> Option<PathBuf> {
    Some(PathBuf::from(
        "/Library/Application Support/ARSY/config.toml",
    ))
}

#[cfg(target_os = "windows")]
pub fn enterprise_config() -> Option<PathBuf> {
    std::env::var_os("ProgramData").map(|base| Path::new(&base).join("ARSY/config.toml"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn enterprise_config() -> Option<PathBuf> {
    None
}

/// Directory that replaces the platform user-configuration location.
///
/// The same override the Codex CLI offers as `CODEX_HOME`. It exists so a run
/// can be pointed at a throwaway configuration — a test, a container, a second
/// account — without editing the operator's own file.
pub const CONFIG_HOME_VAR: &str = "ARSY_CONFIG_HOME";

pub fn user_config() -> Option<PathBuf> {
    match std::env::var_os(CONFIG_HOME_VAR) {
        Some(home) if !home.is_empty() => Some(Path::new(&home).join(CONFIG_FILE)),
        _ => platform_user_config(),
    }
}

#[cfg(target_os = "linux")]
fn platform_user_config() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
        .map(|base| base.join("arsy/config.toml"))
}

#[cfg(target_os = "macos")]
fn platform_user_config() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| Path::new(&home).join("Library/Application Support/ARSY/config.toml"))
}

#[cfg(target_os = "windows")]
fn platform_user_config() -> Option<PathBuf> {
    std::env::var_os("AppData").map(|base| Path::new(&base).join("ARSY/config.toml"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_user_config() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(directory: &Path, name: &str, body: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn load(files: &[(Layer, PathBuf)]) -> Config {
        Config::load(files).unwrap()
    }

    #[test]
    fn a_later_layer_replaces_a_key_and_keeps_the_rest() {
        let directory = tempfile::tempdir().unwrap();
        let enterprise = write(
            directory.path(),
            "enterprise.toml",
            r#"
schema_version = 1
[provider.endpoint.proxy]
kind = "openai"
base_url = "https://enterprise.test/v1"
api_key_env = "ENTERPRISE_KEY"
"#,
        );
        let user = write(
            directory.path(),
            "user.toml",
            r#"
schema_version = 1
[provider]
default = "proxy"
[provider.endpoint.proxy]
base_url = "https://user.test/v1/"
"#,
        );

        let config = load(&[(Layer::Enterprise, enterprise), (Layer::User, user.clone())]);
        let endpoint = config.endpoint(None).unwrap();

        assert_eq!(endpoint.kind, Dialect::Openai);
        assert_eq!(
            endpoint.base_url, "https://user.test/v1",
            "the later layer wins, and the trailing slash is normalized away"
        );
        assert_eq!(
            endpoint.api_key_env.as_deref(),
            Some("ENTERPRISE_KEY"),
            "a key the later layer did not set survives"
        );
        let trace = config.explain(Some("provider.endpoint.proxy.base_url"));
        assert_eq!(
            trace["values"]["provider.endpoint.proxy.base_url"]["layer"],
            "user"
        );
        assert_eq!(
            trace["values"]["provider.endpoint.proxy.base_url"]["path"],
            serde_json::json!(user)
        );
    }

    #[test]
    fn a_repository_cannot_name_an_endpoint() {
        let directory = tempfile::tempdir().unwrap();
        let user = write(
            directory.path(),
            "user.toml",
            r#"
schema_version = 1
[provider.endpoint.official]
kind = "anthropic"
"#,
        );
        let workspace = write(
            directory.path(),
            "workspace.toml",
            r#"
schema_version = 1
[provider.endpoint.official]
kind = "anthropic"
base_url = "https://attacker.test"
credential = "secret://os/official"
"#,
        );

        let config = load(&[(Layer::User, user), (Layer::Workspace, workspace)]);

        assert_eq!(
            config.endpoint(None).unwrap().base_url,
            Dialect::Anthropic.default_base_url(),
            "the workspace file must not redirect the endpoint"
        );
        assert_eq!(config.diagnostics().len(), 1);
        assert_eq!(config.diagnostics()[0].key, "provider.endpoint.official");
        assert_eq!(config.diagnostics()[0].layer, Layer::Workspace);
    }

    #[test]
    fn invalid_input_is_rejected_rather_than_partly_applied() {
        let directory = tempfile::tempdir().unwrap();
        let cases = [
            ("[provider]\ndefault = \"x\"\n", "requires `schema_version = 1`"),
            ("schema_version = 2\n", "unsupported schema_version 2"),
            ("schema_version = 1\n[nonsense]\na = 1\n", "unknown key `nonsense`"),
            (
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"gemini\"\n",
                "must be \"anthropic\" or \"openai\"",
            ),
            (
                "schema_version = 1\n[provider.endpoint.p]\nbase_url = \"https://x.test\"\n",
                "requires `kind`",
            ),
            (
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\nbase_url = \"x.test\"\n",
                "must start with http:// or https://",
            ),
            (
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\ncredential = \"os/p\"\n",
                "malformed",
            ),
            (
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\nport = 1\n",
                "unknown key `provider.endpoint.p.port`",
            ),
            (
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\nmax_output_tokens = 0\n",
                "`provider.endpoint.p.max_output_tokens` must be a positive integer",
            ),
        ];
        for (index, (body, expected)) in cases.into_iter().enumerate() {
            let path = write(directory.path(), &format!("case{index}.toml"), body);
            let error = Config::load(&[(Layer::User, path)]).unwrap_err();
            assert!(
                error.message.contains(expected),
                "case {index}: {:?} does not contain {expected:?}",
                error.message
            );
        }
    }

    #[test]
    fn a_missing_file_is_not_an_error_and_a_lone_endpoint_needs_no_default() {
        let directory = tempfile::tempdir().unwrap();
        let user = write(
            directory.path(),
            "user.toml",
            r#"
schema_version = 1
[model]
default = "claude-sonnet-4-6"
[provider.endpoint.local]
kind = "openai"
base_url = "http://localhost:11434/v1"
[provider.endpoint.local.oauth]
authorize_url = "https://issuer.test/authorize"
token_url = "https://issuer.test/token"
client_id = "arsy"
scopes = ["offline_access"]
"#,
        );

        let config = load(&[
            (Layer::Enterprise, directory.path().join("absent.toml")),
            (Layer::User, user),
        ]);

        let endpoint = config.endpoint(None).unwrap();
        assert_eq!(endpoint.id, "local");
        assert_eq!(endpoint.max_output_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(config.model_default(), Some("claude-sonnet-4-6"));
        assert_eq!(endpoint.oauth.as_ref().unwrap().client_id, "arsy");
        assert_eq!(endpoint.oauth.as_ref().unwrap().scopes, ["offline_access"]);
        assert!(endpoint
            .oauth
            .as_ref()
            .unwrap()
            .device_authorization_url
            .is_none());
        assert!(
            config.endpoint(Some("nope")).is_none(),
            "an unknown provider resolves to nothing, never to a different endpoint"
        );
    }

    #[test]
    fn nested_layers_run_from_the_root_toward_the_working_directory() {
        let workspace = Path::new("/w");
        let found = layers(workspace, &workspace.join("services/payments"));
        let nested: Vec<_> = found
            .iter()
            .filter(|(layer, _)| *layer == Layer::Nested)
            .map(|(_, path)| path.clone())
            .collect();
        assert_eq!(
            nested,
            [
                PathBuf::from("/w/services/.arsy/config.toml"),
                PathBuf::from("/w/services/payments/.arsy/config.toml"),
            ]
        );
        assert!(layers(workspace, Path::new("/elsewhere"))
            .iter()
            .all(|(layer, _)| *layer != Layer::Nested));
    }
}

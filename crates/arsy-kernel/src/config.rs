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
/// Where the credential catalog lives. `file` keeps it beside the user
/// configuration; `os` keeps it in the platform credential store.
///
/// The catalog holds handles, provider names, and timestamps — no secret value
/// — so an operator who does not want a keychain unlock on every turn can keep
/// it in a file without putting a key on disk.
pub const CREDENTIAL_STORES: &[&str] = &["file", "os"];
/// What an operator gets without saying: no unlock prompt to read metadata.
pub const DEFAULT_CREDENTIAL_STORE: &str = "file";

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

/// Config-driven OAuth client. An operator supplies the whole client, so this
/// works for any issuer; the built-in presets in `oauth::presets` fill the
/// same shape for the vendors ARSY ships a client for.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct OAuth {
    pub authorize_url: String,
    pub token_url: String,
    /// Present when the issuer supports RFC 8628, which needs no loopback port.
    pub device_authorization_url: Option<String>,
    pub client_id: String,
    /// Some installed-app clients (Google's, for one) still require the
    /// "secret" in the token exchange. It is not confidential for a client
    /// that ships in software, but the exchange fails without it.
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    /// Exact loopback redirect the issuer has registered, e.g.
    /// `http://localhost:1455/auth/callback`. When set, the listener binds
    /// that port and the URI is sent verbatim; otherwise a free port is taken
    /// and `http://127.0.0.1:<port>/callback` is used.
    pub redirect_uri: Option<String>,
    /// Extra query parameters for the authorization request, such as Google's
    /// `access_type=offline`.
    pub authorize_params: Vec<(String, String)>,
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
    /// Every model this endpoint offers, in the order the picker should show
    /// them. One endpoint speaks to one host, and a host serves more than one
    /// model, so the model is a list rather than a second endpoint that would
    /// duplicate the URL and the credential.
    ///
    /// `model` remains the default; it is always the first entry here.
    pub models: Vec<String>,
    /// Cap on one response. Providers differ in what they accept and the
    /// Anthropic dialect requires a value, so it is configurable rather than
    /// fixed.
    pub max_output_tokens: u32,
    pub oauth: Option<OAuth>,
}

impl Endpoint {
    /// Put the default at the head of the offered models: a picker that does
    /// not list the model the endpoint is already using cannot show what is in
    /// force.
    fn offer_default_first(&mut self) {
        if let Some(model) = &self.model {
            self.models.retain(|listed| listed != model);
            self.models.insert(0, model.clone());
        }
    }
}

/// Response cap used when an endpoint does not set one. Large enough for a
/// substantial edit, small enough to bound a runaway response.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;

/// The `[theme]` table: a built-in theme to start from, plus per-role colour
/// overrides. The CLI turns this into its palette; the kernel only carries and
/// validates it, so a headless run rejects a bad colour at load time too.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Theme {
    /// Name of a built-in theme. `None` leaves the CLI's default in force.
    pub base: Option<String>,
    /// `role -> "#rrggbb"`. Role names are the CLI's to know; the kernel only
    /// checks the colour is well formed.
    pub roles: BTreeMap<String, String>,
}

/// Whether `hex` is `#rrggbb` (the `#` optional), the one colour form `[theme]`
/// accepts.
fn valid_hex(hex: &str) -> bool {
    let body = hex.strip_prefix('#').unwrap_or(hex);
    body.len() == 6 && body.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The port of a loopback OAuth redirect URI, or `None` when it is not one:
/// `http`/`https`, a loopback host, and an explicit port. The login binds this
/// port so the issuer's registered redirect resolves to ARSY's own listener.
pub fn redirect_loopback_port(uri: &str) -> Option<u16> {
    let rest = uri
        .strip_prefix("http://")
        .or_else(|| uri.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = authority.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return None;
    }
    port.parse().ok()
}

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
    credential_store: Option<String>,
    endpoints: BTreeMap<String, Endpoint>,
    theme: Theme,
    trace: BTreeMap<String, Origin>,
    diagnostics: Vec<Diagnostic>,
}

impl Config {
    /// Every configured endpoint id, in configuration order, so a picker can
    /// offer them without the caller reaching into the map.
    pub fn endpoint_ids(&self) -> Vec<String> {
        self.endpoints.keys().cloned().collect()
    }

    /// Which store the credential catalog is kept in.
    pub fn credential_store(&self) -> &str {
        self.credential_store
            .as_deref()
            .unwrap_or(DEFAULT_CREDENTIAL_STORE)
    }

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

    /// The `[theme]` table, empty when the file did not set one.
    pub fn theme(&self) -> &Theme {
        &self.theme
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
                "credentials" => {
                    let table = as_table(value, "credentials", path)?;
                    if let Some(store) = string(table, "store", "credentials.store", path)?.cloned()
                    {
                        if !CREDENTIAL_STORES.contains(&store.as_str()) {
                            return Err(reject(format!(
                                "credentials.store must be one of {}, not `{store}`",
                                CREDENTIAL_STORES.join(", ")
                            )));
                        }
                        self.credential_store = Some(store.clone());
                        self.record(layer, path, "credentials.store", store);
                    }
                }
                "theme" => self.apply_theme(layer, path, value)?,
                section if INERT_SECTIONS.contains(&section) => {}
                other => return Err(reject(format!("unknown key `{other}`"))),
            }
        }
        Ok(())
    }

    fn apply_theme(
        &mut self,
        layer: Layer,
        path: &Path,
        value: &toml::Value,
    ) -> Result<(), ConfigError> {
        let reject = |message: String| ConfigError {
            path: path.to_path_buf(),
            message,
        };
        for (key, value) in as_table(value, "theme", path)? {
            if key == "base" {
                let base = expect_string(value, "theme.base", path)?;
                self.theme.base = Some(base.clone());
                self.record(layer, path, "theme.base", base);
                continue;
            }
            // Any other key is a role name. The kernel does not police the set
            // of roles (that is the CLI's), only that the value is a colour.
            let hex = expect_string(value, &format!("theme.{key}"), path)?;
            if !valid_hex(hex) {
                return Err(reject(format!(
                    "`theme.{key}` must be a #rrggbb colour, not `{hex}`"
                )));
            }
            self.theme.roles.insert(key.clone(), hex.clone());
            self.record(layer, path, &format!("theme.{key}"), hex);
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
                    | "models"
                    | "max_output_tokens"
                    | "oauth"
            ) {
                return Err(reject(format!("unknown key `{prefix}.{key}`")));
            }
        }

        // A dialect is required the first time; a later layer may refine an
        // endpoint it already knows without repeating it.
        let stated_kind = string(table, "kind", &format!("{prefix}.kind"), path)?
            .map(|raw| {
                Dialect::parse(raw).ok_or_else(|| {
                    reject(format!(
                        "`{prefix}.kind` must be \"anthropic\" or \"openai\", not \"{raw}\""
                    ))
                })
            })
            .transpose()?;
        let existing = self.endpoints.remove(id);
        let kind = match (stated_kind, &existing) {
            (Some(kind), _) => kind,
            (None, Some(existing)) => existing.kind,
            (None, None) => return Err(reject(format!("`{prefix}` requires `kind`"))),
        };

        let introduced = existing.is_none();
        let mut endpoint = existing.unwrap_or(Endpoint {
            id: id.to_owned(),
            kind,
            base_url: kind.default_base_url().to_owned(),
            credential: None,
            api_key_env: None,
            model: None,
            models: Vec::new(),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            oauth: None,
        });
        // Changing the dialect changes which API the default base URL names,
        // so one inherited from the previous dialect cannot be kept.
        let redialected = endpoint.kind != kind;
        if redialected {
            endpoint.kind = kind;
            endpoint.base_url = kind.default_base_url().to_owned();
        }
        // Only a layer that actually supplied a value may claim it in the
        // trace. Attributing an inherited value to the last layer that merely
        // mentioned the endpoint would make `arsy config explain` name the
        // wrong file, which is the one thing it exists to get right.
        if introduced || stated_kind.is_some() {
            self.record(layer, path, &format!("{prefix}.kind"), kind.as_str());
        }
        if introduced || redialected {
            self.record(
                layer,
                path,
                &format!("{prefix}.base_url"),
                &endpoint.base_url,
            );
        }

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
            // The rejected value is never quoted back. This key is where an
            // operator is most likely to paste a real API key by mistake, and
            // a diagnostic travels to stdout, logs, and CI output long before
            // any redaction pipeline is holding that value.
            let handle = SecretHandle::try_from(raw.clone()).map_err(|_| {
                reject(format!(
                    "`{prefix}.credential` must be a handle such as \"secret://os/{id}\", not a \
                     credential; store the value with `arsy auth set {id}` instead"
                ))
            })?;
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
        if let Some(value) = table.get("models") {
            let key = format!("{prefix}.models");
            let models = model_list(value, &key, path)?;
            self.record(layer, path, &key, models.join(", "));
            endpoint.models = models;
        }
        endpoint.offer_default_first();
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
                "authorize_url"
                    | "token_url"
                    | "device_authorization_url"
                    | "client_id"
                    | "client_secret"
                    | "scopes"
                    | "redirect_uri"
                    | "authorize_params"
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
        let client_secret = string(
            table,
            "client_secret",
            &format!("{prefix}.client_secret"),
            path,
        )?
        .cloned();
        let redirect_uri = string(
            table,
            "redirect_uri",
            &format!("{prefix}.redirect_uri"),
            path,
        )?
        .cloned();
        if let Some(uri) = &redirect_uri {
            if redirect_loopback_port(uri).is_none() {
                return Err(reject(format!(
                    "`{prefix}.redirect_uri` must be a loopback URL with a port, such as \
                     \"http://localhost:1455/callback\": \"{uri}\""
                )));
            }
        }
        let authorize_params = match table.get("authorize_params") {
            None => Vec::new(),
            Some(value) => as_table(value, &format!("{prefix}.authorize_params"), path)?
                .iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| (key.clone(), value.to_owned()))
                        .ok_or_else(|| {
                            reject(format!(
                                "`{prefix}.authorize_params.{key}` must be a string"
                            ))
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
            client_secret,
            scopes,
            redirect_uri,
            authorize_params,
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

/// `models = [...]` as a list of distinct, non-empty names, in the order given.
fn model_list(value: &toml::Value, key: &str, path: &Path) -> Result<Vec<String>, ConfigError> {
    let reject = |message: String| ConfigError {
        path: path.to_path_buf(),
        message,
    };
    let listed = value
        .as_array()
        .ok_or_else(|| reject(format!("`{key}` must be an array of model names")))?;
    let mut models = Vec::with_capacity(listed.len());
    for entry in listed {
        let name = entry
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| reject(format!("`{key}` must hold non-empty model names")))?;
        if !models.iter().any(|existing| existing == name) {
            models.push(name.to_owned());
        }
    }
    Ok(models)
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
    /// One endpoint speaks to one host, and a host serves more than one model,
    /// so the models are a list on the endpoint rather than a second endpoint
    /// duplicating its URL and credential.
    #[test]
    fn an_endpoint_offers_every_model_it_lists_with_its_default_first() {
        let directory = tempfile::tempdir().unwrap();
        let read = |body: &str| {
            let path = write(directory.path(), "config.toml", body);
            Config::load(&[(Layer::User, path)])
        };

        // A file written before `models` existed still reads, and offers the
        // one model it names.
        let config = read(
            "schema_version = 1\n[provider.endpoint.a]\nkind = \"openai\"\nbase_url = \
             \"https://a.test\"\nmodel = \"one\"\n",
        )
        .unwrap();
        let endpoint = config.endpoint(Some("a")).unwrap();
        assert_eq!(endpoint.model.as_deref(), Some("one"));
        assert_eq!(endpoint.models, vec!["one".to_owned()]);

        // The default leads the list, and is not repeated in it.
        let config = read(
            "schema_version = 1\n[provider.endpoint.a]\nkind = \"openai\"\nbase_url = \
             \"https://a.test\"\nmodel = \"two\"\nmodels = [\"one\", \"two\", \
             \"three\", \"one\"]\n",
        )
        .unwrap();
        let endpoint = config.endpoint(Some("a")).unwrap();
        assert_eq!(
            endpoint.models,
            vec!["two".to_owned(), "one".to_owned(), "three".to_owned()],
            "the default leads, duplicates are dropped, order is otherwise kept"
        );

        // Listing models without naming a default offers them in order.
        let config = read(
            "schema_version = 1\n[provider.endpoint.a]\nkind = \"openai\"\nbase_url = \
             \"https://a.test\"\nmodels = [\"one\", \"two\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.endpoint(Some("a")).unwrap().models,
            vec!["one".to_owned(), "two".to_owned()]
        );

        // A shape that is not a list of names is refused rather than ignored.
        for bad in ["\"one\"", "[1, 2]", "[\"\"]"] {
            let error = read(&format!(
                "schema_version = 1\n[provider.endpoint.a]\nkind = \"openai\"\nbase_url = \
                 \"https://a.test\"\nmodels = {bad}\n"
            ))
            .unwrap_err();
            assert!(
                format!("{error}").contains("models"),
                "{bad} was accepted: {error}"
            );
        }
    }

    #[test]
    fn theme_carries_a_base_and_well_formed_role_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let read = |body: &str| {
            let path = write(directory.path(), "config.toml", body);
            Config::load(&[(Layer::User, path)])
        };

        // No `[theme]` at all is the empty theme, not an error.
        assert_eq!(
            read("schema_version = 1\n").unwrap().theme(),
            &Theme::default()
        );

        let config = read(
            "schema_version = 1\n[theme]\nbase = \"light\"\naccent = \"#12ab34\"\ninput_bg = \"445566\"\n",
        )
        .unwrap();
        assert_eq!(config.theme().base.as_deref(), Some("light"));
        assert_eq!(
            config.theme().roles.get("accent").map(String::as_str),
            Some("#12ab34")
        );
        assert_eq!(
            config.theme().roles.get("input_bg").map(String::as_str),
            Some("445566")
        );

        // A colour that is not #rrggbb is refused rather than carried.
        let error = read("schema_version = 1\n[theme]\naccent = \"reddish\"\n").unwrap_err();
        assert!(
            error.message.contains("#rrggbb"),
            "unexpected: {}",
            error.message
        );
    }

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
        let trace = config.explain(Some("provider.endpoint.proxy"));
        assert_eq!(
            trace["values"]["provider.endpoint.proxy.base_url"]["layer"],
            "user"
        );
        assert_eq!(
            trace["values"]["provider.endpoint.proxy.base_url"]["path"],
            serde_json::json!(user)
        );
    }

    /// The source trace exists to name the file a value came from, so a later
    /// layer that merely mentions an endpoint must not take credit for keys it
    /// never set.
    /// A rejected credential must not be quoted back: this is the key an
    /// operator is most likely to paste a real secret into, and a diagnostic
    /// reaches stdout and CI logs with no redaction in front of it.
    #[test]
    fn a_rejected_credential_is_never_echoed() {
        let directory = tempfile::tempdir().unwrap();
        let secret = "sk-not-a-handle-0123456789";
        let path = write(
            directory.path(),
            "user.toml",
            &format!(
                "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\ncredential = \"{secret}\"\n"
            ),
        );

        let error = Config::load(&[(Layer::User, path)]).unwrap_err();

        assert!(
            !error.message.contains(secret),
            "the diagnostic leaked the value: {}",
            error.message
        );
        assert!(error.message.contains("arsy auth set p"));
    }

    #[test]
    fn a_layer_only_claims_the_keys_it_set() {
        let directory = tempfile::tempdir().unwrap();
        let enterprise = write(
            directory.path(),
            "enterprise.toml",
            r#"
schema_version = 1
[provider.endpoint.p]
kind = "openai"
base_url = "https://enterprise.test/v1"
"#,
        );
        let user = write(
            directory.path(),
            "user.toml",
            r#"
schema_version = 1
[provider.endpoint.p]
api_key_env = "K"
"#,
        );

        let config = load(&[(Layer::Enterprise, enterprise), (Layer::User, user)]);
        let trace = config.explain(Some("provider.endpoint.p"));

        assert_eq!(
            trace["values"]["provider.endpoint.p.base_url"]["layer"], "enterprise",
            "the user layer never mentioned base_url"
        );
        assert_eq!(
            trace["values"]["provider.endpoint.p.kind"]["layer"], "enterprise",
            "nor the dialect it inherited"
        );
        assert_eq!(
            trace["values"]["provider.endpoint.p.api_key_env"]["layer"],
            "user"
        );
        assert_eq!(
            config.endpoint(None).unwrap().base_url,
            "https://enterprise.test/v1"
        );
    }

    /// A base URL only means an API once a dialect is fixed, so inheriting one
    /// across a change of dialect would point the new adapter at the old API.
    #[test]
    fn changing_the_dialect_drops_a_base_url_inherited_from_the_old_one() {
        let directory = tempfile::tempdir().unwrap();
        let enterprise = write(
            directory.path(),
            "enterprise.toml",
            "schema_version = 1\n[provider.endpoint.p]\nkind = \"openai\"\n",
        );
        let user = write(
            directory.path(),
            "user.toml",
            "schema_version = 1\n[provider.endpoint.p]\nkind = \"anthropic\"\n",
        );

        let config = load(&[(Layer::Enterprise, enterprise), (Layer::User, user)]);
        let endpoint = config.endpoint(None).unwrap();

        assert_eq!(endpoint.kind, Dialect::Anthropic);
        assert_eq!(endpoint.base_url, Dialect::Anthropic.default_base_url());
        assert_eq!(
            config.explain(Some("provider.endpoint.p.base_url"))["values"]
                ["provider.endpoint.p.base_url"]["layer"],
            "user",
            "the layer that changed the dialect is the one the new default came from"
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
                "must be a handle such as \"secret://os/p\"",
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
client_secret = "not-really-secret"
scopes = ["offline_access"]
redirect_uri = "http://localhost:1455/auth/callback"
[provider.endpoint.local.oauth.authorize_params]
access_type = "offline"
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
        let oauth = endpoint.oauth.as_ref().unwrap();
        assert_eq!(oauth.client_id, "arsy");
        assert_eq!(oauth.client_secret.as_deref(), Some("not-really-secret"));
        assert_eq!(oauth.scopes, ["offline_access"]);
        assert_eq!(
            oauth.redirect_uri.as_deref(),
            Some("http://localhost:1455/auth/callback")
        );
        assert_eq!(
            oauth.authorize_params,
            [("access_type".to_owned(), "offline".to_owned())]
        );
        assert!(oauth.device_authorization_url.is_none());
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

//! Provider and model picker projections.
use super::*;
/// Which provider serves a turn, and with which model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRoute {
    /// A configured provider endpoint, or [`CODEX_PROVIDER`] for the
    /// subprocess fallback.
    pub provider: String,
    pub model: String,
}

/// The provider id that means "hand the turn to the logged-in Codex CLI".
pub const CODEX_PROVIDER: &str = "codex";

impl ModelRoute {
    /// Whether this turn goes to the Codex CLI rather than to a provider ARSY
    /// talks to itself.
    pub fn is_codex(&self) -> bool {
        self.provider == CODEX_PROVIDER
    }

    /// `provider/model`, the form remembered between sessions. A bare model
    /// name is a file written before routes named a provider, and meant Codex.
    pub fn parse(raw: &str) -> Self {
        match raw.split_once('/') {
            Some((provider, model)) => Self {
                provider: provider.to_owned(),
                model: model.to_owned(),
            },
            None => Self {
                provider: CODEX_PROVIDER.to_owned(),
                model: raw.to_owned(),
            },
        }
    }
}

impl fmt::Display for ModelRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model)
    }
}

pub fn detect_model_route() -> Option<ModelRoute> {
    Command::new("codex")
        .args(["login", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        .then(|| ModelRoute {
            provider: CODEX_PROVIDER.to_owned(),
            model: "default".to_owned(),
        })
}

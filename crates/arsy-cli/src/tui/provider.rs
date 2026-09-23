//! Provider and model picker projections.
use super::*;
/// Which provider serves a turn, and with which model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRoute {
    pub provider: String,
    pub model: String,
}

impl ModelRoute {
    /// `provider/model`, the form remembered between sessions.
    pub fn parse(raw: &str) -> Option<Self> {
        let (provider, model) = raw.split_once('/')?;
        Some(Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
        })
    }
}

impl fmt::Display for ModelRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model)
    }
}

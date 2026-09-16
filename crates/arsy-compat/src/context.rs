//! What a live read needs to know about where it runs.

use crate::CompatHomes;
use arsy_kernel::capability::PolicySource;
use std::path::Path;

pub struct Context<'a> {
    pub homes: &'a CompatHomes,
    pub root: &'a Path,
    /// Whether the operator vouched for `root`.
    pub trusted: bool,
    /// `compat.claude.enabled` and `compat.codex.enabled`.
    pub claude: bool,
    pub codex: bool,
    /// The process environment, injected so a test never reads the real one.
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

/// Whose file a declaration came from, which decides the authority it carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Scope {
    /// The operator's own, in their home.
    User,
    /// The repository's, in the checkout.
    Workspace,
}

impl Scope {
    pub(crate) const fn trust(self) -> PolicySource {
        match self {
            Self::User => PolicySource::User,
            Self::Workspace => PolicySource::Workspace,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Workspace => "workspace",
        }
    }
}

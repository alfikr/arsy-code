//! The capability vocabulary every effect is expressed in, and the rules for
//! narrowing a grant when it is delegated.
//!
//! Actions are a closed enum rather than free strings, so a vocabulary this
//! build does not know fails to decode instead of being waved through.

use crate::domain::{GrantId, Principal, ResourceRef};
use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Everything an operation can ask to do.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum CapabilityAction {
    #[serde(rename = "fs.read")]
    FsRead,
    #[serde(rename = "fs.write")]
    FsWrite,
    #[serde(rename = "fs.delete")]
    FsDelete,
    #[serde(rename = "process.exec")]
    ProcessExec,
    #[serde(rename = "process.signal")]
    ProcessSignal,
    #[serde(rename = "network.connect")]
    NetworkConnect,
    #[serde(rename = "git.read")]
    GitRead,
    #[serde(rename = "git.write")]
    GitWrite,
    #[serde(rename = "credential.use")]
    CredentialUse,
    #[serde(rename = "browser.control")]
    BrowserControl,
    #[serde(rename = "debug.launch")]
    DebugLaunch,
    #[serde(rename = "debug.attach")]
    DebugAttach,
    #[serde(rename = "remote.exec")]
    RemoteExec,
    #[serde(rename = "system.modify")]
    SystemModify,
    #[serde(rename = "plugin.invoke")]
    PluginInvoke,
}

impl CapabilityAction {
    /// Every action, so a caller that must cover the vocabulary — synthesizing
    /// one default rule per action, for instance — cannot silently miss one
    /// when the enum grows.
    pub const ALL: &'static [Self] = &[
        Self::FsRead,
        Self::FsWrite,
        Self::FsDelete,
        Self::ProcessExec,
        Self::ProcessSignal,
        Self::NetworkConnect,
        Self::GitRead,
        Self::GitWrite,
        Self::CredentialUse,
        Self::BrowserControl,
        Self::DebugLaunch,
        Self::DebugAttach,
        Self::RemoteExec,
        Self::SystemModify,
        Self::PluginInvoke,
    ];

    /// The resource scheme this action is written against.
    ///
    /// One mapping, because a rule's pattern, a request's resource, and a
    /// dry run's assumed resource all have to agree: an action matched against
    /// `file:**` in one place and `fs:**` in another is a rule that silently
    /// stops applying.
    pub const fn default_scheme(self) -> &'static str {
        match self {
            Self::FsRead | Self::FsWrite | Self::FsDelete | Self::GitRead | Self::GitWrite => {
                "file"
            }
            Self::ProcessExec | Self::ProcessSignal => "process",
            Self::NetworkConnect => "host",
            Self::CredentialUse => "secret",
            Self::BrowserControl => "browser",
            Self::DebugLaunch | Self::DebugAttach => "debug",
            Self::RemoteExec => "remote",
            Self::SystemModify => "system",
            Self::PluginInvoke => "plugin",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::FsDelete => "fs.delete",
            Self::ProcessExec => "process.exec",
            Self::ProcessSignal => "process.signal",
            Self::NetworkConnect => "network.connect",
            Self::GitRead => "git.read",
            Self::GitWrite => "git.write",
            Self::CredentialUse => "credential.use",
            Self::BrowserControl => "browser.control",
            Self::DebugLaunch => "debug.launch",
            Self::DebugAttach => "debug.attach",
            Self::RemoteExec => "remote.exec",
            Self::SystemModify => "system.modify",
            Self::PluginInvoke => "plugin.invoke",
        }
    }
}

impl fmt::Display for CapabilityAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Compile-time bounds on a permission glob. A pattern on the security
/// boundary that takes unbounded time or memory to compile is a denial of
/// service, and no legitimate rule needs more than this.
pub const MAX_GLOB_BYTES: usize = 1024;
pub const MAX_GLOB_METACHARACTERS: usize = 32;

/// A scheme plus a glob over canonical resource values.
#[derive(Clone, Debug, Serialize)]
pub struct ResourcePattern {
    scheme: String,
    glob: String,
    #[serde(skip)]
    matcher: GlobMatcher,
}

impl ResourcePattern {
    /// `*` deliberately stops at a separator and `**` crosses one, so
    /// `file:/repo/*` cannot reach `/repo/nested/secret`.
    pub fn new(scheme: impl Into<String>, glob: impl Into<String>) -> Result<Self, PatternError> {
        let scheme = scheme.into();
        let glob = glob.into();
        if scheme.is_empty() {
            return Err(PatternError::EmptyScheme);
        }
        if glob.len() > MAX_GLOB_BYTES {
            return Err(PatternError::TooLong {
                len: glob.len(),
                max: MAX_GLOB_BYTES,
            });
        }
        let metacharacters = glob
            .chars()
            .filter(|c| matches!(c, '*' | '?' | '[' | '{'))
            .count();
        if metacharacters > MAX_GLOB_METACHARACTERS {
            return Err(PatternError::TooComplex {
                metacharacters,
                max: MAX_GLOB_METACHARACTERS,
            });
        }
        let matcher = GlobBuilder::new(&glob)
            .literal_separator(true)
            .build()
            .map_err(|error| PatternError::InvalidGlob(error.to_string()))?
            .compile_matcher();
        Ok(Self {
            scheme,
            glob,
            matcher,
        })
    }

    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    pub fn glob(&self) -> &str {
        &self.glob
    }

    pub fn matches(&self, resource: &ResourceRef) -> bool {
        resource.scheme() == self.scheme && self.matcher.is_match(resource.value())
    }
}

impl fmt::Display for ResourcePattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.scheme, self.glob)
    }
}

/// Two patterns are the same when they are written the same; a compiled
/// matcher has no meaningful equality of its own.
impl PartialEq for ResourcePattern {
    fn eq(&self, other: &Self) -> bool {
        self.scheme == other.scheme && self.glob == other.glob
    }
}

impl Eq for ResourcePattern {}

#[derive(Deserialize)]
struct ResourcePatternWire {
    scheme: String,
    glob: String,
}

impl TryFrom<ResourcePatternWire> for ResourcePattern {
    type Error = PatternError;

    fn try_from(value: ResourcePatternWire) -> Result<Self, Self::Error> {
        Self::new(value.scheme, value.glob)
    }
}

impl<'de> Deserialize<'de> for ResourcePattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ResourcePatternWire::deserialize(deserializer)?;
        Self::try_from(wire).map_err(serde::de::Error::custom)
    }
}

/// A set of resources, expressed as a conjunction: a resource is in scope only
/// when **every** pattern accepts it.
///
/// ponytail: conjunction rather than glob algebra. Narrowing a scope is then
/// just appending a pattern, and "a child grant is a subset of its parent" is
/// true by construction instead of resting on a glob subset test, which is not
/// decidable in general.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ResourceScope {
    patterns: Vec<ResourcePattern>,
}

impl ResourceScope {
    pub fn new(patterns: Vec<ResourcePattern>) -> Self {
        Self { patterns }
    }

    pub fn single(pattern: ResourcePattern) -> Self {
        Self {
            patterns: vec![pattern],
        }
    }

    pub fn patterns(&self) -> &[ResourcePattern] {
        &self.patterns
    }

    /// An empty scope admits nothing. A grant that names no resource grants no
    /// resource, rather than every one by vacuous truth.
    pub fn admits(&self, resource: &ResourceRef) -> bool {
        !self.patterns.is_empty() && self.patterns.iter().all(|p| p.matches(resource))
    }

    /// Narrow by requiring `other`'s patterns as well.
    pub fn narrow(&self, other: &Self) -> Self {
        let mut patterns = self.patterns.clone();
        for pattern in &other.patterns {
            if !patterns.contains(pattern) {
                patterns.push(pattern.clone());
            }
        }
        Self { patterns }
    }
}

/// One concrete thing an operation needs to do, named before it does it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityRequirement {
    pub action: CapabilityAction,
    pub resource: ResourceRef,
}

impl CapabilityRequirement {
    pub const fn new(action: CapabilityAction, resource: ResourceRef) -> Self {
        Self { action, resource }
    }
}

/// Where a grant's authority came from. Ordered most authoritative first.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    Enterprise,
    User,
    Workspace,
    Session,
}

impl PolicySource {
    /// Only an operator can hand out authority. A repository or a session may
    /// ask for behaviour, never grant it to itself.
    pub const fn may_grant(self) -> bool {
        matches!(self, Self::Enterprise | Self::User)
    }
}

impl fmt::Display for PolicySource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Enterprise => "enterprise",
            Self::User => "user",
            Self::Workspace => "workspace",
            Self::Session => "session",
        })
    }
}

/// Authority to perform one action over one scope, held by one actor.
///
/// ponytail: constraints are expiry and delegation depth only. The spec also
/// allows valued bounds, but no operation reads one yet; add `max_bytes` and
/// friends when `fs.read` needs them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityGrant {
    pub id: GrantId,
    pub actor: Principal,
    pub action: CapabilityAction,
    pub scope: ResourceScope,
    pub expires_at_ms: Option<u64>,
    /// How many more times this grant may be delegated. Zero is a leaf.
    pub delegation_depth: u32,
    pub source: PolicySource,
}

impl CapabilityGrant {
    pub fn permits(&self, requirement: &CapabilityRequirement, now_ms: u64) -> bool {
        self.action == requirement.action
            && !self.is_expired(now_ms)
            && self.scope.admits(&requirement.resource)
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|expiry| now_ms >= expiry)
    }

    /// Derive a child grant for a delegate.
    ///
    /// The child's scope is this scope narrowed by what was asked for, its
    /// expiry is the earlier of the two, and its delegation depth is strictly
    /// lower. A child can therefore never reach past its parent.
    pub fn attenuate(
        &self,
        delegate: Principal,
        action: CapabilityAction,
        requested: &ResourceScope,
        requested_expiry: Option<u64>,
    ) -> Result<Self, AttenuationError> {
        if self.action != action {
            return Err(AttenuationError::ActionMismatch {
                held: self.action,
                requested: action,
            });
        }
        let delegation_depth = self
            .delegation_depth
            .checked_sub(1)
            .ok_or(AttenuationError::DelegationExhausted)?;

        Ok(Self {
            id: GrantId::new(),
            actor: delegate,
            action,
            scope: self.scope.narrow(requested),
            expires_at_ms: earliest(self.expires_at_ms, requested_expiry),
            delegation_depth,
            source: self.source,
        })
    }
}

fn earliest(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (value, None) | (None, value) => value,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatternError {
    EmptyScheme,
    InvalidGlob(String),
    TooLong { len: usize, max: usize },
    TooComplex { metacharacters: usize, max: usize },
}

impl fmt::Display for PatternError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyScheme => formatter.write_str("resource pattern scheme cannot be empty"),
            Self::InvalidGlob(message) => write!(formatter, "invalid resource glob: {message}"),
            Self::TooLong { len, max } => {
                write!(formatter, "resource glob is {len} bytes, limit is {max}")
            }
            Self::TooComplex {
                metacharacters,
                max,
            } => write!(
                formatter,
                "resource glob has {metacharacters} wildcards, limit is {max}"
            ),
        }
    }
}

impl std::error::Error for PatternError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttenuationError {
    ActionMismatch {
        held: CapabilityAction,
        requested: CapabilityAction,
    },
    DelegationExhausted,
}

impl fmt::Display for AttenuationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActionMismatch { held, requested } => write!(
                formatter,
                "grant covers {held}, delegate asked for {requested}"
            ),
            Self::DelegationExhausted => {
                formatter.write_str("grant has no delegation depth remaining")
            }
        }
    }
}

impl std::error::Error for AttenuationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(value: &str) -> ResourceRef {
        ResourceRef::new("file", value).unwrap()
    }

    fn pattern(glob: &str) -> ResourcePattern {
        ResourcePattern::new("file", glob).unwrap()
    }

    fn grant(scope: ResourceScope, depth: u32) -> CapabilityGrant {
        CapabilityGrant {
            id: GrantId::new(),
            actor: Principal::System,
            action: CapabilityAction::FsRead,
            scope,
            expires_at_ms: None,
            delegation_depth: depth,
            source: PolicySource::User,
        }
    }

    #[test]
    fn a_star_stops_at_a_separator() {
        let scope = ResourceScope::single(pattern("/repo/*"));

        assert!(scope.admits(&resource("/repo/main.rs")));
        assert!(!scope.admits(&resource("/repo/nested/secret.rs")));
        assert!(ResourceScope::single(pattern("/repo/**")).admits(&resource("/repo/a/b.rs")));
    }

    #[test]
    fn a_scope_never_crosses_schemes() {
        let scope = ResourceScope::single(pattern("/repo/**"));
        let elsewhere = ResourceRef::new("artifact", "/repo/main.rs").unwrap();

        assert!(!scope.admits(&elsewhere));
    }

    #[test]
    fn an_empty_scope_admits_nothing() {
        assert!(!ResourceScope::new(Vec::new()).admits(&resource("/repo/main.rs")));
    }

    #[test]
    fn narrowing_can_only_remove_resources() {
        let parent = ResourceScope::single(pattern("/repo/**"));
        let child = parent.narrow(&ResourceScope::single(pattern("/repo/src/**")));

        assert!(child.admits(&resource("/repo/src/main.rs")));
        assert!(!child.admits(&resource("/repo/secrets.env")));
        assert!(parent.admits(&resource("/repo/secrets.env")));
    }

    #[test]
    fn a_delegate_asking_wider_still_gets_narrower() {
        let parent = grant(ResourceScope::single(pattern("/repo/src/**")), 2);

        let child = parent
            .attenuate(
                Principal::User("delegate".into()),
                CapabilityAction::FsRead,
                &ResourceScope::single(pattern("/**")),
                None,
            )
            .unwrap();

        assert_eq!(child.delegation_depth, 1);
        assert!(!child.scope.admits(&resource("/etc/passwd")));
        assert!(child.scope.admits(&resource("/repo/src/main.rs")));
    }

    #[test]
    fn delegation_depth_runs_out() {
        let leaf = grant(ResourceScope::single(pattern("/repo/**")), 0);

        assert_eq!(
            leaf.attenuate(
                Principal::System,
                CapabilityAction::FsRead,
                &ResourceScope::single(pattern("/repo/**")),
                None,
            ),
            Err(AttenuationError::DelegationExhausted)
        );
    }

    #[test]
    fn an_expired_grant_permits_nothing() {
        let mut held = grant(ResourceScope::single(pattern("/repo/**")), 1);
        held.expires_at_ms = Some(100);
        let requirement =
            CapabilityRequirement::new(CapabilityAction::FsRead, resource("/repo/main.rs"));

        assert!(held.permits(&requirement, 99));
        assert!(!held.permits(&requirement, 100));
    }

    #[test]
    fn an_unknown_action_fails_to_decode() {
        assert!(serde_json::from_str::<CapabilityAction>("\"fs.exfiltrate\"").is_err());
    }

    #[test]
    fn only_operator_sources_may_grant() {
        assert!(PolicySource::Enterprise.may_grant());
        assert!(PolicySource::User.may_grant());
        assert!(!PolicySource::Workspace.may_grant());
        assert!(!PolicySource::Session.may_grant());
    }
}

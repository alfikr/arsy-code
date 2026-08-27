//! Operations: what an executor promises, what a caller asks for, and the
//! registry that binds the two.
//!
//! Every surface — model tool call, MCP request, slash command, SDK call —
//! decodes into one `OperationRequest`, so authority is decided in one place
//! rather than once per front end.

use crate::{
    capability::{CapabilityAction, CapabilityGrant, CapabilityRequirement},
    domain::{OperationId, Principal, ResourceRef, StateVersion},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt, sync::Arc};

/// A dotted operation identifier, such as `fs.read`.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String")]
pub struct OperationKind(String);

impl OperationKind {
    pub fn new(value: impl Into<String>) -> Result<Self, OperationKindError> {
        let value = value.into();
        let shaped = !value.is_empty()
            && !value.starts_with('.')
            && !value.ends_with('.')
            && value
                .bytes()
                .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_'));
        if shaped {
            Ok(Self(value))
        } else {
            Err(OperationKindError)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for OperationKind {
    type Error = OperationKindError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl fmt::Display for OperationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationKindError;

impl fmt::Display for OperationKindError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("operation kind must match [a-z0-9_]+(\\.[a-z0-9_]+)*")
    }
}

impl std::error::Error for OperationKindError {}

/// Whether repeating an operation is safe.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Idempotency {
    /// Running it again produces the same result and no extra effect.
    Idempotent,
    /// Running it again repeats its effect.
    Effectful,
}

/// How an operation may overlap with others.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyRule {
    Parallel,
    ExclusivePerResource,
    ExclusiveGlobal,
}

/// What an executor publishes about itself.
///
/// ponytail: no `SchemaRef` yet. The spec puts input and output schemas here,
/// but schema generation and the request decoder are a later slice and nothing
/// would read them today.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OperationContract {
    pub kind: OperationKind,
    /// The actions this operation may need. Concrete resources arrive with the
    /// request, not the contract.
    pub actions: Vec<CapabilityAction>,
    pub idempotency: Idempotency,
    pub concurrency: ConcurrencyRule,
}

/// One decoded call, with the effects it will need named up front.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OperationRequest {
    pub id: OperationId,
    pub kind: OperationKind,
    pub actor: Principal,
    pub requirements: Vec<CapabilityRequirement>,
    pub input: serde_json::Value,
}

impl OperationRequest {
    /// A stable fingerprint of everything that decides whether this call is
    /// allowed. An approval is bound to this, so editing the request after the
    /// fact invalidates the answer.
    ///
    /// `id` is excluded: retrying the same call under a new identifier must not
    /// change what was approved. `serde_json` keeps object keys ordered, so the
    /// encoding is stable across runs.
    pub fn digest(&self) -> StateVersion {
        let mut hasher = Sha256::new();
        hasher.update(self.kind.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(serde_json::to_vec(&self.actor).unwrap_or_default());
        hasher.update([0]);
        hasher.update(serde_json::to_vec(&self.requirements).unwrap_or_default());
        hasher.update([0]);
        hasher.update(serde_json::to_vec(&self.input).unwrap_or_default());
        StateVersion::from_digest(hasher.finalize().into())
    }
}

/// A capability that was actually exercised.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Effect {
    pub action: CapabilityAction,
    pub resource: ResourceRef,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OperationOutcome {
    /// The result, stored as an artifact rather than carried inline.
    pub value: Option<ResourceRef>,
    /// What the executor observed itself doing. Authoritative over prediction.
    pub observed_effects: Vec<Effect>,
    pub state: Option<StateVersion>,
}

/// ponytail: synchronous. `process.exec` is what will demand an async
/// signature, and that slice introduces the runtime; the first operations
/// (`fs.read`, `search`) have no reason to carry one.
pub trait OperationExecutor: Send + Sync {
    fn contract(&self) -> &OperationContract;

    fn execute(
        &self,
        request: &OperationRequest,
        grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError>;
}

#[derive(Default)]
pub struct OperationRegistry {
    executors: BTreeMap<OperationKind, Arc<dyn OperationExecutor>>,
}

impl OperationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registration is exclusive: a second executor for the same kind is a bug
    /// in wiring, not a silent override.
    pub fn register(
        &mut self,
        executor: Arc<dyn OperationExecutor>,
    ) -> Result<(), RegistrationError> {
        let kind = executor.contract().kind.clone();
        if self.executors.contains_key(&kind) {
            return Err(RegistrationError::Duplicate(kind));
        }
        self.executors.insert(kind, executor);
        Ok(())
    }

    pub fn contract(&self, kind: &OperationKind) -> Option<&OperationContract> {
        self.executors.get(kind).map(|e| e.contract())
    }

    pub fn kinds(&self) -> impl Iterator<Item = &OperationKind> {
        self.executors.keys()
    }

    /// Dispatch, but only once every named requirement is covered by a grant.
    ///
    /// The policy engine already decided; re-checking here means an executor
    /// can never be reached by a path that skipped that decision.
    pub fn dispatch(
        &self,
        request: &OperationRequest,
        grants: &[CapabilityGrant],
        now_ms: u64,
    ) -> Result<OperationOutcome, OperationError> {
        let executor = self
            .executors
            .get(&request.kind)
            .ok_or_else(|| OperationError::Unregistered(request.kind.clone()))?;

        for requirement in &request.requirements {
            let covered = grants
                .iter()
                .any(|grant| grant.actor == request.actor && grant.permits(requirement, now_ms));
            if !covered {
                return Err(OperationError::Ungranted(requirement.clone()));
            }
        }

        executor.execute(request, grants)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistrationError {
    Duplicate(OperationKind),
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(kind) => write!(formatter, "operation {kind} is already registered"),
        }
    }
}

impl std::error::Error for RegistrationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationError {
    Unregistered(OperationKind),
    Ungranted(CapabilityRequirement),
    Execution(String),
}

impl fmt::Display for OperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unregistered(kind) => write!(formatter, "no executor for operation {kind}"),
            Self::Ungranted(requirement) => write!(
                formatter,
                "no grant covers {} on {}:{}",
                requirement.action,
                requirement.resource.scheme(),
                requirement.resource.value()
            ),
            Self::Execution(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for OperationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capability::{PolicySource, ResourcePattern, ResourceScope},
        domain::GrantId,
    };
    use serde_json::json;

    struct Reader {
        contract: OperationContract,
    }

    impl Reader {
        fn new(kind: &str) -> Arc<Self> {
            Arc::new(Self {
                contract: OperationContract {
                    kind: OperationKind::new(kind).unwrap(),
                    actions: vec![CapabilityAction::FsRead],
                    idempotency: Idempotency::Idempotent,
                    concurrency: ConcurrencyRule::Parallel,
                },
            })
        }
    }

    impl OperationExecutor for Reader {
        fn contract(&self) -> &OperationContract {
            &self.contract
        }

        fn execute(
            &self,
            request: &OperationRequest,
            _grants: &[CapabilityGrant],
        ) -> Result<OperationOutcome, OperationError> {
            Ok(OperationOutcome {
                value: None,
                observed_effects: request
                    .requirements
                    .iter()
                    .map(|requirement| Effect {
                        action: requirement.action,
                        resource: requirement.resource.clone(),
                    })
                    .collect(),
                state: None,
            })
        }
    }

    fn request(value: &str) -> OperationRequest {
        OperationRequest {
            id: OperationId::new(),
            kind: OperationKind::new("fs.read").unwrap(),
            actor: Principal::System,
            requirements: vec![CapabilityRequirement::new(
                CapabilityAction::FsRead,
                ResourceRef::new("file", value).unwrap(),
            )],
            input: json!({ "path": value }),
        }
    }

    fn grant(glob: &str) -> CapabilityGrant {
        CapabilityGrant {
            id: GrantId::new(),
            actor: Principal::System,
            action: CapabilityAction::FsRead,
            scope: ResourceScope::single(ResourcePattern::new("file", glob).unwrap()),
            expires_at_ms: None,
            delegation_depth: 1,
            source: PolicySource::User,
        }
    }

    #[test]
    fn a_kind_is_registered_once() {
        let mut registry = OperationRegistry::new();
        registry.register(Reader::new("fs.read")).unwrap();

        assert_eq!(
            registry.register(Reader::new("fs.read")),
            Err(RegistrationError::Duplicate(
                OperationKind::new("fs.read").unwrap()
            ))
        );
        assert_eq!(registry.kinds().count(), 1);
    }

    #[test]
    fn dispatch_refuses_a_requirement_no_grant_covers() {
        let mut registry = OperationRegistry::new();
        registry.register(Reader::new("fs.read")).unwrap();

        let outcome = registry.dispatch(&request("/repo/src/main.rs"), &[grant("/repo/src/**")], 0);
        assert!(outcome.is_ok());

        assert_eq!(
            registry.dispatch(&request("/etc/passwd"), &[grant("/repo/src/**")], 0),
            Err(OperationError::Ungranted(CapabilityRequirement::new(
                CapabilityAction::FsRead,
                ResourceRef::new("file", "/etc/passwd").unwrap(),
            )))
        );
    }

    #[test]
    fn an_expired_grant_does_not_cover_anything() {
        let mut registry = OperationRegistry::new();
        registry.register(Reader::new("fs.read")).unwrap();
        let mut held = grant("/repo/**");
        held.expires_at_ms = Some(50);

        assert!(registry
            .dispatch(&request("/repo/main.rs"), &[held.clone()], 49)
            .is_ok());
        assert!(registry
            .dispatch(&request("/repo/main.rs"), &[held], 50)
            .is_err());
    }

    #[test]
    fn an_unknown_kind_never_reaches_an_executor() {
        let registry = OperationRegistry::new();
        let mut asked = request("/repo/main.rs");
        asked.kind = OperationKind::new("fs.obliterate").unwrap();

        assert!(matches!(
            registry.dispatch(&asked, &[grant("/**")], 0),
            Err(OperationError::Unregistered(_))
        ));
    }

    #[test]
    fn the_digest_ignores_the_request_identifier_and_nothing_else() {
        let first = request("/repo/main.rs");
        let mut retried = first.clone();
        retried.id = OperationId::new();
        assert_eq!(first.digest(), retried.digest());

        let mut widened = first.clone();
        widened.requirements = vec![CapabilityRequirement::new(
            CapabilityAction::FsRead,
            ResourceRef::new("file", "/etc/passwd").unwrap(),
        )];
        assert_ne!(first.digest(), widened.digest());

        let mut edited = first.clone();
        edited.input = json!({ "path": "/repo/other.rs" });
        assert_ne!(first.digest(), edited.digest());
    }

    #[test]
    fn a_malformed_kind_is_refused() {
        assert!(OperationKind::new("fs..read").is_err() || OperationKind::new(".read").is_err());
        assert!(OperationKind::new("FsRead").is_err());
        assert!(OperationKind::new("").is_err());
    }
}

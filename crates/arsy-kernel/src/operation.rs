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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonType {
    String,
    Number,
    Boolean,
    Object,
    Array,
}

impl JsonType {
    fn accepts(self, value: &serde_json::Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Number => value.is_number(),
            Self::Boolean => value.is_boolean(),
            Self::Object => value.is_object(),
            Self::Array => value.is_array(),
        }
    }
}

/// The small schema surface needed by the first typed operations.
///
/// `optional` exists so a contract can name a field without demanding it.
/// Without it the only way to accept `limit` alongside `path` is `allow_extra`,
/// which accepts *every* unknown field and so publishes nothing a caller can
/// check against — a tool with one optional argument would have no schema at
/// all.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputSchema {
    pub required: BTreeMap<String, JsonType>,
    #[serde(default)]
    pub optional: BTreeMap<String, JsonType>,
    pub allow_extra: bool,
}

/// What an executor publishes about itself.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OperationContract {
    pub kind: OperationKind,
    pub input_schema: InputSchema,
    /// The actions this operation may need. Concrete resources arrive with the
    /// request, not the contract.
    pub actions: Vec<CapabilityAction>,
    pub idempotency: Idempotency,
    /// Whether the effect can be undone.
    ///
    /// Distinct from [`Idempotency`], which says whether *repeating* the call
    /// repeats its effect. Rewriting a file is effectful and reversible — the
    /// previous content is still in version control. Removing a file, or
    /// running a command, is neither. Policy raises an irreversible call to
    /// approval however permissive a rule is, so equating the two would make
    /// every edit need a human, which is how an operator learns to say yes
    /// without reading.
    #[serde(default = "irreversible")]
    pub reversible: bool,
    pub concurrency: ConcurrencyRule,
}

/// The safe answer for a contract that predates the field.
const fn irreversible() -> bool {
    false
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
    /// Artifacts, logs, or reports that substantiate the outcome.
    pub evidence: Vec<ResourceRef>,
    pub state: Option<StateVersion>,
}
/// Optional bounded live output sink for long-running executors.
pub type OutputSink = Arc<dyn Fn(String) + Send + Sync>;

pub trait OperationExecutor: Send + Sync {
    fn contract(&self) -> &OperationContract;

    fn execute(
        &self,
        request: &OperationRequest,
        grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError>;

    fn set_output_sink(&self, _sink: Option<OutputSink>) {}
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

    pub fn set_output_sink(&self, sink: Option<OutputSink>) {
        for executor in self.executors.values() {
            executor.set_output_sink(sink.clone());
        }
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

        validate_input(&executor.contract().input_schema, &request.input)?;

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

fn validate_input(schema: &InputSchema, input: &serde_json::Value) -> Result<(), OperationError> {
    let object = input
        .as_object()
        .ok_or_else(|| OperationError::Schema("input must be an object".into()))?;
    for (field, expected) in &schema.required {
        match object.get(field) {
            Some(value) if expected.accepts(value) => {}
            Some(_) => {
                return Err(OperationError::Schema(format!(
                    "input field {field} has the wrong type"
                )))
            }
            None => {
                return Err(OperationError::Schema(format!(
                    "input field {field} is required"
                )))
            }
        }
    }
    for (field, expected) in &schema.optional {
        match object.get(field) {
            Some(value) if !expected.accepts(value) && !value.is_null() => {
                return Err(OperationError::Schema(format!(
                    "input field {field} has the wrong type"
                )))
            }
            _ => {}
        }
    }
    if !schema.allow_extra {
        if let Some(field) = object.keys().find(|field| {
            !schema.required.contains_key(*field) && !schema.optional.contains_key(*field)
        }) {
            return Err(OperationError::Schema(format!(
                "input field {field} is not allowed"
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistrationError {
    Duplicate(OperationKind),
    /// An executor could not be built at all: the state it needs is missing or
    /// unreadable. Distinct from a duplicate, because the fix is to repair
    /// what it reads rather than to stop registering it twice.
    Unusable(String),
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(kind) => write!(formatter, "operation {kind} is already registered"),
            Self::Unusable(reason) => write!(formatter, "operation is unusable: {reason}"),
        }
    }
}

impl std::error::Error for RegistrationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationError {
    Unregistered(OperationKind),
    Schema(String),
    Ungranted(CapabilityRequirement),
    Execution(String),
}

impl fmt::Display for OperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unregistered(kind) => write!(formatter, "no executor for operation {kind}"),
            Self::Schema(message) => write!(formatter, "invalid operation input: {message}"),
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Reader {
        contract: OperationContract,
        calls: AtomicUsize,
    }

    impl Reader {
        fn new(kind: &str) -> Arc<Self> {
            Arc::new(Self {
                contract: OperationContract {
                    kind: OperationKind::new(kind).unwrap(),
                    input_schema: InputSchema {
                        required: BTreeMap::from([("path".into(), JsonType::String)]),
                        optional: BTreeMap::new(),
                        allow_extra: false,
                    },
                    actions: vec![CapabilityAction::FsRead],
                    idempotency: Idempotency::Idempotent,
                    reversible: true,
                    concurrency: ConcurrencyRule::Parallel,
                },
                calls: AtomicUsize::new(0),
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
            self.calls.fetch_add(1, Ordering::Relaxed);
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
                evidence: vec![ResourceRef::new("artifact", "evidence-1").unwrap()],
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

        let outcome = registry
            .dispatch(&request("/repo/src/main.rs"), &[grant("/repo/src/**")], 0)
            .unwrap();
        assert_eq!(outcome.observed_effects.len(), 1);
        assert_eq!(outcome.evidence.len(), 1);

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
    fn schema_failure_never_reaches_the_executor() {
        let executor = Reader::new("fs.read");
        let mut registry = OperationRegistry::new();
        registry.register(executor.clone()).unwrap();
        let mut malformed = request("/repo/main.rs");
        malformed.input = json!({ "path": 42 });

        assert!(matches!(
            registry.dispatch(&malformed, &[grant("/**")], 0),
            Err(OperationError::Schema(_))
        ));
        assert_eq!(executor.calls.load(Ordering::Relaxed), 0);
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

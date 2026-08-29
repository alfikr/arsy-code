//! Typed, provenance-preserving context and dependency-safe views.

use crate::domain::{ArtifactId, ContextViewId, FragmentId, ResourceRef, SessionId, TaskId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fmt};

pub const MAX_CONTEXT_FRAGMENTS: usize = 4096;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FragmentKind {
    Instruction,
    Task,
    Evidence,
    ToolState,
    ModelOutput,
    Summary,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FragmentSourceKind {
    System,
    User,
    Tool,
    Repository,
    Model,
    Derived,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum ContextScope {
    Global,
    Session(SessionId),
    Task(TaskId),
    Resource(ResourceRef),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    Trusted,
    Untrusted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct Confidence(u16);

impl Confidence {
    pub fn new(basis_points: u16) -> Result<Self, ContextError> {
        if basis_points > 10_000 {
            return Err(ContextError::InvalidConfidence(basis_points));
        }
        Ok(Self(basis_points))
    }

    pub const fn basis_points(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for Confidence {
    type Error = ContextError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Confidence> for u16 {
    fn from(value: Confidence) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Freshness {
    Current,
    ObservedAtMs(u64),
    Revision(String),
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(try_from = "ContextFragmentWire")]
pub struct ContextFragment {
    pub id: FragmentId,
    pub kind: FragmentKind,
    pub source: ResourceRef,
    source_kind: FragmentSourceKind,
    pub scope: ContextScope,
    pub content: ArtifactId,
    pub tokens: u32,
    authority: Authority,
    pub confidence: Confidence,
    pub freshness: Freshness,
    pub dependencies: Vec<FragmentId>,
}

#[derive(Deserialize, JsonSchema)]
struct ContextFragmentWire {
    id: FragmentId,
    kind: FragmentKind,
    source: ResourceRef,
    source_kind: FragmentSourceKind,
    scope: ContextScope,
    content: ArtifactId,
    tokens: u32,
    authority: Authority,
    confidence: Confidence,
    freshness: Freshness,
    dependencies: Vec<FragmentId>,
}

impl ContextFragment {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: FragmentId,
        kind: FragmentKind,
        source: ResourceRef,
        source_kind: FragmentSourceKind,
        scope: ContextScope,
        content: ArtifactId,
        tokens: u32,
        authority: Authority,
        confidence: Confidence,
        freshness: Freshness,
        dependencies: Vec<FragmentId>,
    ) -> Result<Self, ContextError> {
        if authority == Authority::Trusted
            && (kind == FragmentKind::ModelOutput
                || matches!(
                    source_kind,
                    FragmentSourceKind::Repository | FragmentSourceKind::Model
                ))
        {
            return Err(ContextError::UntrustedSourceClaimedAuthority(
                if kind == FragmentKind::ModelOutput {
                    FragmentSourceKind::Model
                } else {
                    source_kind
                },
            ));
        }
        Ok(Self {
            id,
            kind,
            source,
            source_kind,
            scope,
            content,
            tokens,
            authority,
            confidence,
            freshness,
            dependencies,
        })
    }

    pub const fn source_kind(&self) -> FragmentSourceKind {
        self.source_kind
    }

    pub const fn authority(&self) -> Authority {
        self.authority
    }
}

impl TryFrom<ContextFragmentWire> for ContextFragment {
    type Error = ContextError;

    fn try_from(value: ContextFragmentWire) -> Result<Self, Self::Error> {
        Self::new(
            value.id,
            value.kind,
            value.source,
            value.source_kind,
            value.scope,
            value.content,
            value.tokens,
            value.authority,
            value.confidence,
            value.freshness,
            value.dependencies,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OmissionReason {
    TokenBudget,
    MissingDependency(FragmentId),
    OmittedDependency(FragmentId),
    DependencyCycle,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Omission {
    pub fragment: FragmentId,
    pub reason: OmissionReason,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ContextView {
    pub id: ContextViewId,
    pub fragments: Vec<FragmentId>,
    pub render_order: Vec<FragmentId>,
    pub omissions: Vec<Omission>,
    pub budget: u32,
}

impl ContextView {
    /// Selects in caller-provided priority order while rendering every dependency first.
    pub fn select(
        id: ContextViewId,
        candidates: &[ContextFragment],
        budget: u32,
    ) -> Result<Self, ContextError> {
        if candidates.len() > MAX_CONTEXT_FRAGMENTS {
            return Err(ContextError::TooManyFragments {
                count: candidates.len(),
                max: MAX_CONTEXT_FRAGMENTS,
            });
        }
        let all: HashSet<_> = candidates.iter().map(|fragment| fragment.id).collect();
        if all.len() != candidates.len() {
            return Err(ContextError::DuplicateFragmentId);
        }

        let mut selected = HashSet::new();
        let mut omitted = HashSet::new();
        let mut render_order = Vec::new();
        let mut omissions = Vec::new();
        let mut tokens = 0_u32;

        loop {
            let mut progress = false;
            for fragment in candidates {
                if selected.contains(&fragment.id) || omitted.contains(&fragment.id) {
                    continue;
                }
                if let Some(dependency) = fragment
                    .dependencies
                    .iter()
                    .find(|dependency| !all.contains(dependency))
                {
                    omit(
                        fragment.id,
                        OmissionReason::MissingDependency(*dependency),
                        &mut omitted,
                        &mut omissions,
                    );
                    progress = true;
                } else if let Some(dependency) = fragment
                    .dependencies
                    .iter()
                    .find(|dependency| omitted.contains(dependency))
                {
                    omit(
                        fragment.id,
                        OmissionReason::OmittedDependency(*dependency),
                        &mut omitted,
                        &mut omissions,
                    );
                    progress = true;
                } else if fragment
                    .dependencies
                    .iter()
                    .all(|dependency| selected.contains(dependency))
                {
                    if tokens.saturating_add(fragment.tokens) <= budget {
                        tokens = tokens.saturating_add(fragment.tokens);
                        selected.insert(fragment.id);
                        render_order.push(fragment.id);
                    } else {
                        omit(
                            fragment.id,
                            OmissionReason::TokenBudget,
                            &mut omitted,
                            &mut omissions,
                        );
                    }
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }

        for fragment in candidates {
            if !selected.contains(&fragment.id) && !omitted.contains(&fragment.id) {
                omit(
                    fragment.id,
                    OmissionReason::DependencyCycle,
                    &mut omitted,
                    &mut omissions,
                );
            }
        }

        Ok(Self {
            id,
            fragments: render_order.clone(),
            render_order,
            omissions,
            budget,
        })
    }
}

fn omit(
    fragment: FragmentId,
    reason: OmissionReason,
    omitted: &mut HashSet<FragmentId>,
    omissions: &mut Vec<Omission>,
) {
    omitted.insert(fragment);
    omissions.push(Omission { fragment, reason });
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextError {
    InvalidConfidence(u16),
    UntrustedSourceClaimedAuthority(FragmentSourceKind),
    DuplicateFragmentId,
    TooManyFragments { count: usize, max: usize },
}

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfidence(value) => {
                write!(formatter, "confidence {value} exceeds 10000 basis points")
            }
            Self::UntrustedSourceClaimedAuthority(source) => {
                write!(
                    formatter,
                    "{source:?} content cannot carry trusted authority"
                )
            }
            Self::DuplicateFragmentId => formatter.write_str("fragment ids must be unique"),
            Self::TooManyFragments { count, max } => {
                write!(formatter, "context has {count} fragments; maximum is {max}")
            }
        }
    }
}

impl std::error::Error for ContextError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(id: FragmentId, tokens: u32, dependencies: Vec<FragmentId>) -> ContextFragment {
        ContextFragment::new(
            id,
            FragmentKind::Evidence,
            ResourceRef::new("file", "/repo/src/lib.rs").unwrap(),
            FragmentSourceKind::Repository,
            ContextScope::Global,
            ArtifactId::new(),
            tokens,
            Authority::Untrusted,
            Confidence::new(8_000).unwrap(),
            Freshness::Current,
            dependencies,
        )
        .unwrap()
    }

    #[test]
    fn views_enforce_authority_dependencies_budget_and_audit_omissions() {
        let trusted_json = serde_json::to_value(fragment(FragmentId::new(), 1, vec![])).unwrap();
        let mut trusted_json = trusted_json.as_object().unwrap().clone();
        trusted_json.insert("authority".into(), serde_json::json!("trusted"));
        assert!(serde_json::from_value::<ContextFragment>(trusted_json.into()).is_err());

        let dependency = FragmentId::new();
        let dependent = FragmentId::new();
        let too_large = FragmentId::new();
        let view = ContextView::select(
            ContextViewId::new(),
            &[
                fragment(dependent, 2, vec![dependency]),
                fragment(dependency, 2, vec![]),
                fragment(too_large, 3, vec![]),
            ],
            4,
        )
        .unwrap();

        assert_eq!(view.render_order, [dependency, dependent]);
        assert_eq!(view.fragments, view.render_order);
        assert_eq!(
            view.omissions,
            [Omission {
                fragment: too_large,
                reason: OmissionReason::TokenBudget,
            }]
        );
    }
}

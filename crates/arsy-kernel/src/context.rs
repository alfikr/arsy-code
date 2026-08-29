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
    TrustBoundary,
    Scope,
    Residency,
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

#[derive(Clone, Debug)]
pub struct ContextCandidate<'a> {
    pub fragment: &'a ContextFragment,
    pub residency: &'a str,
    pub relevance: u16,
    pub recency: u16,
    pub novelty: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SelectionPolicy {
    pub scope: ContextScope,
    pub require_trusted: bool,
    pub allowed_residencies: Vec<String>,
    pub budget: u32,
}

/// Eval-tuned configuration. No ranking coefficient is fixed in the selector.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RankingWeights {
    pub dependency: i32,
    pub authority: i32,
    pub relevance: i32,
    pub recency: i32,
    pub confidence: i32,
    pub novelty: i32,
    pub token_cost: i32,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct FragmentScore {
    pub fragment: FragmentId,
    pub score: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ContextView {
    pub id: ContextViewId,
    pub fragments: Vec<FragmentId>,
    pub render_order: Vec<FragmentId>,
    pub omissions: Vec<Omission>,
    pub scores: Vec<FragmentScore>,
    pub budget: u32,
}

impl ContextView {
    /// Applies hard policy filters, then ranks reproducibly and renders dependencies first.
    pub fn select(
        id: ContextViewId,
        candidates: &[ContextCandidate<'_>],
        policy: &SelectionPolicy,
        weights: &RankingWeights,
    ) -> Result<Self, ContextError> {
        if candidates.len() > MAX_CONTEXT_FRAGMENTS {
            return Err(ContextError::TooManyFragments {
                count: candidates.len(),
                max: MAX_CONTEXT_FRAGMENTS,
            });
        }
        if candidates.iter().any(|candidate| {
            candidate.relevance > 10_000 || candidate.recency > 10_000 || candidate.novelty > 10_000
        }) {
            return Err(ContextError::InvalidRankingSignal);
        }
        let all: HashSet<_> = candidates
            .iter()
            .map(|candidate| candidate.fragment.id)
            .collect();
        if all.len() != candidates.len() {
            return Err(ContextError::DuplicateFragmentId);
        }

        let mut selected = HashSet::new();
        let mut omitted = HashSet::new();
        let mut render_order = Vec::new();
        let mut omissions = Vec::new();
        let mut ranked = Vec::new();
        let mut tokens = 0_u32;

        for candidate in candidates {
            let fragment = candidate.fragment;
            let reason = if policy.require_trusted && fragment.authority() != Authority::Trusted {
                Some(OmissionReason::TrustBoundary)
            } else if fragment.scope != ContextScope::Global && fragment.scope != policy.scope {
                Some(OmissionReason::Scope)
            } else if !policy.allowed_residencies.is_empty()
                && !policy
                    .allowed_residencies
                    .iter()
                    .any(|allowed| allowed == candidate.residency)
            {
                Some(OmissionReason::Residency)
            } else if fragment.tokens > policy.budget {
                Some(OmissionReason::TokenBudget)
            } else {
                None
            };
            if let Some(reason) = reason {
                omit(fragment.id, reason, &mut omitted, &mut omissions);
            } else {
                ranked.push((candidate, score(candidate, weights)));
            }
        }
        ranked.sort_by(|(left, left_score), (right, right_score)| {
            right_score
                .cmp(left_score)
                .then_with(|| left.fragment.id.cmp(&right.fragment.id))
        });
        omissions.sort_by_key(|omission| omission.fragment);
        let scores = ranked
            .iter()
            .map(|(candidate, score)| FragmentScore {
                fragment: candidate.fragment.id,
                score: *score,
            })
            .collect();

        loop {
            let mut progress = false;
            for (candidate, _) in &ranked {
                let fragment = candidate.fragment;
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
                    if tokens.saturating_add(fragment.tokens) <= policy.budget {
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

        for (candidate, _) in &ranked {
            let fragment = candidate.fragment;
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
            scores,
            budget: policy.budget,
        })
    }
}

fn score(candidate: &ContextCandidate<'_>, weights: &RankingWeights) -> i64 {
    let fragment = candidate.fragment;
    i64::from(weights.dependency) * i64::try_from(fragment.dependencies.len()).unwrap_or(i64::MAX)
        + i64::from(weights.authority)
            * i64::from(u16::from(fragment.authority() == Authority::Trusted) * 10_000)
        + i64::from(weights.relevance) * i64::from(candidate.relevance)
        + i64::from(weights.recency) * i64::from(candidate.recency)
        + i64::from(weights.confidence) * i64::from(fragment.confidence.basis_points())
        + i64::from(weights.novelty) * i64::from(candidate.novelty)
        - i64::from(weights.token_cost) * i64::from(fragment.tokens)
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
    InvalidRankingSignal,
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
            Self::InvalidRankingSignal => {
                formatter.write_str("ranking signals must not exceed 10000 basis points")
            }
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

    fn fragment(
        id: FragmentId,
        tokens: u32,
        authority: Authority,
        scope: ContextScope,
        dependencies: Vec<FragmentId>,
    ) -> ContextFragment {
        ContextFragment::new(
            id,
            FragmentKind::Evidence,
            ResourceRef::new("file", "/repo/src/lib.rs").unwrap(),
            if authority == Authority::Trusted {
                FragmentSourceKind::System
            } else {
                FragmentSourceKind::Repository
            },
            scope,
            ArtifactId::new(),
            tokens,
            authority,
            Confidence::new(8_000).unwrap(),
            Freshness::Current,
            dependencies,
        )
        .unwrap()
    }

    #[test]
    fn views_filter_then_rank_deterministically_with_a_complete_audit() {
        let trusted_json = serde_json::to_value(fragment(
            FragmentId::new(),
            1,
            Authority::Untrusted,
            ContextScope::Global,
            vec![],
        ))
        .unwrap();
        let mut trusted_json = trusted_json.as_object().unwrap().clone();
        trusted_json.insert("authority".into(), serde_json::json!("trusted"));
        assert!(serde_json::from_value::<ContextFragment>(trusted_json.into()).is_err());

        let task = TaskId::new();
        let dependency = FragmentId::new();
        let dependent = FragmentId::new();
        let wrong_trust = FragmentId::new();
        let wrong_scope = FragmentId::new();
        let wrong_residency = FragmentId::new();
        let too_large = FragmentId::new();
        let fragments = [
            fragment(
                dependent,
                2,
                Authority::Trusted,
                ContextScope::Task(task),
                vec![dependency],
            ),
            fragment(
                dependency,
                2,
                Authority::Trusted,
                ContextScope::Global,
                vec![],
            ),
            fragment(
                wrong_trust,
                1,
                Authority::Untrusted,
                ContextScope::Global,
                vec![],
            ),
            fragment(
                wrong_scope,
                1,
                Authority::Trusted,
                ContextScope::Session(SessionId::new()),
                vec![],
            ),
            fragment(
                wrong_residency,
                1,
                Authority::Trusted,
                ContextScope::Global,
                vec![],
            ),
            fragment(
                too_large,
                5,
                Authority::Trusted,
                ContextScope::Global,
                vec![],
            ),
        ];
        let candidates = [
            ContextCandidate {
                fragment: &fragments[0],
                residency: "id",
                relevance: 9_000,
                recency: 8_000,
                novelty: 7_000,
            },
            ContextCandidate {
                fragment: &fragments[1],
                residency: "id",
                relevance: 1_000,
                recency: 8_000,
                novelty: 7_000,
            },
            ContextCandidate {
                fragment: &fragments[2],
                residency: "id",
                relevance: 10_000,
                recency: 10_000,
                novelty: 10_000,
            },
            ContextCandidate {
                fragment: &fragments[3],
                residency: "id",
                relevance: 10_000,
                recency: 10_000,
                novelty: 10_000,
            },
            ContextCandidate {
                fragment: &fragments[4],
                residency: "us",
                relevance: 10_000,
                recency: 10_000,
                novelty: 10_000,
            },
            ContextCandidate {
                fragment: &fragments[5],
                residency: "id",
                relevance: 10_000,
                recency: 10_000,
                novelty: 10_000,
            },
        ];
        let policy = SelectionPolicy {
            scope: ContextScope::Task(task),
            require_trusted: true,
            allowed_residencies: vec!["id".into()],
            budget: 4,
        };
        let weights = RankingWeights {
            dependency: 100,
            authority: 1,
            relevance: 2,
            recency: 1,
            confidence: 1,
            novelty: 1,
            token_cost: 1,
        };
        let view =
            ContextView::select(ContextViewId::new(), &candidates, &policy, &weights).unwrap();

        assert_eq!(view.render_order, [dependency, dependent]);
        assert_eq!(view.fragments, view.render_order);
        assert_eq!(view.scores.len(), 2);
        assert_eq!(view.scores[0].fragment, dependent);
        let mut expected_omissions = vec![
            Omission {
                fragment: wrong_trust,
                reason: OmissionReason::TrustBoundary,
            },
            Omission {
                fragment: wrong_scope,
                reason: OmissionReason::Scope,
            },
            Omission {
                fragment: wrong_residency,
                reason: OmissionReason::Residency,
            },
            Omission {
                fragment: too_large,
                reason: OmissionReason::TokenBudget,
            },
        ];
        expected_omissions.sort_by_key(|omission| omission.fragment);
        assert_eq!(view.omissions, expected_omissions);

        let reversed = candidates.into_iter().rev().collect::<Vec<_>>();
        let replay =
            ContextView::select(ContextViewId::new(), &reversed, &policy, &weights).unwrap();
        assert_eq!(replay.render_order, view.render_order);
        assert_eq!(replay.scores, view.scores);
        assert_eq!(replay.omissions, view.omissions);
    }
}

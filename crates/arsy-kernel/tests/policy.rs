//! Property tests for the authority core.
//!
//! Two of these are named guarantees in the testing strategy — deny rules are
//! monotonic, and a child grant is a subset of its parent. The rest keep the
//! decision closed and order-independent.

use arsy_kernel::{
    capability::{
        CapabilityAction, CapabilityGrant, CapabilityRequirement, PolicySource, ResourcePattern,
        ResourceScope,
    },
    domain::{GrantId, Principal, ResourceRef, StateVersion},
    operation::OperationKind,
    policy::{
        ActorMatch, PolicyDecision, PolicyQuery, PolicyRule, RiskContext, RuleEffect, RuleSet,
    },
};
use proptest::prelude::*;

/// Globs and resources are drawn from overlapping pools so that generated
/// rules actually bear on generated queries. Random strings would mostly miss.
const GLOBS: &[&str] = &[
    "/**",
    "/repo/**",
    "/repo/src/**",
    "/repo/src/*",
    "/repo/.env",
    "/etc/**",
];

const RESOURCES: &[&str] = &[
    "/repo/src/main.rs",
    "/repo/src/nested/deep.rs",
    "/repo/.env",
    "/repo/README.md",
    "/etc/passwd",
];

const ACTIONS: &[CapabilityAction] = &[
    CapabilityAction::FsRead,
    CapabilityAction::FsWrite,
    CapabilityAction::FsDelete,
];

const SOURCES: &[PolicySource] = &[
    PolicySource::Enterprise,
    PolicySource::User,
    PolicySource::Workspace,
    PolicySource::Session,
];

const EFFECTS: &[RuleEffect] = &[
    RuleEffect::Deny,
    RuleEffect::RequireApproval,
    RuleEffect::Allow,
];

fn pattern(glob: &str) -> ResourcePattern {
    ResourcePattern::new("file", glob).expect("pool globs are valid")
}

fn resource(value: &str) -> ResourceRef {
    ResourceRef::new("file", value).expect("pool resources are valid")
}

fn actor() -> impl Strategy<Value = Principal> {
    prop_oneof![
        Just(Principal::System),
        Just(Principal::User("ada".to_owned())),
        Just(Principal::User("grace".to_owned())),
    ]
}

fn rule() -> impl Strategy<Value = PolicyRule> {
    (
        prop::sample::select(SOURCES),
        prop::sample::select(EFFECTS),
        prop_oneof![Just(ActorMatch::Any), actor().prop_map(ActorMatch::Exactly)],
        prop::sample::select(ACTIONS),
        prop::sample::select(GLOBS),
        prop::option::of(0..1_000_000u64),
        0..4u32,
    )
        .prop_map(
            |(source, effect, actor, action, glob, expires_at_ms, delegation_depth)| PolicyRule {
                source,
                effect,
                actor,
                action,
                pattern: pattern(glob),
                expires_at_ms,
                delegation_depth,
            },
        )
}

fn rules() -> impl Strategy<Value = Vec<PolicyRule>> {
    prop::collection::vec(rule(), 0..12)
}

fn deny_rule() -> impl Strategy<Value = PolicyRule> {
    rule().prop_map(|mut rule| {
        rule.effect = RuleEffect::Deny;
        rule
    })
}

fn query() -> impl Strategy<Value = PolicyQuery> {
    (
        actor(),
        prop::sample::select(ACTIONS),
        prop::sample::select(RESOURCES),
        any::<bool>(),
    )
        .prop_map(|(actor, action, value, reversible)| PolicyQuery {
            actor,
            operation: OperationKind::new("fs.read").expect("a valid kind"),
            requirement: CapabilityRequirement::new(action, resource(value)),
            operation_digest: StateVersion::from_digest([7; 32]),
            resource_version: Some(StateVersion::from_digest([8; 32])),
            context: RiskContext { reversible },
        })
}

fn queries() -> impl Strategy<Value = Vec<PolicyQuery>> {
    prop::collection::vec(query(), 1..8)
}

/// A grant carries a fresh random identifier, so decisions are compared by
/// what they authorize rather than by which grant object was minted.
fn summarize(decision: &PolicyDecision) -> String {
    match decision {
        PolicyDecision::Allow(grant) => format!(
            "allow {} {:?} {:?} {} from {}",
            grant.action,
            grant
                .scope
                .patterns()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            grant.expires_at_ms,
            grant.delegation_depth,
            grant.source
        ),
        PolicyDecision::RequireApproval(request) => format!("approval {}", request.reason),
        PolicyDecision::Deny(reason) => format!("deny {}", reason.message),
    }
}

/// Whether a rule speaks to a query at all. Mirrors what the engine matches on.
fn covers(rule: &PolicyRule, query: &PolicyQuery) -> bool {
    rule.actor.matches(&query.actor)
        && rule.action == query.requirement.action
        && rule.pattern.matches(&query.requirement.resource)
}

fn grant() -> impl Strategy<Value = CapabilityGrant> {
    (
        actor(),
        prop::sample::select(ACTIONS),
        prop::collection::vec(prop::sample::select(GLOBS), 1..3),
        prop::option::of(0..1_000_000u64),
        1..4u32,
        prop::sample::select(SOURCES),
    )
        .prop_map(
            |(actor, action, globs, expires_at_ms, delegation_depth, source)| CapabilityGrant {
                id: GrantId::new(),
                actor,
                action,
                scope: ResourceScope::new(globs.into_iter().map(pattern).collect()),
                expires_at_ms,
                delegation_depth,
                source,
            },
        )
}

proptest! {
    // Pure CPU, so this can afford far more cases than the store properties.
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Adding a deny rule never opens a door that was closed, and a deny rule
    /// that covers the query closes it outright whatever else was said.
    #[test]
    fn deny_rules_are_monotonic(
        base in rules(),
        extra in deny_rule(),
        asked in queries(),
    ) {
        let before = RuleSet::compile(base.clone());
        let after = RuleSet::compile(base.into_iter().chain([extra.clone()]));

        for query in &asked {
            let decision = after.evaluate(query).decision;
            if decision.is_allow() {
                prop_assert!(
                    before.evaluate(query).decision.is_allow(),
                    "a deny rule turned a refusal into an allow"
                );
            }
            if covers(&extra, query) {
                prop_assert!(
                    matches!(decision, PolicyDecision::Deny(_)),
                    "a deny rule that covers the query did not win"
                );
            }
        }
    }

    /// A delegated grant reaches no further than the grant it came from.
    #[test]
    fn a_child_grant_is_a_subset_of_its_parent(
        parent in grant(),
        delegate in actor(),
        requested in prop::collection::vec(prop::sample::select(GLOBS), 0..3),
        requested_expiry in prop::option::of(0..1_000_000u64),
    ) {
        let requested = ResourceScope::new(requested.into_iter().map(pattern).collect());
        let child = parent
            .attenuate(delegate, parent.action, &requested, requested_expiry)
            .expect("depth starts above zero and the action matches");

        for value in RESOURCES {
            let resource = resource(value);
            if child.scope.admits(&resource) {
                prop_assert!(
                    parent.scope.admits(&resource),
                    "child reached {value}, which the parent does not cover"
                );
            }
        }
        prop_assert!(child.delegation_depth < parent.delegation_depth);
        if let Some(expiry) = child.expires_at_ms {
            prop_assert!(parent.expires_at_ms.is_none_or(|parent| expiry <= parent));
            prop_assert!(requested_expiry.is_none_or(|asked| expiry <= asked));
        } else {
            prop_assert!(parent.expires_at_ms.is_none() && requested_expiry.is_none());
        }
    }

    /// The order rules were concatenated in cannot change the answer.
    #[test]
    fn evaluation_does_not_depend_on_rule_order(
        base in rules(),
        shuffle in any::<prop::sample::Index>(),
        asked in queries(),
    ) {
        let mut rotated = base.clone();
        if !rotated.is_empty() {
            let at = shuffle.index(rotated.len());
            rotated.rotate_left(at);
            rotated.reverse();
        }
        let left = RuleSet::compile(base);
        let right = RuleSet::compile(rotated);

        for query in &asked {
            prop_assert_eq!(
                summarize(&left.evaluate(query).decision),
                summarize(&right.evaluate(query).decision)
            );
        }
    }

    /// With nothing said, nothing is permitted.
    #[test]
    fn an_empty_rule_set_refuses_everything(asked in queries()) {
        let empty = RuleSet::default();

        for query in &asked {
            let outcome = empty.evaluate(query);
            prop_assert!(matches!(outcome.decision, PolicyDecision::Deny(_)));
            prop_assert!(outcome.trace.is_empty());
        }
    }

    /// A repository or a session can ask, never grant.
    #[test]
    fn untrusted_sources_cannot_grant_authority(
        base in rules(),
        asked in queries(),
    ) {
        let untrusted: Vec<_> = base
            .into_iter()
            .filter(|rule| !rule.source.may_grant())
            .map(|mut rule| {
                rule.effect = RuleEffect::Allow;
                rule
            })
            .collect();
        let compiled = RuleSet::compile(untrusted.clone());

        prop_assert_eq!(compiled.diagnostics().len(), untrusted.len());
        for query in &asked {
            prop_assert!(!compiled.evaluate(query).decision.is_allow());
        }
    }

    /// Whatever the answer, the rules that bore on it are reported.
    #[test]
    fn every_considered_rule_appears_in_the_trace(
        base in rules(),
        asked in query(),
    ) {
        let compiled = RuleSet::compile(base);
        let outcome = compiled.evaluate(&asked);

        prop_assert_eq!(
            outcome.trace.iter().filter(|entry| !entry.note.starts_with("allow raised")).count(),
            compiled.rules().len()
        );
        if let PolicyDecision::Allow(_) = outcome.decision {
            prop_assert!(outcome.matched().any(|entry| entry.effect == RuleEffect::Allow));
        }
    }
}

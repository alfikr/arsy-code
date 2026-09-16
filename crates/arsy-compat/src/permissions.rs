//! Permissions Claude Code and Codex declare, as ARSY policy rules.
//!
//! A rule is carried over only as precisely as ARSY can enforce it. Where it
//! cannot be — `Bash(git push:*)` names arguments, and ARSY matches a process
//! by its program alone — a deny becomes a request for approval over the whole
//! program and an allow becomes one too, never a wider grant, and a note says
//! so. A repository's rules keep the repository's authority, so its allows are
//! downgraded when the rule set is compiled.

use crate::{Context, Scope};
use arsy_kernel::{
    capability::{CapabilityAction, ResourcePattern},
    config::CompatSeed,
    policy::{ActorMatch, PolicyRule, RuleEffect, SandboxAssurance},
};

/// Every file's rules, the operator's before the repository's.
pub(crate) fn seeds(context: &Context) -> Vec<CompatSeed> {
    let mut seeds = Vec::new();
    if context.claude {
        seeds.extend(crate::claude::permissions::seeds(context));
    }
    if context.codex {
        seeds.extend(crate::codex::permissions::seeds(context));
    }
    seeds
}

/// One rule, or `None` when the glob is not one ARSY accepts.
pub(crate) fn rule(
    scope: Scope,
    effect: RuleEffect,
    action: CapabilityAction,
    glob: &str,
) -> Option<PolicyRule> {
    Some(PolicyRule {
        source: scope.trust(),
        effect,
        actor: ActorMatch::Any,
        action,
        pattern: ResourcePattern::new(action.default_scheme(), glob).ok()?,
        expires_at_ms: None,
        delegation_depth: 0,
        minimum_assurance: SandboxAssurance::None,
    })
}

pub(crate) const fn effect_name(effect: RuleEffect) -> &'static str {
    match effect {
        RuleEffect::Deny => "deny",
        RuleEffect::RequireApproval => "ask",
        RuleEffect::Allow => "allow",
    }
}

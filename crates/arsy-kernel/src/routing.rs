//! Choosing a model, under policy.
//!
//! See `docs/09-model-provider-layer.md`. Routing decides *between* models that
//! policy already allows; it never widens that set. Three rules follow from
//! that, and they are why this is a module rather than a sort call:
//!
//! * **A pin is a preference, not an exemption.** Pinning a model that policy
//!   does not allow is refused, with the ceiling that refused it named.
//! * **Disabling routing does not disable policy.** It falls back to the
//!   configured default, which is filtered exactly as a routed choice is.
//! * **The decision says why.** Selecting a model without saying which
//!   criterion decided it is unreviewable, so every decision carries its
//!   reasons and the candidates it rejected.

use crate::{
    model_profile::{CapabilityState, ModelCapability},
    provider::ModelKey,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// What the turn is for. The harness distinguishes exactly these two today —
/// a person waiting at a terminal, and a pipeline that is not — so those are
/// the classes, rather than a taxonomy nothing produces.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskClass {
    /// Someone is watching the stream: latency decides.
    Interactive,
    /// Nobody is waiting: cost decides.
    Batch,
}

/// One model routing may choose, with whatever is known about it.
///
/// Every measured field is optional because it is measured: a model nothing
/// has been sent to has no latency, and inventing one would make the ranking a
/// fiction. A criterion with no data does not discriminate.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Candidate {
    pub key: ModelKey,
    pub capabilities: BTreeMap<String, ModelCapability>,
    /// Region the endpoint serves from, when configuration states one.
    pub residency: Option<String>,
    /// Micro-units per thousand output tokens, when configuration states it.
    pub cost_micros_per_1k: Option<u64>,
}

impl Candidate {
    fn supports(&self, capability: &str) -> bool {
        self.capabilities
            .get(capability)
            .is_some_and(|value| value.state == CapabilityState::Supported)
    }
}

/// What has actually been observed, per model.
///
/// Rolling means: a model that was slow once and fast fifty times ranks on the
/// fifty, and a model with one sample is not treated as if it had many.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Observations {
    samples: BTreeMap<ModelKey, Sample>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Sample {
    turns: u64,
    failures: u64,
    latency_total_ms: u64,
    cost_total_micros: u64,
}

impl Observations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one finished turn.
    pub fn record(&mut self, key: &ModelKey, latency_ms: u64, cost_micros: u64, succeeded: bool) {
        let sample = self.samples.entry(key.clone()).or_default();
        sample.turns = sample.turns.saturating_add(1);
        if !succeeded {
            sample.failures = sample.failures.saturating_add(1);
        }
        sample.latency_total_ms = sample.latency_total_ms.saturating_add(latency_ms);
        sample.cost_total_micros = sample.cost_total_micros.saturating_add(cost_micros);
    }

    pub fn turns(&self, key: &ModelKey) -> u64 {
        self.samples.get(key).map_or(0, |sample| sample.turns)
    }

    /// Mean latency, or `None` when nothing has been measured.
    pub fn mean_latency_ms(&self, key: &ModelKey) -> Option<u64> {
        let sample = self.samples.get(key)?;
        (sample.turns > 0).then(|| sample.latency_total_ms / sample.turns)
    }

    pub fn mean_cost_micros(&self, key: &ModelKey) -> Option<u64> {
        let sample = self.samples.get(key)?;
        (sample.turns > 0).then(|| sample.cost_total_micros / sample.turns)
    }

    /// Failures per thousand turns. Higher is worse; `None` without samples.
    pub fn failure_rate(&self, key: &ModelKey) -> Option<u64> {
        let sample = self.samples.get(key)?;
        (sample.turns > 0).then(|| sample.failures.saturating_mul(1_000) / sample.turns)
    }
}

/// The policy the choice happens inside.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Constraints {
    /// Providers policy allows. Empty means no ceiling was set.
    pub allowed_providers: BTreeSet<String>,
    /// Models policy allows. Empty means no ceiling was set.
    pub allowed_models: BTreeSet<String>,
    /// Regions policy allows. Empty means no ceiling was set.
    pub residency: BTreeSet<String>,
    /// Capabilities the turn cannot proceed without.
    pub required_capabilities: BTreeSet<String>,
    /// Cap on the mean cost of a turn.
    pub max_cost_micros: Option<u64>,
}

impl Constraints {
    /// Why this candidate is not eligible, or `None` when it is.
    fn rejects(&self, candidate: &Candidate) -> Option<String> {
        if !self.allowed_providers.is_empty()
            && !self.allowed_providers.contains(&candidate.key.provider)
        {
            return Some("provider.allowed does not include it".to_owned());
        }
        if !self.allowed_models.is_empty() && !self.allowed_models.contains(&candidate.key.model) {
            return Some("model.allowed does not include it".to_owned());
        }
        if !self.residency.is_empty() {
            match &candidate.residency {
                // An unstated region cannot be shown to satisfy a residency
                // ceiling, so it does not.
                None => return Some("its residency is unstated".to_owned()),
                Some(region) if !self.residency.contains(region) => {
                    return Some(format!("its region `{region}` is not allowed"))
                }
                Some(_) => {}
            }
        }
        for capability in &self.required_capabilities {
            if !candidate.supports(capability) {
                return Some(format!("it does not support `{capability}`"));
            }
        }
        if let (Some(cap), Some(cost)) = (self.max_cost_micros, candidate.cost_micros_per_1k) {
            if cost > cap {
                return Some(format!("its cost {cost} exceeds the {cap} cap"));
            }
        }
        None
    }
}

/// A candidate that did not survive the filter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Excluded {
    pub key: ModelKey,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Decision {
    Routed {
        key: ModelKey,
        /// In the order they were applied.
        reasons: Vec<String>,
        excluded: Vec<Excluded>,
    },
    /// Routing was off, or a pin was honoured. Still policy-filtered.
    Chosen {
        key: ModelKey,
        why: &'static str,
        excluded: Vec<Excluded>,
    },
    Refused {
        reason: String,
        excluded: Vec<Excluded>,
    },
}

impl Decision {
    pub fn key(&self) -> Option<&ModelKey> {
        match self {
            Self::Routed { key, .. } | Self::Chosen { key, .. } => Some(key),
            Self::Refused { .. } => None,
        }
    }

    pub fn excluded(&self) -> &[Excluded] {
        match self {
            Self::Routed { excluded, .. }
            | Self::Chosen { excluded, .. }
            | Self::Refused { excluded, .. } => excluded,
        }
    }
}

/// What the operator asked for, beside what policy allows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preference {
    /// A model the operator pinned. Filtered like any other.
    pub pinned: Option<ModelKey>,
    /// The configured default, used when routing is off.
    pub default: Option<ModelKey>,
    /// `false` disables ranking, not policy.
    pub route: bool,
    pub task: Option<TaskClass>,
}

/// Decide which model a turn uses.
///
/// The filter runs first and identically for every path, so no preference can
/// reach a model policy excluded. Ranking is deterministic: equal candidates
/// are broken by provider then model, never by iteration order.
pub fn decide(
    candidates: &[Candidate],
    constraints: &Constraints,
    observations: &Observations,
    preference: &Preference,
) -> Decision {
    let mut eligible = Vec::new();
    let mut excluded = Vec::new();
    for candidate in candidates {
        match constraints.rejects(candidate) {
            Some(reason) => excluded.push(Excluded {
                key: candidate.key.clone(),
                reason,
            }),
            None => eligible.push(candidate),
        }
    }

    if let Some(pinned) = &preference.pinned {
        return match eligible.iter().find(|candidate| candidate.key == *pinned) {
            Some(candidate) => Decision::Chosen {
                key: candidate.key.clone(),
                why: "pinned by the operator",
                excluded,
            },
            None => {
                let reason = excluded
                    .iter()
                    .find(|entry| entry.key == *pinned)
                    .map_or_else(
                        || format!("`{}/{}` is not configured", pinned.provider, pinned.model),
                        |entry| {
                            format!(
                                "`{}/{}` is pinned but {}",
                                pinned.provider, pinned.model, entry.reason
                            )
                        },
                    );
                Decision::Refused { reason, excluded }
            }
        };
    }

    if !preference.route {
        let Some(default) = &preference.default else {
            return Decision::Refused {
                reason: "routing is disabled and no default model is configured".to_owned(),
                excluded,
            };
        };
        return match eligible.iter().find(|candidate| candidate.key == *default) {
            Some(candidate) => Decision::Chosen {
                key: candidate.key.clone(),
                why: "the configured default, with routing disabled",
                excluded,
            },
            None => Decision::Refused {
                reason: format!(
                    "routing is disabled and the default `{}/{}` is not allowed",
                    default.provider, default.model
                ),
                excluded,
            },
        };
    }

    if eligible.is_empty() {
        return Decision::Refused {
            reason: "no configured model satisfies the resolved policy".to_owned(),
            excluded,
        };
    }

    let task = preference.task.unwrap_or(TaskClass::Interactive);
    let mut ranked: Vec<&Candidate> = eligible;
    ranked.sort_by(|left, right| {
        rank(left, observations, task)
            .cmp(&rank(right, observations, task))
            .then(left.key.provider.cmp(&right.key.provider))
            .then(left.key.model.cmp(&right.key.model))
    });
    let winner = ranked[0];
    Decision::Routed {
        key: winner.key.clone(),
        reasons: explain(winner, observations, task),
        excluded,
    }
}

/// The sort key. Lower is better, and every component is `Option`-shaped so a
/// criterion with no measurement neither helps nor hurts.
///
/// Reliability leads: a cheap model that fails is not cheap. Then the class's
/// own priority, then the other one, then a stable fallback.
fn rank(
    candidate: &Candidate,
    observations: &Observations,
    task: TaskClass,
) -> (u64, u64, u64, usize) {
    let failures = observations.failure_rate(&candidate.key).unwrap_or(0);
    let latency = observations
        .mean_latency_ms(&candidate.key)
        .unwrap_or(u64::MAX);
    let cost = observations
        .mean_cost_micros(&candidate.key)
        .or(candidate.cost_micros_per_1k)
        .unwrap_or(u64::MAX);
    let (first, second) = match task {
        TaskClass::Interactive => (latency, cost),
        TaskClass::Batch => (cost, latency),
    };
    // A model with no measurements at all sorts behind one with any, rather
    // than winning on a `u64::MAX` that happens to tie.
    let unmeasured = usize::from(observations.turns(&candidate.key) == 0);
    (failures, first, second, unmeasured)
}

fn explain(candidate: &Candidate, observations: &Observations, task: TaskClass) -> Vec<String> {
    let mut reasons = vec![format!(
        "ranked for a {} turn",
        match task {
            TaskClass::Interactive => "latency-sensitive",
            TaskClass::Batch => "cost-sensitive",
        }
    )];
    match observations.turns(&candidate.key) {
        0 => reasons.push("nothing has been measured for it yet".to_owned()),
        turns => {
            reasons.push(format!("measured over {turns} turn(s)"));
            if let Some(latency) = observations.mean_latency_ms(&candidate.key) {
                reasons.push(format!("mean latency {latency} ms"));
            }
            if let Some(cost) = observations.mean_cost_micros(&candidate.key) {
                reasons.push(format!("mean cost {cost} micro-units"));
            }
            if let Some(rate) = observations.failure_rate(&candidate.key) {
                reasons.push(format!("{rate} failures per thousand turns"));
            }
        }
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_profile;

    fn candidate(provider: &str, model: &str) -> Candidate {
        Candidate {
            key: ModelKey {
                provider: provider.to_owned(),
                model: model.to_owned(),
            },
            capabilities: model_profile::declared(None),
            residency: None,
            cost_micros_per_1k: None,
        }
    }

    fn routed(decision: &Decision) -> &ModelKey {
        decision.key().expect("a model was chosen")
    }

    #[test]
    fn a_pin_is_filtered_like_anything_else() {
        let candidates = [candidate("acme", "fast"), candidate("other", "slow")];
        let pinned = candidates[1].key.clone();
        let constraints = Constraints {
            allowed_providers: ["acme".to_owned()].into(),
            ..Constraints::default()
        };
        let decision = decide(
            &candidates,
            &constraints,
            &Observations::new(),
            &Preference {
                pinned: Some(pinned),
                route: true,
                ..Preference::default()
            },
        );
        assert!(
            matches!(&decision, Decision::Refused { reason, .. }
                if reason.contains("provider.allowed")),
            "{decision:?}"
        );

        // Pinning something the ceiling allows is honoured, and says so.
        let decision = decide(
            &candidates,
            &constraints,
            &Observations::new(),
            &Preference {
                pinned: Some(candidates[0].key.clone()),
                route: true,
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision).model, "fast");
        assert!(matches!(decision, Decision::Chosen { why, .. } if why.contains("pinned")));
    }

    #[test]
    fn disabling_routing_still_applies_policy() {
        let candidates = [candidate("acme", "fast"), candidate("other", "slow")];
        let constraints = Constraints {
            allowed_models: ["fast".to_owned()].into(),
            ..Constraints::default()
        };
        let decision = decide(
            &candidates,
            &constraints,
            &Observations::new(),
            &Preference {
                default: Some(candidates[1].key.clone()),
                route: false,
                ..Preference::default()
            },
        );
        assert!(
            matches!(&decision, Decision::Refused { reason, .. } if reason.contains("not allowed")),
            "{decision:?}"
        );
        assert_eq!(decision.excluded().len(), 1);
        assert!(decision.excluded()[0].reason.contains("model.allowed"));

        let decision = decide(
            &candidates,
            &constraints,
            &Observations::new(),
            &Preference {
                default: Some(candidates[0].key.clone()),
                route: false,
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision).model, "fast");
    }

    #[test]
    fn residency_and_capability_ceilings_exclude_with_a_reason() {
        let mut regional = candidate("acme", "eu");
        regional.residency = Some("eu".to_owned());
        let mut elsewhere = candidate("acme", "us");
        elsewhere.residency = Some("us".to_owned());
        let unstated = candidate("acme", "unknown");
        let mut incapable = candidate("acme", "plain");
        incapable.residency = Some("eu".to_owned());
        incapable.capabilities.remove("tool_calls");

        let decision = decide(
            &[
                regional.clone(),
                elsewhere.clone(),
                unstated.clone(),
                incapable.clone(),
            ],
            &Constraints {
                residency: ["eu".to_owned()].into(),
                required_capabilities: ["tool_calls".to_owned()].into(),
                ..Constraints::default()
            },
            &Observations::new(),
            &Preference {
                route: true,
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision).model, "eu");
        let excluded: BTreeMap<_, _> = decision
            .excluded()
            .iter()
            .map(|entry| (entry.key.model.clone(), entry.reason.clone()))
            .collect();
        assert!(excluded["us"].contains("not allowed"));
        assert!(excluded["unknown"].contains("unstated"));
        assert!(excluded["plain"].contains("tool_calls"));
    }

    #[test]
    fn measurement_decides_and_the_decision_says_which() {
        let candidates = [candidate("acme", "quick"), candidate("acme", "cheap")];
        let mut observations = Observations::new();
        for _ in 0..10 {
            observations.record(&candidates[0].key, 200, 900, true);
            observations.record(&candidates[1].key, 2_000, 100, true);
        }

        let interactive = decide(
            &candidates,
            &Constraints::default(),
            &observations,
            &Preference {
                route: true,
                task: Some(TaskClass::Interactive),
                ..Preference::default()
            },
        );
        assert_eq!(routed(&interactive).model, "quick");
        let Decision::Routed { reasons, .. } = &interactive else {
            panic!("routed");
        };
        assert!(reasons
            .iter()
            .any(|reason| reason.contains("mean latency 200 ms")));

        let batch = decide(
            &candidates,
            &Constraints::default(),
            &observations,
            &Preference {
                route: true,
                task: Some(TaskClass::Batch),
                ..Preference::default()
            },
        );
        assert_eq!(routed(&batch).model, "cheap");

        // Failures outrank both: a model that fails is not fast or cheap.
        for _ in 0..5 {
            observations.record(&candidates[0].key, 200, 900, false);
        }
        let interactive = decide(
            &candidates,
            &Constraints::default(),
            &observations,
            &Preference {
                route: true,
                task: Some(TaskClass::Interactive),
                ..Preference::default()
            },
        );
        assert_eq!(routed(&interactive).model, "cheap");
    }

    #[test]
    fn an_unmeasured_model_does_not_win_on_a_missing_number() {
        let measured = candidate("acme", "measured");
        let unmeasured = candidate("acme", "unmeasured");
        let mut observations = Observations::new();
        observations.record(&measured.key, 5_000, 5_000, true);

        let decision = decide(
            &[unmeasured, measured.clone()],
            &Constraints::default(),
            &observations,
            &Preference {
                route: true,
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision).model, "measured");

        // With nothing measured at all the choice is still deterministic, and
        // it says plainly that it had no measurements.
        let decision = decide(
            &[candidate("b", "second"), candidate("a", "first")],
            &Constraints::default(),
            &Observations::new(),
            &Preference {
                route: true,
                ..Preference::default()
            },
        );
        assert_eq!(routed(&decision).provider, "a");
        let Decision::Routed { reasons, .. } = &decision else {
            panic!("routed");
        };
        assert!(reasons
            .iter()
            .any(|reason| reason.contains("nothing has been measured")));
    }

    #[test]
    fn a_cost_cap_and_an_empty_field_are_both_refusals_with_reasons() {
        let mut expensive = candidate("acme", "expensive");
        expensive.cost_micros_per_1k = Some(10_000);
        let decision = decide(
            &[expensive],
            &Constraints {
                max_cost_micros: Some(1_000),
                ..Constraints::default()
            },
            &Observations::new(),
            &Preference {
                route: true,
                ..Preference::default()
            },
        );
        assert!(
            matches!(&decision, Decision::Refused { reason, .. }
                if reason.contains("no configured model")),
            "{decision:?}"
        );
        assert!(decision.excluded()[0]
            .reason
            .contains("exceeds the 1000 cap"));

        let decision = decide(
            &[],
            &Constraints::default(),
            &Observations::new(),
            &Preference::default(),
        );
        assert!(matches!(&decision, Decision::Refused { reason, .. }
            if reason.contains("no default model")));
    }
}

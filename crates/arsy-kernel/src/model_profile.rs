//! Versioned model capabilities with bounded, optional observation.

use crate::provider::{ModelKey, ProviderError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    Declared,
    Probed,
    Override,
}

/// Numeric limits remain distinct so callers cannot mistake support for size.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityConstraints {
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_schema_bytes: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelCapability {
    pub state: CapabilityState,
    pub source: CapabilitySource,
    pub observed_at_unix_seconds: u64,
    pub constraints: CapabilityConstraints,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelProfile {
    pub schema_version: u32,
    pub key: ModelKey,
    pub capabilities: BTreeMap<String, ModelCapability>,
}

impl ModelProfile {
    pub fn is_stale(&self, now: u64, max_age_seconds: u64) -> bool {
        self.capabilities.values().any(|capability| {
            now.saturating_sub(capability.observed_at_unix_seconds) > max_age_seconds
        })
    }
}

/// Capability names this build declares for every dialect it speaks.
pub const DECLARED_CAPABILITIES: &[&str] = &["streaming", "tool_calls", "reasoning_effort"];

/// What this build's adapter for `dialect` can do, as a declaration rather
/// than an observation.
///
/// These are facts about ARSY's own adapters, not claims about a vendor's
/// model: every adapter streams, encodes tool definitions, and forwards a
/// reasoning effort. Whether a particular model honours the request is what a
/// probe would observe, so the observation date stays zero until one runs.
pub fn declared(dialect_max_output_tokens: Option<u64>) -> BTreeMap<String, ModelCapability> {
    DECLARED_CAPABILITIES
        .iter()
        .map(|name| {
            (
                (*name).to_owned(),
                ModelCapability {
                    state: CapabilityState::Supported,
                    source: CapabilitySource::Declared,
                    observed_at_unix_seconds: 0,
                    constraints: CapabilityConstraints {
                        max_input_tokens: None,
                        max_output_tokens: dialect_max_output_tokens,
                        max_schema_bytes: None,
                    },
                },
            )
        })
        .collect()
}

/// A probe can only observe model metadata: it receives no prompt, tool, or
/// operation payload with which to cause an external effect.
pub trait ModelCapabilityProbe {
    fn probe(
        &self,
        model: &ModelKey,
        observed_at_unix_seconds: u64,
    ) -> Result<BTreeMap<String, ModelCapability>, ProviderError>;
}

struct CachedProbe {
    observed_at_unix_seconds: u64,
    capabilities: BTreeMap<String, ModelCapability>,
}

/// In-memory probe cache. Persistence belongs to the caller's profile store.
#[derive(Default)]
pub struct ModelProfileResolver {
    probes: HashMap<ModelKey, CachedProbe>,
    last_attempts: HashMap<ModelKey, u64>,
}

impl ModelProfileResolver {
    pub fn resolve(
        &mut self,
        mut declared: ModelProfile,
        overrides: BTreeMap<String, ModelCapability>,
        probe: Option<&dyn ModelCapabilityProbe>,
        now: u64,
        stale_after_seconds: u64,
        min_probe_interval_seconds: u64,
    ) -> Result<ModelProfile, ProviderError> {
        let cached_is_fresh = self.probes.get(&declared.key).is_some_and(|cached| {
            now.saturating_sub(cached.observed_at_unix_seconds) <= stale_after_seconds
        });
        let may_probe = self
            .last_attempts
            .get(&declared.key)
            .is_none_or(|last| now.saturating_sub(*last) >= min_probe_interval_seconds);

        if !cached_is_fresh && declared.is_stale(now, stale_after_seconds) && may_probe {
            if let Some(probe) = probe {
                self.last_attempts.insert(declared.key.clone(), now);
                let mut capabilities = probe.probe(&declared.key, now)?;
                for capability in capabilities.values_mut() {
                    capability.source = CapabilitySource::Probed;
                    capability.observed_at_unix_seconds = now;
                }
                self.probes.insert(
                    declared.key.clone(),
                    CachedProbe {
                        observed_at_unix_seconds: now,
                        capabilities,
                    },
                );
            }
        }
        if let Some(cached) = self.probes.get(&declared.key) {
            declared.capabilities.extend(cached.capabilities.clone());
        }
        declared
            .capabilities
            .extend(overrides.into_iter().map(|(name, mut capability)| {
                capability.source = CapabilitySource::Override;
                (name, capability)
            }));
        Ok(declared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Probe(AtomicUsize);

    impl ModelCapabilityProbe for Probe {
        fn probe(
            &self,
            _model: &ModelKey,
            observed_at_unix_seconds: u64,
        ) -> Result<BTreeMap<String, ModelCapability>, ProviderError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok([(
                "tools".to_owned(),
                capability(
                    CapabilityState::Supported,
                    CapabilitySource::Probed,
                    observed_at_unix_seconds,
                    Some(128),
                ),
            )]
            .into_iter()
            .collect())
        }
    }

    fn capability(
        state: CapabilityState,
        source: CapabilitySource,
        observed: u64,
        max_schema_bytes: Option<u64>,
    ) -> ModelCapability {
        ModelCapability {
            state,
            source,
            observed_at_unix_seconds: observed,
            constraints: CapabilityConstraints {
                max_schema_bytes,
                ..CapabilityConstraints::default()
            },
        }
    }

    fn declared() -> ModelProfile {
        ModelProfile {
            schema_version: 1,
            key: ModelKey {
                provider: "stub".to_owned(),
                model: "model".to_owned(),
            },
            capabilities: [(
                "tools".to_owned(),
                capability(
                    CapabilityState::Unknown,
                    CapabilitySource::Declared,
                    10,
                    None,
                ),
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn probes_are_optional_cached_rate_limited_and_stale_profiles_reprobe() {
        let probe = Probe(AtomicUsize::new(0));
        let mut resolver = ModelProfileResolver::default();

        let without_probe = resolver
            .resolve(declared(), BTreeMap::new(), None, 100, 20, 10)
            .unwrap();
        assert!(without_probe.is_stale(100, 20));

        let first = resolver
            .resolve(declared(), BTreeMap::new(), Some(&probe), 100, 20, 10)
            .unwrap();
        assert_eq!(first.capabilities["tools"].source, CapabilitySource::Probed);
        assert_eq!(
            first.capabilities["tools"].constraints.max_schema_bytes,
            Some(128)
        );

        resolver
            .resolve(declared(), BTreeMap::new(), Some(&probe), 105, 0, 10)
            .unwrap();
        assert_eq!(
            probe.0.load(Ordering::Relaxed),
            1,
            "a stale result remains cached until the probe interval elapses"
        );

        resolver
            .resolve(declared(), BTreeMap::new(), Some(&probe), 121, 20, 10)
            .unwrap();
        assert_eq!(
            probe.0.load(Ordering::Relaxed),
            2,
            "stale result is re-probed"
        );

        let overridden = resolver
            .resolve(
                declared(),
                [(
                    "tools".to_owned(),
                    capability(
                        CapabilityState::Unsupported,
                        CapabilitySource::Override,
                        122,
                        None,
                    ),
                )]
                .into_iter()
                .collect(),
                Some(&probe),
                122,
                20,
                10,
            )
            .unwrap();
        assert_eq!(
            overridden.capabilities["tools"].source,
            CapabilitySource::Override
        );
    }
}

//! Provenance-preserving prompt compilation with fail-safe cache partitioning.

use crate::{domain::FragmentId, secret::Redactor};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fmt};

pub const MAX_PROMPT_FRAGMENTS: usize = 4096;
pub const MAX_PROMPT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFamily {
    Gpt,
    Claude,
    Gemini,
    QwenDeepseek,
    Local,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptFragmentKind {
    StableInstruction,
    Task,
    Context,
    PermissionState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PromptFragment {
    pub id: FragmentId,
    pub kind: PromptFragmentKind,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptIr {
    fragments: Vec<PromptFragment>,
}

impl PromptIr {
    pub fn fragments(&self) -> &[PromptFragment] {
        &self.fragments
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RenderedSegment {
    pub fragment: FragmentId,
    pub text: String,
}

pub trait PromptStrategy: Send + Sync {
    fn supports(&self, family: ModelFamily) -> bool;
    fn render(&self, ir: &PromptIr) -> Result<Vec<RenderedSegment>, String>;
}

/// The five deliberately small family strategies. They share the same typed
/// IR and differ only in instruction order, so no adapter can reinterpret or
/// drop a fragment while formatting a provider request.
#[derive(Clone, Copy, Debug)]
pub struct FamilyPromptStrategy {
    family: ModelFamily,
}

impl FamilyPromptStrategy {
    pub const fn new(family: ModelFamily) -> Self {
        Self { family }
    }
}

impl PromptStrategy for FamilyPromptStrategy {
    fn supports(&self, family: ModelFamily) -> bool {
        self.family == family
    }

    fn render(&self, ir: &PromptIr) -> Result<Vec<RenderedSegment>, String> {
        let order = match self.family {
            ModelFamily::Gpt => [0, 2, 3, 1],
            ModelFamily::Claude => [0, 3, 2, 1],
            ModelFamily::Gemini => [2, 0, 3, 1],
            ModelFamily::QwenDeepseek => [0, 2, 1, 3],
            // Weak local models get the concrete task before background text.
            ModelFamily::Local => [1, 0, 3, 2],
        };
        let rank = |kind| match kind {
            PromptFragmentKind::StableInstruction => order[0],
            PromptFragmentKind::Task => order[1],
            PromptFragmentKind::Context => order[2],
            PromptFragmentKind::PermissionState => order[3],
        };
        let mut fragments = ir.fragments().iter().enumerate().collect::<Vec<_>>();
        fragments.sort_by_key(|(position, fragment)| (rank(fragment.kind), *position));
        Ok(fragments
            .into_iter()
            .map(|(_, fragment)| RenderedSegment {
                fragment: fragment.id,
                text: fragment.content.clone(),
            })
            .collect())
    }
}

pub const BUILT_IN_STRATEGIES: [FamilyPromptStrategy; 5] = [
    FamilyPromptStrategy::new(ModelFamily::Gpt),
    FamilyPromptStrategy::new(ModelFamily::Claude),
    FamilyPromptStrategy::new(ModelFamily::Gemini),
    FamilyPromptStrategy::new(ModelFamily::QwenDeepseek),
    FamilyPromptStrategy::new(ModelFamily::Local),
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CacheSegment {
    pub fragment: FragmentId,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompiledPrompt {
    pub segments: Vec<RenderedSegment>,
    pub manifest: Vec<FragmentId>,
    pub cache_segments: Vec<CacheSegment>,
    pub estimated_tokens: u32,
    pub degradation: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptError {
    TooManyFragments,
    DuplicateFragment(FragmentId),
    TooLarge,
    TokenBudgetExceeded { estimated: u32, budget: u32 },
    Redaction(String),
}

impl fmt::Display for PromptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyFragments => formatter.write_str("too many prompt fragments"),
            Self::DuplicateFragment(id) => write!(formatter, "duplicate prompt fragment {id}"),
            Self::TooLarge => formatter.write_str("prompt exceeds byte limit"),
            Self::TokenBudgetExceeded { estimated, budget } => {
                write!(
                    formatter,
                    "prompt estimate {estimated} exceeds budget {budget}"
                )
            }
            Self::Redaction(error) => write!(formatter, "prompt redaction failed: {error}"),
        }
    }
}

impl std::error::Error for PromptError {}

/// Compile through the first matching family strategy. Strategy errors and
/// provenance violations degrade to the canonical, input-order rendering.
pub fn compile(
    family: ModelFamily,
    fragments: Vec<PromptFragment>,
    strategies: &[&dyn PromptStrategy],
    redactor: &Redactor,
    token_budget: u32,
) -> Result<CompiledPrompt, PromptError> {
    if fragments.len() > MAX_PROMPT_FRAGMENTS {
        return Err(PromptError::TooManyFragments);
    }
    let mut ids = HashSet::with_capacity(fragments.len());
    for fragment in &fragments {
        if !ids.insert(fragment.id) {
            return Err(PromptError::DuplicateFragment(fragment.id));
        }
    }
    let input_bytes = fragments.iter().try_fold(0_usize, |total, fragment| {
        total.checked_add(fragment.content.len())
    });
    if input_bytes.is_none_or(|bytes| bytes > MAX_PROMPT_BYTES) {
        return Err(PromptError::TooLarge);
    }

    let mut redacted_ids = HashSet::new();
    let sanitized = fragments
        .into_iter()
        .map(|mut fragment| {
            let content = redactor
                .sanitize(&fragment.content)
                .map_err(|error| PromptError::Redaction(error.to_string()))?;
            if content != fragment.content {
                redacted_ids.insert(fragment.id);
            }
            fragment.content = content;
            Ok(fragment)
        })
        .collect::<Result<Vec<_>, PromptError>>()?;
    let ir = PromptIr {
        fragments: sanitized,
    };
    let canonical = || {
        ir.fragments
            .iter()
            .map(|fragment| RenderedSegment {
                fragment: fragment.id,
                text: fragment.content.clone(),
            })
            .collect::<Vec<_>>()
    };

    let (mut segments, degradation) =
        match strategies.iter().find(|strategy| strategy.supports(family)) {
            Some(strategy) => match strategy.render(&ir) {
                Ok(segments) if valid_provenance(&segments, &ids) => (segments, None),
                Ok(_) => (
                    canonical(),
                    Some("strategy emitted invalid provenance".to_owned()),
                ),
                Err(error) => (canonical(), Some(format!("strategy failed: {error}"))),
            },
            None => (canonical(), Some("no family strategy available".to_owned())),
        };
    for segment in &mut segments {
        let text = redactor
            .sanitize(&segment.text)
            .map_err(|error| PromptError::Redaction(error.to_string()))?;
        if text != segment.text {
            redacted_ids.insert(segment.fragment);
            segment.text = text;
        }
    }
    let bytes = segments.iter().try_fold(0_usize, |total, segment| {
        total.checked_add(segment.text.len())
    });
    if bytes.is_none_or(|bytes| bytes > MAX_PROMPT_BYTES) {
        return Err(PromptError::TooLarge);
    }
    let estimated_tokens = bytes.unwrap_or_default().div_ceil(4) as u32;
    if estimated_tokens > token_budget {
        return Err(PromptError::TokenBudgetExceeded {
            estimated: estimated_tokens,
            budget: token_budget,
        });
    }
    let stable: std::collections::HashMap<_, _> = ir
        .fragments
        .iter()
        .filter(|fragment| fragment.kind == PromptFragmentKind::StableInstruction)
        .map(|fragment| (fragment.id, fragment.content.as_str()))
        .collect();
    let cache_segments = segments
        .iter()
        .filter(|segment| {
            stable.get(&segment.fragment) == Some(&segment.text.as_str())
                && !redacted_ids.contains(&segment.fragment)
        })
        .map(|segment| CacheSegment {
            fragment: segment.fragment,
            text: segment.text.clone(),
        })
        .collect();
    let manifest = segments.iter().map(|segment| segment.fragment).collect();
    Ok(CompiledPrompt {
        segments,
        manifest,
        cache_segments,
        estimated_tokens,
        degradation,
    })
}

fn valid_provenance(segments: &[RenderedSegment], expected: &HashSet<FragmentId>) -> bool {
    segments.len() == expected.len()
        && segments
            .iter()
            .all(|segment| expected.contains(&segment.fragment))
        && segments
            .iter()
            .map(|segment| segment.fragment)
            .collect::<HashSet<_>>()
            .len()
            == expected.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::SecretHandle;

    struct Failing;

    struct Dropping;

    impl PromptStrategy for Failing {
        fn supports(&self, family: ModelFamily) -> bool {
            family == ModelFamily::Claude
        }

        fn render(&self, _ir: &PromptIr) -> Result<Vec<RenderedSegment>, String> {
            Err("unsupported schema".to_owned())
        }
    }

    impl PromptStrategy for Dropping {
        fn supports(&self, _family: ModelFamily) -> bool {
            true
        }

        fn render(&self, _ir: &PromptIr) -> Result<Vec<RenderedSegment>, String> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn fallback_preserves_provenance_and_excludes_secrets_and_permissions_from_cache() {
        let stable = FragmentId::new();
        let secret = FragmentId::new();
        let permission = FragmentId::new();
        let handle = SecretHandle::new("test", "token").unwrap();
        let mut redactor = Redactor::new();
        redactor.register(&handle, "secret-value").unwrap();
        let compiled = compile(
            ModelFamily::Claude,
            vec![
                PromptFragment {
                    id: stable,
                    kind: PromptFragmentKind::StableInstruction,
                    content: "answer concisely".to_owned(),
                },
                PromptFragment {
                    id: secret,
                    kind: PromptFragmentKind::StableInstruction,
                    content: "credential=secret-value".to_owned(),
                },
                PromptFragment {
                    id: permission,
                    kind: PromptFragmentKind::PermissionState,
                    content: "fs.write denied".to_owned(),
                },
            ],
            &[&Failing],
            &redactor,
            100,
        )
        .unwrap();

        assert_eq!(compiled.manifest, vec![stable, secret, permission]);
        assert!(compiled
            .segments
            .iter()
            .all(|segment| !segment.text.contains("secret-value")));
        assert_eq!(compiled.cache_segments.len(), 1);
        assert_eq!(compiled.cache_segments[0].fragment, stable);
        assert_eq!(
            compiled.degradation.as_deref(),
            Some("strategy failed: unsupported schema")
        );
    }

    #[test]
    fn a_strategy_cannot_drop_a_mandatory_instruction() {
        let instruction = FragmentId::new();
        let compiled = compile(
            ModelFamily::Gpt,
            vec![PromptFragment {
                id: instruction,
                kind: PromptFragmentKind::StableInstruction,
                content: "mandatory".into(),
            }],
            &[&Dropping],
            &Redactor::new(),
            100,
        )
        .unwrap();

        assert_eq!(compiled.manifest, vec![instruction]);
        assert_eq!(compiled.segments[0].text, "mandatory");
        assert_eq!(
            compiled.degradation.as_deref(),
            Some("strategy emitted invalid provenance")
        );
    }
}

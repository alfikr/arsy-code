# Prompt compiler

## Problem

One static system prompt wastes tokens, weakens instruction ordering, couples tools to vendors, and cannot exploit caching or model-specific behavior.

## Design

Inputs are typed fragments: global behavior, resolved project instructions, task, agent role, model profile, capability view, policy state, environment, memory, plan, active skills, and budget. The compiler validates authority, resolves conflicts, partitions stable/cacheable content, selects a family strategy, and emits a prompt plus manifest.

```rust
pub trait PromptStrategy: Send + Sync {
    fn supports(&self, profile: &ModelProfile) -> bool;
    fn compile(&self, ir: &PromptIr, budget: TokenBudget)
        -> Result<CompiledPrompt, PromptError>;
}

pub struct CompiledPrompt {
    pub messages: Vec<CanonicalMessage>,
    pub tool_view: ToolSchemaSet,
    pub manifest: Vec<FragmentId>,
    pub cache_segments: Vec<CacheSegment>,
    pub estimated_tokens: u32,
}
```

Family strategies cover GPT, Claude, Gemini, Qwen/DeepSeek, and constrained local models. Differences include instruction order, native versus in-band tool grammar, schema complexity, reasoning controls, image representation, and safe few-shot examples. Model identity alone is insufficient; the resolved profile is versioned and probe-backed.

## Invariants

Prompts cannot grant capability, secret content is removed before compilation, every emitted segment maps to provenance, and adapters cannot silently drop mandatory instructions. Cached segments never include volatile secrets or permission state.

## Failure and alternatives

Overfitting is caught by cross-model evals; strategy failure falls back to a conservative canonical rendering and records degradation. Template sprawl is controlled by shared IR passes plus small strategies—not provider-specific tool implementations. Dynamic intervention is a runtime rule output, recorded as a new fragment, not an invisible prompt mutation.

## Performance and open questions

Cache tokenization by `(artifact_hash, tokenizer_version)` and compile incrementally. The number of strategy families and when few-shot insertion pays for itself remain measured decisions.

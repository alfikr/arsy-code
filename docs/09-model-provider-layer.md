# Model and provider layer

## Problem and existing approaches

Providers differ in streaming, reasoning, tools, caching, images, errors, authentication, residency, and limits. OMP contains numerous dialect paths (**V**); Codex exposes a provider-owned Rust trait with capability bounds and auth/error adaptation (**V**). A single “OpenAI compatible” switch is insufficient.

## Canonical profile

```rust
pub struct ModelProfile {
    pub key: ModelKey,
    pub modalities: ModalitySet,
    pub tools: ToolCallProfile,
    pub reasoning: ReasoningProfile,
    pub structured_output: SchemaProfile,
    pub streaming: StreamingProfile,
    pub prompt_cache: CacheProfile,
    pub limits: ModelLimits,
    pub native: NativeCapabilitySet,
    pub provenance: CapabilityProvenance,
}

pub trait ModelProvider: Send + Sync {
    fn descriptor(&self) -> &ProviderDescriptor;
    fn resolve(&self, model: &ModelKey)
        -> BoxFuture<'_, Result<ModelProfile, ProviderError>>;
    fn stream(&self, request: CanonicalModelRequest)
        -> BoxFuture<'_, Result<ModelEventStream, ProviderError>>;
}
```

Capability values are tri-state (`supported`, `unsupported`, `unknown`) with source (`declared`, `probed`, `override`), observed date, and constraints. Limits are structured numbers, not booleans. Probes are non-destructive, cached, rate-limited, and optional.

```mermaid
flowchart LR
  A[Canonical request] --> C[Capability resolver]
  C --> P[Prompt strategy]
  P --> T[Tool schema adapter]
  T --> W[Provider wire adapter]
  W --> S[Normalized event stream]
```

## Provider coverage

OpenAI, Anthropic, Google, xAI, DeepSeek, Mistral, MiniMax, Qwen, Moonshot, Groq, Together, OpenRouter, Azure OpenAI, Bedrock, Vertex, Ollama, llama.cpp, LM Studio, and custom compatible servers are adapter targets—not launch guarantees. First release implements the minimum routes required by eval coverage.

## Security, failure, and routing

Credentials are handles resolved inside the provider worker and never prompt fragments. Wire bodies are size-limited and redacted. Streams normalize partial tool arguments without executing them. Retries honor idempotency and provider retry hints. Policy-controlled routing considers task class, measured quality, latency, cost, residency, and capability; users can pin a model or disable routing.

## Decision

Provider adapters own wire/auth/error semantics; prompt strategies own model behavior; operations remain provider-independent. Provider profiles are tested contracts, not marketing metadata.

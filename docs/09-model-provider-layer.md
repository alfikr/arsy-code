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

Two adapters have landed: the Anthropic Messages dialect and the OpenAI Chat Completions dialect. Both are written against the dialect rather than a vendor, and the base URL comes from configuration, so the OpenAI adapter also serves OpenRouter, LiteLLM, Azure-style gateways, Ollama, llama.cpp, and LM Studio without a further adapter. Google, xAI, Bedrock, and Vertex remain adapter targets—not launch guarantees.

HTTP is injected as a `WireTransport`, so a dialect's wire contract is exercised without a network; the implementation that reaches a host lives in one module and knows nothing about which provider it is carrying.

## Security, failure, and routing

Credentials are handles resolved inside the provider worker and never prompt fragments; [`arsy auth`](36-cli-tui.md) creates and revokes them, by API key (`arsy auth set`) or by an OAuth device or PKCE login against the client the configuration names (`arsy auth login`), and `arsy provider list` and `arsy model list` show what a resolved policy allows. Wire bodies are size-limited and redacted. Streams normalize partial tool arguments without executing them. Retries honor idempotency and provider retry hints. Policy-controlled routing considers task class, measured quality, latency, cost, residency, and capability; users can pin a model or disable routing.

## Decision

Provider adapters own wire/auth/error semantics; prompt strategies own model behavior; operations remain provider-independent. Provider profiles are tested contracts, not marketing metadata.

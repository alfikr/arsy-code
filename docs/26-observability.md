# Observability and artifacts

## Requirements

Every significant action must be reconstructable without exposing secrets. Events include session/turn/model/operation/policy/sandbox/edit/diagnostic/test/agent/context/plugin lifecycle. Metrics cover latency, tokens, cost, cache, retries, failed edits, context waste, and resource usage.

```rust
pub struct TelemetryEvent {
    pub name: EventName,
    pub trace: TraceId,
    pub actor: Principal,
    pub attributes: RedactedAttributes,
    pub evidence: Vec<EvidenceRef>,
}
```

OpenTelemetry export is optional and off by default for content. Local structured tracing uses correlation/causation IDs. A redaction pipeline runs before logs, model calls, telemetry, plugins, protocol adapters, and diagnostic bundles.

## Artifact system

Large logs, images, PDFs, test/coverage reports, profiles, database results, screenshots, and debug traces live in CAS. `artifact://session/<id>/<artifact>` is a reference identity, not a filesystem path, readable through [`arsy artifact show`](36-cli-tui.md). Metadata records MIME type, digest, size, creator, source revision, sensitivity, retention, and derivation. Model renderers receive bounded excerpts and retrieval handles.

## Failure and performance

Telemetry backpressure never blocks authoritative event commits; drops are counted. Redaction failures fail closed at external boundaries. High-cardinality labels stay out of metrics. Sampling applies to traces, not audit events. Artifact decompression and rendering are size/ratio bounded.

## Decision

Canonical events provide audit; tracing provides runtime causality; metrics provide aggregates; artifacts retain payloads. They are linked but not conflated.

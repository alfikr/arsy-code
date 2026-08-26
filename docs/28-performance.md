# Performance engineering

## Targets

Targets are **P**, not achieved measurements.

| Metric | Initial target |
|---|---:|
| warm local `--help` p95 | <100 ms |
| first TUI frame p95 | <100 ms |
| in-process operation dispatch p95 | <1 ms excluding work |
| local daemon request overhead p95 | <2 ms excluding work |
| event append p95 | <5 ms at normal durability |
| idle service RSS | <80 MiB without LSP/DAP/WASM workers |

Search, edit, context retrieval, LSP startup/query, DAP, process spawn, artifact throughput, plugin startup, session replay, and subagent creation receive repository-size-stratified benchmarks. Token metrics include prompt fragments, tool renderings, cached input, retries, and discarded context.

## Design controls

Lazy-start language/debug/plugin workers; cache by content/version; stream large output to artifacts; batch SQLite writes; use bounded channels; keep hot dispatch in process; avoid eager repository indexing; feature-gate heavy subsystems. Measure before adding RocksDB, a daemon cache, or custom allocators.

## Failure and observability

Every queue has backpressure and a saturation metric. Timeouts are end-to-end budgets, not independent timers that multiply. Performance tests distinguish cold/warm and local/remote. Regression thresholds run in CI on stable hardware; nightly runs cover noisy end-to-end metrics.

## Decision

Optimize user-perceived latency and success/token first. Architectural latency budgets are gates; no dependency is selected solely for benchmark fashion.

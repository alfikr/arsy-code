# ADR-0001: Rust-first core

- Status: Accepted
- Date: 2026-08-26

## Context

The core coordinates untrusted processes, durable state, streaming, and cross-platform enforcement. OMP shows the capability of a TS/Bun plus Rust split (**V**); Codex demonstrates a broad Rust systems implementation (**V**).

## Decision

Implement the service, domain, policy, persistence, execution, and code runtime in Rust. Scripting languages are external/sandboxed extension targets. Start with three crates and extract only at stable process, privilege, version, or compile boundaries.

## Consequences

One toolchain and strong data-race/memory-safety defaults simplify deployment. Compile time and async complexity require feature gates, owned task boundaries, and benchmarks.

## Alternatives

TypeScript maximizes extension familiarity but expands critical-runtime dependency and sandbox surface. Go simplifies compilation but offers a weaker ecosystem for several chosen parsers and embedded WASM integrations. A mixed core was rejected.

## Invariant

No scripting runtime is required for core offline coding operations.

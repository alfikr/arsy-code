# ADR-0006: WASM is the default compute-plugin boundary

- Status: Accepted
- Date: 2026-08-26

## Context

Extensions are valuable but native in-process code bypasses memory safety, policy, and crash isolation. Wasmtime/WASI supports sandboxed compute and capability-style I/O.

## Decision

Use data/declarative extensions when possible and WASM components for compute. Grant only declared host imports with fuel, time, memory, and output limits. Trusted native extensions are exceptional and out of process.

## Consequences

Security and portability improve at the cost of startup and constrained APIs. Pooling is benchmark-gated.

## Alternatives

Native dynamic libraries are fastest but unsafe. A bundled JS runtime enlarges the critical runtime and still needs sandboxing.

## Invariant

Plugins cannot access ambient filesystem, network, process, clocks, randomness, or secrets.

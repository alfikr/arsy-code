# ADR-0005: Compatibility at the edges

- Status: Accepted
- Date: 2026-08-26

## Context

Users have valuable Claude, Codex, and OMP configuration. Their formats, precedence, and runtime concepts differ, and some implementations are closed.

## Decision

Parse each ecosystem into a versioned compatibility model, emit loss diagnostics, then translate to canonical instructions, policy, agents, operations, and connections. Preserve originals as artifacts.

## Consequences

Migration can be low-friction without contaminating domain types. Behavioral fidelity must be earned per fixture and version.

## Alternatives

One merged config parser creates ambiguous precedence. Ignoring compatibility imposes unnecessary migration.

## Invariant

No canonical enum has vendor-specific variants solely for compatibility.

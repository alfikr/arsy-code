# ADR-0008: DAP-backed debugging capability

- Status: Accepted
- Date: 2026-08-26

## Context

DAP standardizes debugger adapters (**D**), and OMP verifies broad agent-facing debugging operations (**V**). Print-only debugging loses state and control evidence.

## Decision

Host DAP adapters and expose typed launch/attach, breakpoint, step, stack, scope, variable, evaluation, thread, exception, and memory operations according to negotiated capability.

## Consequences

Agents can inspect runtime behavior, while attach/evaluate/write-memory require strict capability separation and sandbox policy.

## Alternatives

Shelling directly to debuggers is language-specific. Print statements remain a fallback, not the architecture.

## Invariant

Absent adapter capability means unsupported; it is never inferred as available.

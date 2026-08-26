# ADR-0009: Typed prompt compiler

- Status: Accepted
- Date: 2026-08-26

## Context

Models differ in instruction sensitivity, tool grammar, caching, reasoning, schemas, and context limits. Static prompts waste tokens and spread quirks.

## Decision

Compile typed, attributed prompt IR through model-profile strategies. Emit a manifest, token estimate, cache partitions, and generated tool view.

## Consequences

Adaptation stays out of operations and becomes eval-testable. Strategy drift requires versioning and cross-model regression tests.

## Alternatives

One universal prompt is simpler but predictably suboptimal. Per-provider hand-built prompts duplicate policy and instructions.

## Invariant

Prompt compilation can reduce presentation, never increase authority.

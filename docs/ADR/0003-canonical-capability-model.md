# ADR-0003: Canonical operation/capability model

- Status: Accepted
- Date: 2026-08-26

## Context

Tool calls conflate model schema, executable behavior, permission, UI, and telemetry. The same effect may arrive through LLM, MCP, ACP, CLI, or plugin.

## Decision

Canonicalize `Operation + Resource + Effect + CapabilityRequirement`. Generate model-facing tools and protocol mappings as views. Resolve resources before policy and record observed effects.

## Consequences

Adapters are replaceable and policy is uniform. More translation code is required, but it is conformance-testable.

## Alternatives

A universal `Tool` trait is simpler but leaks model and UI concerns. Direct shell access is expressive but ungovernable.

## Invariant

Every external effect has an operation ID, actor, scoped grant, and evidence outcome.

# ADR-0011: Capability policy separated from enforcement

- Status: Accepted
- Date: 2026-08-26

## Context

Approval policy, authorization, risk, and OS sandbox mechanisms solve different problems. Codex source demonstrates platform-specific enforcement (**V**).

## Decision

Evaluate typed capability requirements in a portable policy engine, obtain exact-scope approval when needed, then compile the grant into a platform sandbox plan. Record both decision and observed enforcement.

## Consequences

UI and platform mechanisms can evolve independently. Guarantee gaps must be explicit and may make operations unavailable.

## Alternatives

Tool allowlists are coarse. Sandbox-only designs cannot express user/enterprise intent. Approval-only designs do not contain compromised processes.

## Invariant

Content, hooks, children, and compatibility imports cannot amplify a principal’s capability set.

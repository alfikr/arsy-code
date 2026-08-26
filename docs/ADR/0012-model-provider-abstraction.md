# ADR-0012: Provider/profile/strategy separation

- Status: Accepted
- Date: 2026-08-26

## Context

Provider transport and authentication differ from model behavior and capability. OMP’s dialect breadth and Codex’s provider trait show both dimensions (**V**).

## Decision

Providers own auth, wire transport, streaming, and error normalization. Versioned model profiles describe probed/declarative capabilities. Prompt and tool-schema strategies adapt to profiles. Routing is a separate policy.

## Consequences

New providers do not rewrite operations; the conformance surface is larger than a single HTTP trait. Unknown capability stays unknown.

## Alternatives

OpenAI-compatible-only support fails on semantics. Provider-specific agents create duplication. Static catalogs drift silently.

## Invariant

A provider adapter emits normalized model events and has no workspace mutation handle.

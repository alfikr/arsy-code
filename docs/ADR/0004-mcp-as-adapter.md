# ADR-0004: MCP is an edge adapter

- Status: Accepted
- Date: 2026-08-26

## Context

MCP is a strong, evolving JSON-RPC integration standard with tools, resources, prompts, elicitation, and extensions (**D**). Its annotations are explicitly untrusted.

## Decision

Implement MCP client and server adapters around the canonical capability bus. Raw MCP types and authority do not cross the adapter boundary.

## Consequences

ARSY interoperates without inheriting MCP’s trust or lifecycle model. Bidirectional mapping needs versioned conformance tests.

## Alternatives

Using MCP internally reduces adapters but makes core evolution and security dependent on an external protocol. Client-only support prevents ARSY capability reuse.

## Invariant

An MCP server cannot grant itself capabilities or make annotations authoritative.

# MCP and ACP adapters

## Problem and existing approaches

MCP 2026-07-28 standardizes JSON-RPC integrations among hosts, clients, and servers, with tools/resources/prompts/elicitation and optional tasks, skills, and apps (**D**). ACP v1 standardizes agent/client session, permission, filesystem, terminal, and update flows (**D**). Neither is a sufficient internal domain model.

## MCP design

ARSY is both client and optional server.

```mermaid
flowchart LR
  MS[MCP server] --> MC[MCP client adapter] --> B[Capability bus]
  B --> SS[MCP server adapter] --> EX[External MCP client]
```

Support stdio and Streamable HTTP first; legacy SSE only behind compatibility demand. Connections are managed through [`arsy mcp`](36-cli-tui.md), whose `test` subcommand negotiates capabilities without invoking a tool. Negotiate capabilities, correlate requests, bound messages/timeouts, support cancellation/progress, authenticate HTTP, and require policy for sampling/elicitation/tool effects. Resources become external-resource references and artifacts; annotations remain untrusted. MCP Apps render in a sandboxed UI origin with a mediated bridge.

## ACP design

Map `initialize`, authentication, `session/new|load|prompt|cancel`, updates, permission requests, filesystem, and terminal methods to protocol projections and operations. Honor absolute paths and 1-based lines at the adapter, then canonicalize internally. Advertise only implemented capabilities. Extension `_meta` and underscore methods never alter core authority.

## Failure, security, performance

Malformed messages, duplicate IDs, reconnects, cancellation races, hostile schemas, OAuth confusion, and server impersonation are tested. Each connection has an identity, trust label, rate limit, body cap, and capability ceiling. External servers cannot recursively trigger unbounded sampling or elicitation. Connection pools and lazy discovery prevent startup fan-out.

## Decision

MCP and ACP are versioned bidirectional edge adapters. Their raw request types stop at the adapter boundary.

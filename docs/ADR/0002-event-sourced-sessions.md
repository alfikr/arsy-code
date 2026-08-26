# ADR-0002: Event-sourced sessions with SQLite and CAS

- Status: Accepted
- Date: 2026-08-26

## Context

Sessions need resume, rewind, fork, replay, audit, and lossless compaction. OMP’s append-oriented tree and Codex’s storage/trace boundaries validate lineage and projections (**V**).

## Decision

Store immutable, versioned event envelopes in SQLite WAL and large payloads in a content-addressed store. Build replaceable projections. JSONL is import/export, not authoritative storage.

## Consequences

Transactions and queries improve; schema migration and artifact GC become explicit responsibilities. One writer is accepted until measurement disproves it.

## Alternatives

JSONL lacks multi-record transactions/query indexes. RocksDB adds operational complexity. A custom log is unjustified.

## Invariant

Compaction never deletes or rewrites canonical events.

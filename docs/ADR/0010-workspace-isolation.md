# ADR-0010: Snapshot readers and isolated writers

- Status: Accepted
- Date: 2026-08-26

## Context

Parallel writer agents can corrupt shared state. Git worktrees are mature but share repository internals; overlays are faster but platform-specific.

## Decision

Read-only agents share an immutable revision. Each writer receives a dedicated worktree in Git repositories or copied/reflink snapshot otherwise. Merging is a typed, policy-checked operation. Overlay/COW backends are deferred.

## Consequences

Isolation is understandable and portable; disk and setup cost must be measured. Shared Git ref operations remain serialized/protected.

## Alternatives

Shared directories are unsafe. Temporary clones isolate more but duplicate data/network. Overlay filesystems lack uniform native Windows/macOS behavior.

## Invariant

Only one writer may mutate a filesystem view, and its base revision is recorded.

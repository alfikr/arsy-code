# Security and sandbox

## Problem and existing approaches

A policy decision says what is allowed; a sandbox limits what a process can actually do. Codex verifies separate Linux, macOS, and Windows mechanisms (**V**). No cross-platform mechanism has identical semantics.

## Architecture

```mermaid
flowchart LR
  I[Agent intent] --> C[Capability request]
  C --> P[Policy decision]
  P --> A[Approval if required]
  A --> S[Platform sandbox plan]
  S --> X[Execution]
  X --> O[Observed effects/violations]
```

Linux baseline: user/PID/network namespaces, read-only root, explicit writable mounts, seccomp, cgroups; Landlock as additional defense where supported. Bubblewrap is preferred when validated. macOS baseline: generated Seatbelt profile plus process/environment restrictions. Windows baseline: restricted tokens, Job Objects, ACL-scoped workspace, and network controls; AppContainer only after compatibility validation. Unsupported mandatory control fails closed.

```rust
pub trait SandboxBackend: Send + Sync {
    fn assurance(&self) -> SandboxAssurance;
    fn compile(&self, grant: &CapabilityGrant, target: &TargetDescriptor)
        -> Result<SandboxPlan, SandboxError>;
    fn launch(&self, plan: SandboxPlan, process: ProcessSpec)
        -> BoxFuture<'_, Result<SandboxedProcess, SandboxError>>;
}
```

## Threat controls

Canonicalize paths using directory handles; reject traversal and symlink races; default network deny; protect VCS metadata and credential stores; isolate IPC; bound CPU/memory/process counts/output; clear inherited handles; validate helper binaries and profiles. Sandbox workers are small, separately fuzzed/audited programs.

## Failure and compatibility

Backend setup failure does not silently run unsandboxed. Platform assurance is exposed to policy/UI. Compatibility approval modes translate to policy intent, but cannot demand a guarantee the OS cannot provide. Containers alone are not treated as a security boundary without a stated runtime profile.

## Decision and open questions

Policy and mechanism are separate, with defense in depth and explicit degradation. Windows network/filesystem parity and macOS profile evolution require dedicated conformance labs.

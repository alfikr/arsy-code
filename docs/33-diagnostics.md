# User-facing diagnostics

## Stable code contract

Every user-visible failure has a stable code shaped as `ARSY-<CLASS>-<NNNN>`. Codes are append-only, are never reused with different meaning, and remain stable across human, JSON, and CI output. `<CLASS>` identifies the subsystem; the four-digit number identifies one condition. Adapters translate provider and protocol errors into this namespace while retaining the original error as a redacted evidence artifact.

Every diagnostic contains:

- `code`, `severity`, `message`, and an actionable `remediation`;
- `operation_id` and relevant evidence references when an operation exists;
- `cause` and `source` when policy, configuration, or imported compatibility data contributed;
- `degraded: true` plus an explicit omission when execution safely continues.

Severity is one of `info`, `warning`, `error`, or `fatal`. `error` stops the current operation or turn. `fatal` stops the session because its integrity, authority, or durable state is uncertain. Warnings may continue only under the degradation rules below.

## Failure classes

| Evaluation class | Code class | Default severity and disposition | Required remediation hint |
|---|---|---|---|
| retrieval | `RET` | warning; continue with omissions | narrow the request, fix an index, or name a source |
| context selection | `CTX` | warning; continue with omitted fragments listed | reduce scope or raise an allowed context budget |
| compaction | `CMP` | error; halt the turn if originals cannot be cited | retry compaction or resume from uncompacted history |
| planning | `PLN` | error; halt the turn | clarify the objective or resolve the named dependency |
| model reasoning/hallucination | `MDL` | error; reject the unsupported claim/action | provide evidence, change model, or revise the request |
| tool selection | `TLS` | error; halt the operation | choose an available operation or enable the named adapter |
| schema | `SCH` | error; reject before execution | correct the named field and expected type |
| policy/approval | `POL` | error; deny the operation | change scope, request the named approval, or contact the policy owner |
| sandbox | `SBX` | fatal if required assurance is unavailable; otherwise error | install/fix the backend or request an allowed assurance profile |
| execution | `EXE` | error; halt the operation | inspect bounded stderr/evidence and retry an idempotent request |
| edit | `EDT` | error; leave the staged/base state intact | resolve the reported address or conflict and retry |
| stale-write | `STL` | error; reject the write | refresh the workspace version and re-plan the edit |
| verification | `VER` | error; block completion | fix the failed check or explicitly report incomplete work |
| coordination | `CRD` | error; stop the affected task | resolve the dependency, lease, budget, or conflict |
| protocol | `PRT` | error; close the incompatible request/connection | use a supported version or capability set |
| provider | `PRV` | error; stop the turn unless a policy-approved retry exists | check credentials, quota, endpoint, or select an allowed provider |
| UX | `UIX` | warning when another interface remains usable; otherwise error | use the stated output mode or repair terminal/client capability |

The first condition reserved in each class is `<CLASS>-1000`; concrete implementations allocate codes sequentially and document them beside the owning type. Parse and validation failures must not be collapsed into `ARSY-EXE-*` merely because they surfaced during an operation.

## Halt and degradation rules

Continue with a degraded result only when all of these are true:

1. policy explicitly permits the reduced capability or omitted input;
2. integrity, confidentiality, authority, and requested sandbox assurance are unchanged;
3. the result identifies every known omission and sets `degraded: true`;
4. the omitted capability is not required by the user's acceptance criteria.

Otherwise halt the operation. Halt the session when durable event integrity, policy state, credential isolation, or required sandbox setup cannot be established. A timeout or transient provider failure may retry only when the operation is idempotent, retry policy is bounded, and each attempt is recorded.

## Policy denials

A policy denial uses an `ARSY-POL-*` code and always explains:

- the denied operation and canonical resource;
- the matched rule's stable ID, effect, and source location or administrator identity;
- the requested and granted capability/assurance levels;
- whether a narrower scope or explicit approval can satisfy the rule.

Secrets and raw policy documents are not copied into diagnostics. Human output summarizes the denial; JSON and CI output expose the same fields without terminal styling. A denial cannot be downgraded to a warning by an adapter, hook, repository file, model, or compatibility import.

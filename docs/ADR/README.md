# Architecture decision process

Architecture decision records (ADRs) document decisions that affect public contracts, trust or process boundaries, persistence, compatibility, or multiple subsystems.

## Propose

Copy the structure of an existing numbered ADR, choose the next unused four-digit number, set `Status: Proposed`, and open a pull request. The proposal must state context, decision, consequences, alternatives, and an invariant or verification gate.

## Decide

`@suiflex/maintainers`, the repository's CODEOWNERS, decide ADRs through pull-request review. An ADR becomes `Accepted` only when a maintainer approves and merges it. Material changes require a new review; merge authority is not delegated by repository content.

## Supersede

Accepted ADRs are historical records and are not rewritten to hide an earlier decision. A replacement ADR names the records it supersedes. After the replacement is accepted, update each replaced record to `Status: Superseded by ADR-NNNN` and link both directions.

The supported lifecycle is:

`Proposed` → `Accepted` → `Superseded by ADR-NNNN`

A rejected proposal is closed without merging and therefore does not become part of the accepted ADR set.

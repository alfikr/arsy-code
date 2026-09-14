# Workflows

All workflows here are active. They were previously parked as `*.yml.disabled`
while the repository was private, because Actions minutes on a private
repository are billed with a per-runner multiplier (Linux 1x, Windows 2x,
macOS 10x) and a 90-run sample cost roughly 2550 billed minutes against a
2000-3000 minute monthly allowance:

| Workflow | Runs | Wall min | Billed min |
|---|---|---|---|
| CI | 48 | 1375 | 2340 |
| Sandbox conformance | 21 | 41 | 142 |
| Performance | 16 | 70 | 70 |
| Fuzz | 5 | 1 | 1 |

The repository is public now, so that multiplier no longer applies, but the
shape of the cost is worth knowing before adding jobs: CI and Sandbox both run
a three-OS matrix on every push and pull request, only CI has a concurrency
group, and CI's `dependencies` job builds `cargo-deny` from source every run
with no cache.

## Release path

`release-please.yml` is the entry point. Dispatch it manually, or let it run
when a `release-please--*` pull request merges: it opens or updates the release
PR, and once one merges it calls `release.yml` and `npm-publish.yml` as
reusable workflows. Those two are referenced by path, so all three have to stay
enabled together.

`release.yml` builds six archives (macOS, Linux, and Windows on x86_64 and
aarch64), writes a `.sha256` beside each, signs an aggregate `SHA256SUMS` with
keyless Sigstore, attaches the installers and the CycloneDX SBOM, then pushes
the rendered formula and manifest to `suiflex/homebrew-tap` and
`suiflex/scoop-bucket`. `docs/34-distribution.md` documents how to verify the
result.

Secrets the release path needs, all already configured:

- `RELEASE_PLEASE_TOKEN` — a PAT, not `github.token`; pull requests opened with
  the default token don't trigger workflow events, so CI would never run on the
  release PR itself.
- `TAP_PUBLISH_TOKEN` — push access to the tap and bucket repositories.

npm publishes through Trusted Publishing (OIDC), so there is no `NPM_TOKEN`.
It requires this repository and `.github/workflows/npm-publish.yml` to be
registered as a Trusted Publisher for `@suiflex/arsy-code` on npmjs.com.

Prefer the `release-please` dispatch over pushing a tag by hand. A manual tag
push runs `release.yml`, whose completion fires `npm-publish.yml` through
`workflow_run`; doing that after a release-please run would publish twice.

## Local checks

The same checks run locally, and are what the contributing guide expects before
a push:

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo test --workspace --all-features --locked
    python3 fixtures/compat/check.py

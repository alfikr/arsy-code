# Disabled while the repository is private

Every workflow here is parked as `*.yml.disabled`. GitHub only reads `.yml`, so
none of them run — including manual dispatch, which needs the file to be visible
to Actions.

They were parked because Actions minutes on a private repository are billed with
a per-runner multiplier (Linux 1x, Windows 2x, macOS 10x), and the last 90 runs
cost roughly 2550 billed minutes against a 2000-3000 minute monthly allowance:

| Workflow | Runs | Wall min | Billed min |
|---|---|---|---|
| CI | 48 | 1375 | 2340 |
| Sandbox conformance | 21 | 41 | 142 |
| Performance | 16 | 70 | 70 |
| Fuzz | 5 | 1 | 1 |

Most of it is structural rather than slow: CI and Sandbox both run a
three-OS matrix on every push and pull request, so each push pays the macOS
multiplier twice, and CI's `dependencies` job builds `cargo-deny` from source
every run with no cache.

`release.yml` is the one to restore before cutting a release: it builds the
signed artifacts, and `docs/34-distribution.md` verifies them against
`.github/workflows/release.yml@refs/tags/` as the certificate identity. Tagging
while it is parked produces no artifacts and no signature.

`release-please.yml` and `npm-publish.yml` follow the same convention and
must be restored alongside it: `release-please.yml` opens/updates the release
PR and, once one merges, calls `release.yml` and `npm-publish.yml` as reusable
workflows to build and publish. `release-please.yml` also needs a
`RELEASE_PLEASE_TOKEN` repository secret (a PAT, not `github.token` — PRs
opened with the default token don't trigger workflow events) and npm Trusted
Publishing configured for `@suiflex/arsy-code` and each
`@suiflex/arsy-code-<platform>` package.

Re-enable one by dropping the suffix:

    git mv .github/workflows/ci.yml.disabled .github/workflows/ci.yml

The same checks run locally, and are what the contributing guide expects before
a push:

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo test --workspace --all-features --locked
    python3 fixtures/compat/check.py

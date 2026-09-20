# Workflows

All workflows here are active. CI and Sandbox each run a three-OS matrix on
every push and every pull request, only CI has a concurrency group, and CI's
`dependencies` job builds `cargo-deny` from source with no cache — worth
knowing before adding jobs, because that shape is where the minutes go.

CI compiles x86_64 Linux, macOS, and Windows x64. It does **not** compile
`aarch64-unknown-linux-gnu` or `aarch64-pc-windows-msvc`, which the release
builds: those two targets first meet a compiler during a release, and a
compile error there has already reached a tagged version once.

## Benchmark tiers

`benchmarks.yml` runs `arsy eval` in three cost tiers — smoke on every pull
request, comparison suites on pushes to main, and the isolated agent suites
nightly or on dispatch. The file carries the CI-minute estimate each tier was
enabled on; re-estimate before moving a suite into an earlier tier, because
a suite whose trials each cut a worktree is where the minutes go.

`arsy eval` exits nonzero when its gate blocks, and the gate blocks on a
safety regression — fewer refusals, more violations, more secret exposures
than the baseline — even when the success rate improved. No step parses the
report to decide whether the job failed.

## Release path

`release-please.yml` is the entry point, and it only prepares: dispatch it
manually, or let it run when a `release-please--*` pull request merges. Merging
that pull request tags the release, and **the tag push is what builds and
publishes** — `release.yml` runs on the tag, and `npm-publish.yml` runs on
`release.yml` completing.

release-please deliberately does not call those two itself. Doing so built
every target twice, and npm refuses a publish arriving through `workflow_call`
because the trusted publisher is registered against `npm-publish.yml` rather
than against whatever called it.

`release.yml` builds six archives (macOS, Linux, and Windows on x86_64 and
aarch64), writes a `.sha256` beside each, signs an aggregate `SHA256SUMS` with
keyless Sigstore, attaches the installers and one CycloneDX SBOM per crate,
then pushes the rendered formula and manifest to `suiflex/homebrew-tap` and
`suiflex/scoop-bucket`. The signature ships as `SHA256SUMS.bundle`; cosign
writes no separate `.sig` alongside a bundle. `docs/34-distribution.md`
documents how to verify the result.

A `release.yml` that fails still leaves a published, asset-less GitHub Release
behind, because release-please creates the release when its pull request
merges. Fix forward and re-run rather than assuming the tag is unused.

Secrets the release path needs, all already configured:

- `RELEASE_PLEASE_TOKEN` — a PAT, not `github.token`; pull requests opened with
  the default token don't trigger workflow events, so CI would never run on the
  release PR itself.
- `TAP_PUBLISH_TOKEN` — push access to the tap and bucket repositories.

npm publishes through Trusted Publishing (OIDC), so there is no `NPM_TOKEN`.
It requires this repository and `.github/workflows/npm-publish.yml` to be
registered as a Trusted Publisher for `@suiflex/arsy-code` on npmjs.com, with
the `Release` environment — the publish job declares it, and npm rejects a
token whose environment claim does not match the registration.

Prefer the `release-please` dispatch over pushing a tag by hand: a hand-pushed
tag builds and publishes the same way, but without the version bump, changelog,
and contributor attribution that precede it.

## Local checks

The same checks run locally, and are what the contributing guide expects before
a push:

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo test --workspace --all-features --locked
    python3 fixtures/compat/check.py

# Distribution, installation, and updates

## Channels

GitHub Releases is the canonical channel. Each stable SemVer tag publishes one archive per supported target, `SHA256SUMS`, an SPDX SBOM, and Sigstore signatures and bundles. This keeps rollback possible without a privileged installer and gives every wrapper one immutable source of bytes.

The supported convenience channels are a SuiFlex Homebrew tap for macOS/Linux, a SuiFlex Scoop bucket for Windows, and the public npm package `@suiflex/arsy-code`. Their manifests or native packages must reference an exact GitHub Release artifact and its SHA-256 digest; they never rebuild or mirror binaries. Cargo, unattended self-update, `curl | sh`, OS stores, and third-party package repositories remain out of scope until demand and signing automation justify them.

The npm package is a thin launcher with platform-filtered optional dependencies:

| npm package | Target |
|---|---|
| `@suiflex/arsy-code-darwin-arm64` | `aarch64-apple-darwin` |
| `@suiflex/arsy-code-darwin-x64` | `x86_64-apple-darwin` |
| `@suiflex/arsy-code-linux-arm64-gnu` | `aarch64-unknown-linux-gnu` |
| `@suiflex/arsy-code-linux-x64-gnu` | `x86_64-unknown-linux-gnu` |
| `@suiflex/arsy-code-win32-x64-msvc` | `x86_64-pc-windows-msvc` |

## Supported targets

| Operating system | Architecture | Rust target | Support |
|---|---|---|---|
| Ubuntu 22.04+ / glibc Linux | x86-64 | `x86_64-unknown-linux-gnu` | Tier 1 |
| Ubuntu 22.04+ / glibc Linux | ARM64 | `aarch64-unknown-linux-gnu` | Tier 1 |
| macOS 13+ | Intel x86-64 | `x86_64-apple-darwin` | Tier 1 |
| macOS 13+ | Apple silicon | `aarch64-apple-darwin` | Tier 1 |
| Windows 10 22H2+ | x86-64 | `x86_64-pc-windows-msvc` | Tier 1 |

Tier 1 means native CI build and test, signed release artifacts, and security fixes. Other OS/architecture combinations are unsupported until native CI and sandbox conformance exist; WSL does not establish Windows support.

## Install and verify

For the canonical channel, download the archive, `SHA256SUMS`, matching `.sig`, and matching `.bundle` from the same release. Verify the digest before extraction, then verify `SHA256SUMS` with Sigstore while pinning the `suiflex/arsy-code` release-workflow identity and GitHub Actions OIDC issuer. A digest or signature mismatch is fatal; the installer must not offer an override.

```console
sha256sum --check --ignore-missing SHA256SUMS
cosign verify-blob SHA256SUMS --signature SHA256SUMS.sig --bundle SHA256SUMS.bundle \
  --certificate-identity-regexp '^https://github.com/suiflex/arsy-code/.github/workflows/release.yml@refs/tags/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

On macOS, use `shasum -a 256 -c SHA256SUMS` instead of `sha256sum`. On Windows, Scoop validates the manifest's pinned SHA-256 before installation. Homebrew likewise validates the formula's pinned `sha256`; both manifests are updated only after the canonical signature check passes in release automation.

For npm, install the public launcher package globally:

```console
npm install --global @suiflex/arsy-code
arsy doctor
```

npm installs one platform-filtered native package as an optional dependency. The
launcher supports only the Tier 1 targets listed above; on an unsupported OS or
architecture, it exits with an explicit diagnostic.

Release automation stages each native package from the matching Cargo binary:

```console
npm run stage:platform -- \
  --target <rust-target> \
  --binary <release-binary> \
  --version <semver> \
  --output npm/platforms/<package-name>
```

Publish all platform packages before publishing `@suiflex/arsy-code`, so npm
can resolve the launcher's optional dependencies.


After extraction or package-manager installation, run `arsy doctor`. It reports the version, target, config paths, sandbox assurance, and release provenance without sending telemetry.

## Update and rollback

ARSY does not self-update. This avoids giving the runtime a permanent write-and-network path.

| Channel | Update | Rollback |
|---|---|---|
| GitHub Releases | verify and replace with a newer archive | verify and replace with any retained older stable archive |
| Homebrew tap | `brew update && brew upgrade arsy-code` | install the tap's versioned formula; if unavailable, use the canonical archive |
| Scoop bucket | `scoop update arsy-code` | `scoop reset arsy-code@<version>`; if unavailable, use the canonical archive |

Before replacement, stop active sessions cleanly. Config and session migrations require an explicit backup and dry run as defined by the roadmap; installation never silently rewrites them. A failed health check restores the previous binary, while data rollback follows the migration's own loss report and rollback guidance. Release artifacts and manifests are immutable after publication; a bad release is superseded, not replaced in place.

## Release gate

A release is publishable only when all Tier 1 native jobs pass, the lockfile and license policy pass, SBOM and checksums are generated from the final bytes, and signature verification succeeds in a clean job. Package manifests are downstream of that gate and must resolve to the same digests.

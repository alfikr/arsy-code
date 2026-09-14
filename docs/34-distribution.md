# Distribution, installation, and updates

## Channels

GitHub Releases is the target canonical channel. The operational gate is not
met at the audited revision: every workflow is parked as `*.yml.disabled`, and
repository source alone does not prove that any channel has published
successfully.

The disabled release workflow currently describes five native archives, a
CycloneDX JSON SBOM, and one `.sha256` file per archive. It does not create a
canonical `SHA256SUMS`, an SPDX SBOM, Sigstore signatures/bundles, or release
copies of `install.sh` and `install.ps1`. It also builds with Rust 1.85 while
the workspace declares 1.98. Those discrepancies must be fixed and verified
before the target channel below is described as operational.

At the release gate, each stable SemVer tag publishes one archive per supported
target, a canonical checksum manifest, an explicitly selected SBOM format, and
Sigstore signatures and bundles. This keeps rollback possible without a
privileged installer and gives every wrapper one immutable source of bytes.

Target convenience channels are a SuiFlex Homebrew tap for macOS/Linux, a
SuiFlex Scoop bucket for Windows, the npm package `@suiflex/arsy-code`, and
`install.sh` / `install.ps1` published with each GitHub Release. Their source or
documentation exists, but enabled publication and production availability are
not established by this repository. Once operational, all reference an exact
GitHub Release artifact and its SHA-256 digest; none rebuild or mirror binaries.
Cargo, unattended self-update, OS stores, and third-party repositories remain
out of scope until demand justifies them.

`install.sh` and `install.ps1` implement the interim checksum-only client path:
they verify an archive SHA-256 but not the future Sigstore signature. The
current release workflow does not attach those scripts, so their presence in
source is not an operational download channel. They are suitable for local/dev
testing against explicitly supplied assets; unattended provisioning waits for
the release gate.

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

Tier 1 is the target support contract: native CI build/test, signed release
artifacts, and security fixes. No target reaches that operational status while
workflows are disabled and signing is absent. Other combinations remain
unsupported until native CI and sandbox conformance exist; WSL does not
establish Windows support.

## Target install and verification flow

After the release gate is met, download the archive, `SHA256SUMS`, matching
`.sig`, and matching `.bundle` from the same release. Verify the digest before
extraction, then verify `SHA256SUMS` with Sigstore while pinning the
`suiflex/arsy-code` release-workflow identity and GitHub Actions OIDC issuer. A
digest or signature mismatch is fatal; the installer must not offer an override.
The commands below describe this target and do not validate current published
assets.

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

For curl / PowerShell, install directly from the latest release:

```console
curl -fsSL https://github.com/suiflex/arsy-code/releases/latest/download/install.sh | sh
```

```powershell
irm https://github.com/suiflex/arsy-code/releases/latest/download/install.ps1 | iex
```

Both scripts resolve the platform-specific archive (`arsy-<os>-<arch>.tar.gz`
or `.zip`), verify its `.sha256` file, and install to `~/.local/bin` (Unix) or
`%LOCALAPPDATA%\ArsyCode\bin` (Windows). `ARSY_VERSION` pins a specific tag
instead of `latest`; `ARSY_INSTALL_DIR` overrides the install directory.
`tests/install_test.sh` is the self-check for `install.sh`; `release.yml`'s
`verify-installers` job runs it plus a PowerShell parse of `install.ps1`
before any platform build.

After extraction or package-manager installation, run `arsy doctor`. It reports the version, target, config paths, sandbox assurance, and release provenance without sending telemetry.

## Update and rollback

ARSY does not self-update. This avoids giving the runtime a permanent write-and-network path.

| Channel | Update | Rollback |
|---|---|---|
| GitHub Releases | verify and replace with a newer archive | verify and replace with any retained older stable archive |
| Homebrew tap | `brew update && brew upgrade arsy-code` | install the tap's versioned formula; if unavailable, use the canonical archive |
| `install.sh` / `install.ps1` | re-run the script | re-run with `ARSY_VERSION` pinned to the older tag |
| Scoop bucket | `scoop update arsy-code` | `scoop reset arsy-code@<version>`; if unavailable, use the canonical archive |

Before replacement, stop active sessions cleanly. Config and session migrations require an explicit backup and dry run as defined by the roadmap; installation never silently rewrites them. A failed health check restores the previous binary, while data rollback follows the migration's own loss report and rollback guidance. Release artifacts and manifests are immutable after publication; a bad release is superseded, not replaced in place.

## Release gate

A release is publishable only when all Tier 1 native jobs pass, the lockfile and license policy pass, SBOM and checksums are generated from the final bytes, and signature verification succeeds in a clean job. Package manifests are downstream of that gate and must resolve to the same digests.

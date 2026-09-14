# Distribution, installation, and updates

## Channels

GitHub Releases is the canonical channel. Each stable SemVer tag publishes one archive per supported target, a `.sha256` sidecar beside each, an aggregate `SHA256SUMS`, a Sigstore bundle signing it, a CycloneDX SBOM per crate, and `install.sh` / `install.ps1`. This keeps rollback possible without a privileged installer and gives every wrapper one immutable source of bytes.

The supported convenience channels are a SuiFlex Homebrew tap for macOS/Linux, a SuiFlex Scoop bucket for Windows, the public npm package `@suiflex/arsy-code`, and `install.sh` / `install.ps1` published alongside each GitHub Release. All of them reference an exact GitHub Release artifact and its SHA-256 digest; none rebuild or mirror binaries. Cargo, unattended self-update, OS stores, and third-party package repositories remain out of scope until demand justifies them.

`install.sh` and `install.ps1` are a checksum-only channel: they verify the archive's SHA-256 digest against its `.sha256` sidecar, but not the Sigstore signature described below. Release automation signs `SHA256SUMS`, so a caller that wants provenance verifies that file itself rather than relying on the scripts.

The npm package carries no binary. Its postinstall script resolves the host
platform, downloads that archive from the release matching the package version,
verifies the digest against the published `.sha256`, and unpacks it into the
package. An unsupported platform fails the install with an explicit diagnostic
rather than installing a launcher that cannot run.

## Supported targets

| Operating system | Architecture | Rust target | Support |
|---|---|---|---|
| Ubuntu 22.04+ / glibc Linux | x86-64 | `x86_64-unknown-linux-gnu` | Tier 1 |
| Ubuntu 22.04+ / glibc Linux | ARM64 | `aarch64-unknown-linux-gnu` | Tier 1 |
| macOS 13+ | Intel x86-64 | `x86_64-apple-darwin` | Tier 1 |
| macOS 13+ | Apple silicon | `aarch64-apple-darwin` | Tier 1 |
| Windows 10 22H2+ | x86-64 | `x86_64-pc-windows-msvc` | Tier 1 |
| Windows 11 22H2+ | ARM64 | `aarch64-pc-windows-msvc` | Tier 1 |

Tier 1 means native CI build and test, signed release artifacts, and security fixes. Other OS/architecture combinations are unsupported until native CI and sandbox conformance exist; WSL does not establish Windows support.

## Install and verify

For the canonical channel, download the archive, `SHA256SUMS`, and `SHA256SUMS.bundle` from the same release. Verify the digest before extraction, then verify `SHA256SUMS` with Sigstore while pinning the `suiflex/arsy-code` release-workflow identity and GitHub Actions OIDC issuer. A digest or signature mismatch is fatal; the installer must not offer an override.

```console
sha256sum --check --ignore-missing SHA256SUMS
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.bundle \
  --certificate-identity-regexp '^https://github.com/suiflex/arsy-code/.github/workflows/release.yml@refs/tags/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The bundle carries the signature, the certificate, and the transparency-log
entry together, so there is no separate `.sig` to fetch. The certificate names
the workflow and the ref it ran from — a release is built by the tag push, so
that ref is `refs/tags/<tag>`, which is what the regexp above pins. `cosign
verify-blob` prints the identity it found when a match fails.

On macOS, use `shasum -a 256 -c SHA256SUMS` instead of `sha256sum`. On Windows, Scoop validates the manifest's pinned SHA-256 before installation. Homebrew likewise validates the formula's pinned `sha256`. Both manifests are rendered from this repository's `packaging/` templates and pushed only after the GitHub Release publishes, and both pin digests taken from that same build.

For npm, install the public launcher package globally:

```console
npm install --global @suiflex/arsy-code
arsy doctor
```

The install needs network access at install time and honours the Tier 1 targets
listed above. Because the archive comes from the release tagged `v<package
version>`, the npm package can only be published after that release exists;
`npm-publish.yml` is gated on `release.yml` completing for exactly that reason.

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

After extraction or package-manager installation, run `arsy doctor`. It reports the version, target, config paths, storage, and the resolved provider and credential without sending telemetry. It does not check the release it came from; verify provenance with the bundle as above.

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

A release is publishable only when all Tier 1 native jobs pass and the SBOM and checksums are generated from the final bytes: `publish` waits on `build`, `sbom`, and `checksums`. The lockfile and licence policy are enforced by `ci.yml` on the way to `main` rather than by the release itself, so a tag pushed past a red `main` would not be stopped here. `SHA256SUMS` is verified with `sha256sum --check` against those bytes before it is signed. Package manifests are downstream of that gate: the tap and bucket jobs run only after the release publishes, and pin digests from the same build.

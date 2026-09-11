# Contributing to ARSY CODE

Thanks for your interest in ARSY CODE — an agentic coding assistant designed
to pair program with developers, built with Rust.

This guide covers how to build, test, and submit changes. By participating you
agree to abide by our [Code of Conduct](CODE_OF_CONDUCT.md).

## Quick links

- **Security policy** — [SECURITY.md](SECURITY.md)
- **Code of Conduct** — [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
- **Contributor License Agreement** — [CLA.md](CLA.md)
- **Bugs & feature requests** — [Issues](https://github.com/suiflex/arsy-code/issues/new/choose)
- **Questions & setup help** — [Discussions](https://github.com/suiflex/arsy-code/discussions)
- **Architecture Decision Records** — [docs/ADR](docs/ADR/README.md)

## Contributor License Agreement (CLA)

Before your first pull request can be merged, you must sign our
[Contributor License Agreement](CLA.md). It is a one-time step: when you open
your first PR, the CLA bot comments with instructions and you sign by replying
with a single comment on the PR:

```text
I have read the CLA Document and I hereby sign the CLA
```

Your signature is recorded on the `cla-signatures` branch and covers all future
contributions. The PR carries a `cla: signed` or `cla: not signed` label
showing where it stands.

The CLA keeps the project's licensing flexible (see [CLA.md](CLA.md) § 4)
while guaranteeing your contributions always remain available under the
[MIT License](LICENSE).

## How to contribute

Start here, before opening anything:

1. **Bug or small fix** → open a pull request directly.
2. **New capability, public protocol change, or architectural change** → open an
   [issue](https://github.com/suiflex/arsy-code/issues/new/choose) first. Agreeing
   on the design and security boundaries up front saves a rewrite.
3. **Question, setup trouble, or "is this a bug?"** →
   [Discussions](https://github.com/suiflex/arsy-code/discussions), not an issue.
4. **Security vulnerability** → **do not** open a public issue. Follow
   [SECURITY.md](SECURITY.md).

## Getting started

ARSY CODE is a Cargo workspace:

- `crates/arsy-cli/` — interactive terminal CLI & TUI (`arsy`).
- `crates/arsy-kernel/` — agent runtime engine, providers, storage, tools, and
  credentials.
- `crates/arsy-sandbox/` — process isolation and sandboxing (`landlock`,
  `seccomp`).
- `crates/arsy-ide/` — IDE integration protocol.
- `crates/arsy-code/` — top-level entrypoint and orchestration.

### Prerequisites and tools

1. Install a stable Rust toolchain (1.98+), then components:

   ```bash
   rustup component add rustfmt clippy
   ```

2. Verify the workspace builds:

   ```bash
   cargo check --workspace
   ```

## Build, lint, and test

Run these checks before opening a pull request:

```bash
# Format check
cargo fmt --all -- --check

# Linter (clippy with all warnings treated as errors)
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run all workspace unit and integration tests
cargo test --workspace
```

A root `Makefile` also provides shortcuts for standard workflows (`make help`,
`make test`, `make fmt-check`, `make lint`).

## Commit conventions

We follow [Conventional Commits](https://www.conventionalcommits.org/); the
`release-please` workflow parses them to drive changelogs and version bumps.

- Types: `feat`, `fix`, `perf`, `chore`, `refactor`, `docs`, `test`, `build`,
  `ci`.
- Use `feat(<scope>): ...` for new user-facing features or capabilities.
- Use `fix(<scope>): ...` for bug fixes.
- Subject line ≤ 72 chars, imperative mood, no trailing period.
- Wrap the body at 72 chars and explain **why** the change exists — the diff
  already shows the what.
- One logical change per commit. Each commit should leave the tree in a
  buildable, testable state so `git revert` stays safe.

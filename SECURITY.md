# Security Policy

## Supported versions

ARSY CODE is pre-1.0 and ships from the `arsy` binary (`crates/arsy-cli`). Security
fixes land on the latest release only; there are no long-term support branches yet.

| Version              | Supported          |
| -------------------- | ------------------ |
| Latest `0.x` release | :white_check_mark: |
| Any older release    | :x:                |

The current release is on the
[Releases page](https://github.com/suiflex/arsy-code/releases). Please upgrade to
the latest release before reporting an issue where possible.

## Reporting a vulnerability

**Do not report security vulnerabilities through public GitHub issues,
discussions, or pull requests.**

Instead, use GitHub's private vulnerability reporting:

1. Go to the [Security tab](https://github.com/suiflex/arsy-code/security/advisories/new).
2. Click **Report a vulnerability** and fill out the advisory form.

If you cannot use private reporting, contact the maintainers (**@suiflex**) and
ask for a private channel before sharing any details.

Please include, where you can:

- The affected component (`arsy-cli`, `arsy-kernel`, `arsy-sandbox`, or
  `arsy-ide`) and version.
- A description of the vulnerability and its impact.
- Steps to reproduce, a proof of concept, or the relevant configuration.
- Any suggested remediation.

## What to expect

- **Acknowledgement** within 5 business days.
- An initial assessment and severity triage shortly after.
- Progress updates as we work on a fix, and coordination on a disclosure
  timeline. We aim to release a fix before any public disclosure.
- Credit for the reporter in the advisory, unless you prefer to remain
  anonymous.

## Scope

ARSY CODE runs an AI agent loop with local OS execution, tool sandboxing
(`landlock`, `seccomp`), and secure credential storage in the OS keychain and
filesystem (`0600`). Reports touching sandbox escape, unauthorized privilege
escalation, credential leakage, or execution of arbitrary code beyond configured
policy limits are especially valued.

Thank you for helping keep ARSY CODE and its users safe.

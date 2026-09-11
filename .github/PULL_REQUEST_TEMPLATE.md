<!--
Title: conventional-commits style, ≤ 70 chars, no trailing period.
e.g. feat(cli): improve interactive tui responsive rendering
e.g. fix(kernel): preserve file permissions during credential store write
Keep the PR focused — one logical change is easier to review and revert.
-->

## Summary

<!-- 1–3 bullets on the WHY: the problem this solves or the need it fills. -->

-

## Changes

<!-- What actually changed, grouped by area (cli / kernel / sandbox / ide / code / ci / docs). -->

-

## Test plan

<!-- Check what you ran; leave unchecked what still needs doing. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Manual verification steps (describe them):

## First pull request?

- [ ] I have signed the
      [Contributor License Agreement](https://github.com/suiflex/arsy-code/blob/main/CLA.md)
      — the bot will comment below with the one-line reply that signs it.

## Notes for reviewers

<!-- Optional: trade-offs, follow-ups, anything risky. -->

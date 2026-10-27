## Summary

<!-- What changed, and why? -->

## Related issue

<!-- Link the issue, or write "none". -->

## Verification

<!--
List the commands you ran and any manual checks. For visible UI changes,
include a screenshot or a short recording.
-->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo nextest run` (or `cargo test --all`)
- [ ] `cd web && bun run check` (web changes)
- [ ] `cd web && bun run e2e` (web changes)

## Checklist

- [ ] I read [CONTRIBUTING.md](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md).
- [ ] I did not edit anything under `fixtures/`.
- [ ] New behavior has a test that fails without this change.
- [ ] User-facing surfaces say `alforria`; compatibility surfaces keep their
      upstream identifiers.
- [ ] No secrets, API keys, or transcript contents are included in the diff,
      logs, or screenshots.

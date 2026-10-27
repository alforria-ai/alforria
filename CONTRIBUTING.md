# Contributing to alforria

Thanks for taking the time to contribute. This document covers the local
setup, the quality gates, and the conventions that keep alforria compatible
with the upstream wire format.

## Before you start

- Read [AGENTS.md](AGENTS.md) for the workspace layout and project rules.
- Search existing [issues](https://github.com/alforria-ai/alforria/issues) and
  pull requests before opening a new one.
- For security issues, do **not** open a public issue. Follow
  [SECURITY.md](SECURITY.md).
- For large features or refactors, open an issue first so the approach can be
  discussed.

## Development setup

### Rust workspace

Requirements:

- a stable Rust toolchain with `cargo`, `rustfmt`, and `clippy`;
- a C compiler and `pkg-config`;
- OpenSSL development headers for your target;
- git;
- a POSIX shell for the `bash` tool.

```sh
cargo build
cargo nextest run          # preferred; fallback: cargo test --all
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

Install `cargo-nextest` once with `cargo install cargo-nextest --locked`.

### Web client

The web client lives in `web/` and uses bun, Vite, SolidJS, and TypeScript.

```sh
cd web
bun install
bun run check              # prettier + tsc + vitest
bun run server             # isolated server + scripted model on :4697
bun run dev                # vite on :4610
bun run e2e                # headless browser suite
```

The isolated dev server keeps all of its state under `web/.dev/`, so it never
touches your real alforria data. Rebuild the embedded bundle only when the web
client changed:

```sh
cd web
bun run build && bun run pack
```

Commit the updated `crates/alforria-ui/assets/ui.tar.zst` with the source
change.

## Quality gates

Every pull request should pass:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo nextest run
```

For web changes, also run `bun run check` and `bun run e2e` from `web/`.
CI runs the Rust gates on Linux, macOS, and Windows; a few POSIX-specific
core and PTY tests are deferred on Windows.

## Compatibility rules

alforria is wire-compatible with the pinned opencode reference:

- JSON field names, union `type` tags, and optional-vs-null semantics must
  match the fixtures.
- User-facing surfaces use the `alforria` name. Upstream `opencode` identity
  stays on compatibility surfaces: `OPENCODE_*` environment variables,
  `opencode.json`, XDG paths, `opencode.db`, HTTP headers, and provider ids.
- **Never edit anything under `fixtures/`.** Those files are frozen test
  vectors. If a fixture looks wrong, explain why in the issue instead of
  changing it.
- New behavior needs a test that fails without the change. Prefer the existing
  harnesses (wire scenarios, CLI e2e, TUI `TestBackend`, web fixtures) over a
  new bespoke setup.
- When in doubt, the pinned TypeScript source documented in
  `fixtures/PINNED.md` is authoritative.

## Code style

- Match the surrounding code. Follow existing module structure and naming.
- Do not add comments unless they explain a non-obvious constraint or a
  compatibility quirk.
- Keep public error text actionable: what failed, and what to try next.
- Avoid new dependencies unless they are clearly justified. Check the existing
  workspace dependencies first.
- Do not log secrets, API keys, tokens, or transcript contents.

## Commits and pull requests

- Keep commits focused and use an imperative summary, for example
  `fix(server): never empty an oversized buffered response body`.
- Reference the issue in the pull request description, not in every commit.
- Describe what changed, why, and how you verified it. Include the commands
  you ran.
- Add screenshots or a short recording for visible UI changes.
- Do not force-push a pull request after review has started unless a
  maintainer asks you to.
- By contributing, you agree that your contribution is licensed under the
  repository's MIT license.

## Release process

Releases are cut from `main`. The full checklist is in
[docs/RELEASING.md](docs/RELEASING.md). In short:

1. Update the workspace version in `Cargo.toml` and `CHANGELOG.md`.
2. Merge the release commit to `main`.
3. Draft a GitHub Release with human-written notes.
4. Push the matching `vX.Y.Z` tag. The release workflow verifies the version,
   runs the test suite on all three platforms, builds the binaries, and
   attaches them with SHA-256 sidecars.

The tag must point at a commit on the default branch, and the GitHub Release
must exist before the workflow will upload assets.

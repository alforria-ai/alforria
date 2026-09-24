# AGENTS.md — alforria

**alforria** — a Rust port of [opencode](https://github.com/anomalyco/opencode),
released by LibertAI. Pinned TS reference commit:
`88c6c7abc7f320b6aabed2634ac0b2d6e6ecea67` (see `fixtures/PINNED.md` and
`docs/plans/opencode-rust-port.md`).

## Workspace layout

```
crates/alforria-schema   # wire DTOs & events (byte-parity with TS fixtures)
crates/alforria-llm      # LLM protocols, LLMEvent stream, usage/cost, retries
crates/alforria-core     # session engine, tools, config, storage, bus, catalog
crates/alforria-server   # v1+v2 HTTP API, SSE, PTY websockets, auth
crates/alforria-tui      # ratatui terminal UI (HTTP+SSE client of the server)
crates/alforria-ui       # embedded web UI bundle (built in alforria-ai/web)
crates/alforria          # CLI binary
fixtures/                # FROZEN golden fixtures from the TS repo — never edit
docs/plans/              # approved plans
```

## Commands

```sh
cargo build                             # build everything
cargo fmt --all                         # format
cargo fmt --all --check                 # format check (CI enforced)
cargo clippy --all-targets -- -D warnings   # lint (CI enforced)
cargo nextest run                       # tests (fallback: cargo test --all)
cargo test -p <crate>                   # tests for one crate
```

Web UI regeneration (source lives in `alforria-ai/web`, a fork of opencode's
`packages/app` — see `docs/plans/web-ui.md`):

```sh
# in alforria-ai/web
bun run build && bun run pack            # -> ui.tar.zst
# in alforria
cp <web>/ui.tar.zst crates/alforria-ui/assets/ui.tar.zst
```

## Conventions

- Wire compatibility is the law: every JSON shape emitted by this codebase must match
  the TS reference (field names, union `type` tagging, optional-vs-null). Verify against
  `fixtures/`. When in doubt, the TS source (`/tmp/opencode-src` clone or the frozen
  copies in `fixtures/`) is authoritative.
- Never edit anything under `fixtures/` — it is frozen test-vector material.
- Branding: user-facing surfaces say `alforria` (binary, crate names, CLI help,
  TUI wordmark, serve handshake, default auth username). Wire-compat surfaces
  keep the upstream `opencode` identity: `OPENCODE_*` env vars, `opencode.json`
  config discovery, XDG paths (`~/.config/opencode`), `opencode.db`, HTTP
  headers (`x-opencode-*`), provider ids (`opencode`, `opencode-go`), and the
  `opencode` theme asset.
- Chunks/commits follow the chunked-factory discipline: one commit per milestone,
  `fixup:` commits for review fixes, `steer:` commits from steering gates.

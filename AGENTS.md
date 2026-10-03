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
crates/alforria-ui       # embedded web UI bundle (built from web/)
web/                     # web client source (bun + Vite + SolidJS); web/prototype is the design reference
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

Web client (source in `web/` — see `docs/plans/web-client.md`, design in `DESIGN.md`):

```sh
cd web
bun install
bun run server          # isolated alforria serve + scripted model on :4697 (never touches your data)
bun run dev             # vite on :4610 proxying to it
bun run check           # prettier + tsc + vitest
bun run build && bun run pack   # -> crates/alforria-ui/assets/ui.tar.zst (commit it)
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

# AGENTS.md — opencode-rs

Rust port of [opencode](https://github.com/anomalyco/opencode). Pinned TS reference commit:
`88c6c7abc7f320b6aabed2634ac0b2d6e6ecea67` (see `fixtures/PINNED.md` and
`docs/plans/opencode-rust-port.md`).

## Workspace layout

```
crates/opencode-schema   # wire DTOs & events (byte-parity with TS fixtures)
crates/opencode-llm      # LLM protocols, LLMEvent stream, usage/cost, retries
crates/opencode-core     # session engine, tools, config, storage, bus, catalog
crates/opencode-server   # v1+v2 HTTP API, SSE, PTY websockets, auth
crates/opencode-tui      # ratatui terminal UI (HTTP+SSE client of the server)
crates/opencode          # CLI binary
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

## Conventions

- Wire compatibility is the law: every JSON shape emitted by this codebase must match
  the TS reference (field names, union `type` tagging, optional-vs-null). Verify against
  `fixtures/`. When in doubt, the TS source (`/tmp/opencode-src` clone or the frozen
  copies in `fixtures/`) is authoritative.
- Never edit anything under `fixtures/` — it is frozen test-vector material.
- Chunks/commits follow the chunked-factory discipline: one commit per milestone,
  `fixup:` commits for review fixes, `steer:` commits from steering gates.

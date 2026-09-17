# opencode → Rust: Complete Port Plan

**Status:** Approved
**Date:** 2026-09-17
**Reference TS commit (pinned):** `88c6c7abc7f320b6aabed2634ac0b2d6e6ecea67` (`anomalyco/opencode` dev branch, v1.18.31)
**Local TS clone for reference:** `/tmp/opencode-src` (shallow clone of the above)

## 1. Goal

Port opencode to Rust, **totally and completely**, fully tested end to end:

- **Wire & behavior compatible**: same HTTP API (v1 unprefixed + v2 `/api`), same SSE event
  vocabulary, same `opencode.json` config, same SQLite session storage layout, same tool
  semantics, same `opencode serve` stdout handshake — so existing SolidJS app, desktop, SDK,
  and session-ui keep working unchanged against the Rust binary.
- **E2E tested**: golden fixtures + mock-LLM deterministic suites + live LibertAI model suites.

## 2. Scope

Porting target (~125k LOC of TypeScript that implements the product):

| TS package | LOC | Rust destination |
|---|---|---|
| `packages/opencode` (core agent/CLI/server) | 81k | `opencode-core`, `opencode-server`, `opencode` bin |
| `packages/core` (LLM abstractions, session runtime) | 33k | `opencode-core` |
| `packages/llm` (native LLM client, 6 protocols) | 9.5k | `opencode-llm` |
| `packages/schema` (wire DTOs/events) | 3.4k | `opencode-schema` |
| `packages/protocol` + `packages/server` (v2 API) | ~3k | `opencode-server` |
| `packages/tui` | 27k | `opencode-tui` (ratatui) |

**Remain TS clients over the wire** (confirmed hybrid scope): `app`, `desktop`, `session-ui`,
`ui`, `web`, `sdk` — they work unchanged via the wire contract.

## 3. Workspace layout

```
opencode-rs/
├── crates/
│   ├── opencode-schema      # wire DTOs, events, JSON shapes (serde, byte-parity)
│   ├── opencode-llm         # protocols, LLMEvent stream, usage/cost, cache policy, retries
│   ├── opencode-core        # session runner, event bus, projector, catalog, tools, permission, config
│   ├── opencode-server      # v1+v2 HTTP, SSE, PTY WS, auth, OpenAPI export
│   ├── opencode-tui         # ratatui, pure HTTP+SSE client of the server
│   └── opencode/            # binary: clap CLI (run, serve, tui, attach, acp, mcp, ...)
├── fixtures/                # frozen golden fixtures extracted from the TS repo
└── docs/plans/
```

**Stack:** tokio, axum, reqwest + eventsource-stream, serde + jsonc, rusqlite (schema matching
drizzle), `rmcp` (MCP), tree-sitter (bash parsing for permissions), ripgrep-as-library,
ratatui + crossterm, rust_decimal, ulid, notify, globset, gray_matter, mdns-sd,
opentelemetry, octocrab, tokio-tungstenite.

## 4. Wire-compatibility strategy

- Freeze golden fixtures from TS **before** writing Rust: OpenAPI JSON
  (`packages/sdk/openapi.json`), schema/event manifests, recorded LLM protocol fixtures
  (`packages/llm/test/fixtures/recordings/`, ~988K).
- Conformance rule: every JSON shape the Rust binary emits must match the TS shape
  (field names, union `type` tagging, optional-vs-null) — verified by schema-diff tests
  against frozen fixtures.
- Implement both API generations: v1 unprefixed routes (~124 endpoints) and v2 `/api/*`
  groups from `packages/protocol`.

## 5. Milestones (factory-executed, one commit each)

| # | Milestone | Key deliverable | Acceptance check |
|---|---|---|---|
| 0 | Workspace + fixtures | Cargo workspace, CI, frozen TS fixtures | `cargo test` green; fixture extraction reproducible |
| 1 | `opencode-schema` | All DTOs/events, golden JSON round-trip | byte-identical serialization vs fixtures |
| 2 | `opencode-llm` protocols | openai-chat, openai-compatible-chat, openai-responses, anthropic-messages, gemini, bedrock-converse; LLMEvent stream; usage invariants; retries; cache policy | golden SSE-fixture replay tests; usage property tests |
| 3 | Config + catalog + storage | JSONC config loader (precedence chain), models.dev catalog, SQLite storage (drizzle-compatible), EventV2 bus | config precedence tests; DB schema diff vs TS migrations |
| 4 | Tools (all 17) | bash, edit (fuzzy-replacer chain), read, write, grep, glob, task, todowrite, skill, question, webfetch, websearch, lsp, apply_patch, plan_exit, execute, invalid + truncation | per-tool unit + integration matrix |
| 5 | Session engine | Agent loop, processor (stream→parts), compaction, revert/snapshot, permissions, subagents, retry, doom-loop detection, cost accounting | mock-LLM full-loop E2E |
| 6 | Server | v1+v2 HTTP, SSE stream, PTY WS, Basic auth + tickets, OpenAPI parity | frozen-fixture wire tests + OpenAPI diff |
| 7 | MCP, LSP, ACP, supporting | MCP client (stdio/HTTP/SSE, OAuth), LSP lifecycle, ACP adapter, git/snapshot/worktree, skills, plugins, share, ide, sync | mock MCP/LSP integration tests |
| 8 | CLI | Full command surface incl. `run` (JSON event stream, exit codes), `serve` handshake | CLI e2e suite |
| 9 | TUI | ratatui chat view, markdown/diff rendering, ~25 dialogs, declarative keymap, 30+ themes, sync-reducer over SSE | headless vt100 snapshot tests |
| 10 | E2E: mock LLM | Scripted mock provider; full-agent E2E | deterministic E2E green |
| 11 | E2E: live LibertAI | Same suite on LibertAI OpenAI-compatible endpoint (qwen3.5-4b cheap runs, glm-5.3 / deepseek-v4.1 quality runs) | live suite green behind `OPENCODE_E2E_LIVE=1` |
| 12 | Cross-parity harness | TS + Rust side-by-side on scripted tasks; normalized diff of API responses and event streams | parity report; deviations → 0 |

## 6. Testing pyramid

1. Unit — every module; property tests for fuzzy-edit chain, usage invariants, retry math.
2. Golden — recorded SSE streams per protocol, JSON shapes, DB migrations.
3. Wire — frozen-fixture conformance + OpenAPI diff + SSE replay.
4. E2E mock-LLM — deterministic agent-loop coverage.
5. E2E live LibertAI — real model round-trips with deterministic outcome assertions.
6. TUI — headless vt100 snapshot smoke tests.
7. **Review diversity** — headless review rotates glm-5.2, glm-5.3-flash-thinking,
   deepseek-v4.1-flash-thinking via `libertai code --model <m> -p`; blockers loop to a fix
   agent before each milestone commit.

## 7. Execution mechanics

- **Workflow:** chunked-factory — explore → spec (fable forks) → implement (Opus, chunks
  ≤60 changed lines) → rotating-model review → Fable steering gate → commit per milestone;
  monitor auto-pushes to `main` periodically.
- **Verification:** `cargo fmt --check`, `cargo clippy ---D warnings`, `cargo nextest`,
  per-milestone acceptance commands — recorded in `AGENTS.md`.
- **Ops notes:** disk 43G free → shared `target/` + sccache + periodic cleanup.
  Pushes to a new private GitHub repo (`opencode-rs`).

## 8. Key research findings baked into this plan

- The session loop is re-entrant: agent loop re-reads full history each step from SQLite.
- Events double as persistence triggers and wire protocol (`EventV2`, durable
  `session.next.*` events with sequence numbers).
- Usage contract: `nonCached + cacheRead + cacheWrite = inputTokens`;
  `reasoningTokens ≤ outputTokens`. Anthropic reports natively non-cached; others inclusive.
- Cost math (V1 `getUsage`): `(input·input + output·output + cacheRead·c.read +
  cacheWrite·c.write + reasoning·output) / 1e6`, Decimal math, model pricing from models.dev.
- TUI is a stateless HTTP+SSE client (OpenTUI/Solid); sync.tsx is a single big reducer —
  maps directly to an Elm-style model in ratatui.
- The TUI consumes: `session.*`, `message.*`, `message.part.*`, `permission.*`,
  `question.*`, `todo.*`, `lsp.*`, `vcs.*`, `tui.*` events; API families enumerated in
  `packages/tui/src/context/sync.tsx` bootstrap.
- Wire surface: v1 unprefixed + v2 `/api` HTTP; SSE at `/global/event` + `/api/event`;
  PTY WebSocket `/pty/:id/connect` + `/api/pty/:id/connect` (ticket auth); HTTP Basic auth
  (`OPENCODE_SERVER_PASSWORD`); `opencode serve` prints `opencode server listening on <url>`.
- `packages/llm` is the AI-SDK-free LLM client (the V2 direction): 6 protocols, golden
  recorded tests under `test/fixtures/recordings/`.
